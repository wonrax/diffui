//! Credential prompts for the git and ssh that push and fetch spawn.
//!
//! An app started from the Dock has no terminal, so git asking for a username
//! or ssh asking for a key's passphrase would fail or hang. Both delegate the
//! question to an external program named by `GIT_ASKPASS` / `SSH_ASKPASS`;
//! [`Scope::environment`] points those at the diffui binary itself. Started
//! that way, [`run_helper`] connects back to the running app over a private
//! unix socket, hands it the prompt, and prints the answer for git to read (or
//! exits non-zero when the user cancelled).
//!
//! The app side is [`start`]: a listener thread that turns each connection
//! into an [`AskpassEvent::Prompt`], waits for [`answer`], and withdraws the
//! prompt if the helper dies first (git gave up, the op was killed).
//!
//! A [`Scope`] is one git-spawning operation. It tags every prompt that
//! operation raises, so a Cancel answers that operation's later prompts too
//! (ssh asks for a password up to three times) and the operation can tell its
//! failure was the user's doing: see [`Scope::explain`].

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use futures::channel::mpsc;

const SOCKET_ENV: &str = "DIFFUI_ASKPASS_SOCKET";
const TOKEN_ENV: &str = "DIFFUI_ASKPASS_TOKEN";
const SCOPE_ENV: &str = "DIFFUI_ASKPASS_SCOPE";

/// The error a cancelled credential prompt turns its operation's failure
/// into. Frontends match on it (it survives being wrapped in context) to
/// report a cancel rather than a failure.
pub const CANCELLED: &str = "cancelled at the credential prompt";

/// Whether `error` (a rendered error chain) is a cancelled credential prompt.
pub fn is_cancelled(error: &str) -> bool {
    error.contains(CANCELLED)
}

/// What a prompt wants back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// A password, passphrase or PIN: read masked.
    Secret,
    /// A username or other plain answer.
    Text,
    /// A yes/no question (ssh's unknown host key). The answer is the word
    /// `yes`; declining is a cancel.
    YesNo,
    /// ssh's `SSH_ASKPASS_PROMPT=confirm`: no text, the exit status is the
    /// answer.
    Confirm,
    /// ssh's `SSH_ASKPASS_PROMPT=none`: a notice ("touch your security key")
    /// shown until ssh kills the helper.
    Notice,
}

/// One question from git or ssh, read for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// The text exactly as git or ssh sent it (may span lines).
    pub text: String,
    pub kind: PromptKind,
    /// What the question is about: a URL, `user@host`, a host, a key path.
    pub subject: Option<String>,
    /// For a host-key question, the fingerprint line.
    pub detail: Option<String>,
}

impl Prompt {
    /// Classify `text`. `ssh_hint` is ssh's `SSH_ASKPASS_PROMPT`, empty when
    /// git (or an ssh too old to set it) is asking.
    pub fn parse(text: &str, ssh_hint: &str) -> Prompt {
        let text = text.trim_end().to_owned();
        let lower = text.to_lowercase();
        let quoted = first_quoted(&text);
        let mut detail = None;
        let (kind, subject) = match ssh_hint {
            "confirm" => (PromptKind::Confirm, quoted),
            "none" => (PromptKind::Notice, quoted),
            _ if lower.contains("(yes/no") => {
                detail = text
                    .lines()
                    .find(|line| line.contains("fingerprint is"))
                    .map(|line| line.trim().to_owned());
                // `'github.com (140.82.112.3)'`: the name is what the user knows.
                let host = quoted.map(|host| match host.split_once(" (") {
                    Some((name, _)) => name.to_owned(),
                    None => host,
                });
                (PromptKind::YesNo, host)
            }
            _ if lower.starts_with("username") => (PromptKind::Text, quoted),
            _ if ["password", "passphrase", "pin", "token", "secret"]
                .iter()
                .any(|word| lower.contains(word)) =>
            {
                let subject = quoted
                    .or_else(|| {
                        // `(git@host) Password:`, keyboard-interactive.
                        text.strip_prefix('(')
                            .and_then(|rest| rest.split_once(')'))
                            .map(|(subject, _)| subject.to_owned())
                    })
                    .or_else(|| {
                        // `git@host's password:`.
                        text.split_once("'s password")
                            .map(|(subject, _)| subject.trim().to_owned())
                    })
                    .or_else(|| {
                        // `Enter passphrase for /path/to/key:`.
                        text.split_once("passphrase for ")
                            .map(|(_, rest)| rest.trim_end_matches([':', ' ']).to_owned())
                    });
                (PromptKind::Secret, subject)
            }
            _ => (PromptKind::Text, quoted),
        };
        Prompt {
            text,
            kind,
            subject: subject.filter(|subject| !subject.is_empty()),
            detail,
        }
    }

    /// The prompt's question line without its subject and trailing colon:
    /// `Username for 'https://host':` → `Username`.
    pub fn label(&self) -> String {
        let line = self
            .text
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        // The question without its subject: `Username for 'https://h':` →
        // `Username`, `git@h's password:` → `password`.
        let found = self
            .subject
            .as_deref()
            .and_then(|subject| line.find(subject).map(|at| (at, at + subject.len())));
        let line = match found {
            Some((start, end)) => {
                let head = line[..start].trim_end_matches(['\'', '(', ' ']);
                let head = head.strip_suffix(" for").unwrap_or(head);
                if head.is_empty() {
                    let tail = line[end..].trim_start_matches(['\'', ')', ' ']);
                    tail.strip_prefix("s ").unwrap_or(tail)
                } else {
                    head
                }
            }
            None => line,
        };
        let line = line.trim_end_matches([':', ' ', '?']);
        let line = line.strip_prefix("Enter ").unwrap_or(line);
        let mut chars = line.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => String::new(),
        }
    }
}

/// The first `'…'`-quoted span of `text`.
fn first_quoted(text: &str) -> Option<String> {
    let (_, rest) = text.split_once('\'')?;
    let (quoted, _) = rest.split_once('\'')?;
    Some(quoted.to_owned())
}

/// A prompt waiting on the user.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    /// Pass back to [`answer`].
    pub id: u64,
    pub prompt: Prompt,
}

#[derive(Debug, Clone)]
pub enum AskpassEvent {
    Prompt(PromptRequest),
    /// The asking process went away before an answer: drop the prompt.
    Withdrawn(u64),
}

/// Answer prompt `id`: `Some` sends the text (empty for a yes or a notice),
/// `None` cancels it and every later prompt of the same operation.
pub fn answer(id: u64, reply: Option<String>) {
    let Some(server) = SERVER.get() else {
        return;
    };
    let pending = server.pending.lock().unwrap().remove(&id);
    if let Some(pending) = pending {
        if reply.is_none() {
            server.cancelled.lock().unwrap().insert(pending.scope);
        }
        let _ = pending.reply.send(reply);
    }
}

/// The prompts, for the frontend to show. `None` once taken, or when the
/// server isn't running.
pub fn take_events() -> Option<mpsc::UnboundedReceiver<AskpassEvent>> {
    EVENTS.lock().unwrap().take()
}

/// One operation that may spawn git or ssh. Hand [`Self::environment`] to the
/// subprocess and run its failure through [`Self::explain`].
pub struct Scope {
    id: u64,
}

impl Scope {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Scope {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Scope {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The variables that route git's and ssh's prompts to the app. Empty
    /// when the server isn't running, which leaves git as it was. A
    /// `GIT_ASKPASS` / `SSH_ASKPASS` the user set themselves is left alone.
    pub fn environment(&self) -> HashMap<OsString, OsString> {
        let mut env = HashMap::new();
        let Some(server) = SERVER.get() else {
            return env;
        };
        let mut set = |key: &str, value: OsString| {
            env.insert(OsString::from(key), value);
        };
        if std::env::var_os("GIT_ASKPASS").is_none() {
            set("GIT_ASKPASS", server.exe.clone().into());
        }
        if std::env::var_os("SSH_ASKPASS").is_none() {
            set("SSH_ASKPASS", server.exe.clone().into());
            set("SSH_ASKPASS_REQUIRE", "force".into());
        }
        set("GIT_TERMINAL_PROMPT", "0".into());
        set(SOCKET_ENV, server.socket.clone().into());
        set(TOKEN_ENV, server.token.clone().into());
        set(SCOPE_ENV, self.id.to_string().into());
        env
    }

    /// Whether the user cancelled a prompt this operation raised.
    pub fn cancelled(&self) -> bool {
        SERVER
            .get()
            .is_some_and(|server| server.cancelled.lock().unwrap().contains(&self.id))
    }

    /// The error to report for this operation's failure: [`CANCELLED`] when
    /// the user cancelled a prompt (git's own message would only say the
    /// credentials couldn't be read), else `error` unchanged.
    pub fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        if self.cancelled() {
            anyhow::anyhow!(CANCELLED)
        } else {
            error
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(server) = SERVER.get() {
            server.cancelled.lock().unwrap().remove(&self.id);
        }
    }
}

struct Pending {
    scope: u64,
    reply: std::sync::mpsc::Sender<Option<String>>,
}

struct Server {
    exe: PathBuf,
    socket: PathBuf,
    token: String,
    events: mpsc::UnboundedSender<AskpassEvent>,
    pending: Mutex<HashMap<u64, Pending>>,
    cancelled: Mutex<HashSet<u64>>,
    next_id: AtomicU64,
}

static SERVER: OnceLock<Server> = OnceLock::new();
static EVENTS: Mutex<Option<mpsc::UnboundedReceiver<AskpassEvent>>> = Mutex::new(None);

/// Start listening for prompts. Call once at startup; a failure leaves git
/// prompting the way it did before (which from the Dock means failing).
pub fn start() -> std::io::Result<()> {
    imp::start()
}

/// The helper side. When this process was started by git or ssh as the
/// askpass program, relay the prompt and return the exit code; `None` for a
/// normal launch.
pub fn run_helper() -> Option<i32> {
    let socket = std::env::var_os(SOCKET_ENV)?;
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let scope = std::env::var(SCOPE_ENV).unwrap_or_default();
    let hint = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    let prompt = std::env::args().nth(1).unwrap_or_default();
    match imp::ask(&PathBuf::from(socket), &token, &scope, &hint, &prompt) {
        Ok(Some(reply)) => {
            println!("{reply}");
            Some(0)
        }
        Ok(None) => Some(1),
        Err(error) => {
            eprintln!("diffui askpass: {error}");
            Some(1)
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::{
        io::{self, BufRead, BufReader, Read, Write},
        os::unix::{
            fs::{DirBuilderExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::Path,
        sync::atomic::Ordering,
        time::Duration,
    };

    use super::*;

    /// How long a connecting helper has to say what it wants.
    const READ_TIMEOUT: Duration = Duration::from_secs(3);
    /// How often a waiting prompt checks its helper is still there.
    const LIVENESS_POLL: Duration = Duration::from_millis(250);

    pub(super) fn start() -> io::Result<()> {
        if SERVER.get().is_some() {
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        // A private directory, so nothing else on the machine can reach the
        // socket; the token additionally ties a connection to an environment
        // this process handed out.
        let dir = std::env::temp_dir().join(format!(
            "diffui-askpass-{}-{:x}",
            std::process::id(),
            random_u64()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let socket = dir.join("s");
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let (events, receiver) = mpsc::unbounded();
        let server = Server {
            exe,
            socket,
            token: format!("{:016x}{:016x}", random_u64(), random_u64()),
            events,
            pending: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashSet::new()),
            next_id: AtomicU64::new(1),
        };
        if SERVER.set(server).is_err() {
            return Ok(());
        }
        *EVENTS.lock().unwrap() = Some(receiver);
        std::thread::Builder::new()
            .name("askpass".to_owned())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let _ = std::thread::Builder::new()
                        .name("askpass-prompt".to_owned())
                        .spawn(move || {
                            if let Err(error) = serve(stream) {
                                tracing::warn!("askpass connection failed: {error}");
                            }
                        });
                }
            })?;
        Ok(())
    }

    /// 64 bits from the standard library's per-process random hasher keys.
    fn random_u64() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(std::process::id() as u64);
        hasher.finish()
    }

    /// One helper connection: `token`, `scope`, `hint` and the prompt's byte
    /// length on a line each, then the prompt. The reply is `OK` and the
    /// answer, or `NO`.
    fn serve(stream: UnixStream) -> io::Result<()> {
        let server = SERVER.get().expect("askpass server is running");
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        let mut reader = BufReader::new(&stream);
        let mut line = || -> io::Result<String> {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            Ok(line.trim_end_matches('\n').to_owned())
        };
        let token = line()?;
        if token != server.token {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "bad token"));
        }
        let scope: u64 = line()?.parse().unwrap_or(0);
        let hint = line()?;
        let length: usize = line()?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad length"))?;
        let mut text = vec![0; length.min(64 * 1024)];
        reader.read_exact(&mut text)?;
        let text = String::from_utf8_lossy(&text).into_owned();
        let mut writer = &stream;

        if scope != 0 && server.cancelled.lock().unwrap().contains(&scope) {
            return writer.write_all(b"NO\n");
        }

        let id = server.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = std::sync::mpsc::channel();
        server
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending { scope, reply });
        let prompt = Prompt::parse(&text, &hint);
        tracing::info!(kind = ?prompt.kind, subject = ?prompt.subject, "credential prompt");
        let _ = server
            .events
            .unbounded_send(AskpassEvent::Prompt(PromptRequest { id, prompt }));

        let reply = loop {
            match answer.recv_timeout(LIVENESS_POLL) {
                Ok(reply) => break reply,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if alive(&stream) => {}
                Err(_) => {
                    server.pending.lock().unwrap().remove(&id);
                    let _ = server.events.unbounded_send(AskpassEvent::Withdrawn(id));
                    return Ok(());
                }
            }
        };
        match reply {
            Some(answer) => {
                writer.write_all(b"OK\n")?;
                writer.write_all(answer.as_bytes())?;
            }
            None => writer.write_all(b"NO\n")?,
        }
        writer.flush()
    }

    /// Whether the helper on the other end is still connected. It never
    /// writes after the prompt, so a readable socket means it hung up.
    fn alive(stream: &UnixStream) -> bool {
        if stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut byte = [0u8; 1];
        let mut reader = stream;
        let alive = matches!(
            reader.read(&mut byte),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        );
        let _ = stream.set_nonblocking(false);
        alive
    }

    pub(super) fn ask(
        socket: &Path,
        token: &str,
        scope: &str,
        hint: &str,
        prompt: &str,
    ) -> io::Result<Option<String>> {
        let mut stream = UnixStream::connect(socket)?;
        write!(
            stream,
            "{token}\n{scope}\n{hint}\n{}\n{prompt}",
            prompt.len()
        )?;
        stream.flush()?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        Ok(reply
            .strip_prefix("OK\n")
            .map(|answer| answer.trim_end_matches('\n').to_owned()))
    }
}

#[cfg(not(unix))]
mod imp {
    use std::{io, path::Path};

    pub(super) fn start() -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "credential prompts need unix sockets",
        ))
    }

    pub(super) fn ask(
        _socket: &Path,
        _token: &str,
        _scope: &str,
        _hint: &str,
        _prompt: &str,
    ) -> io::Result<Option<String>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "credential prompts need unix sockets",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_username_and_password() {
        let user = Prompt::parse("Username for 'https://example.com': ", "");
        assert_eq!(user.kind, PromptKind::Text);
        assert_eq!(user.subject.as_deref(), Some("https://example.com"));
        assert_eq!(user.label(), "Username");

        let pass = Prompt::parse("Password for 'https://me@example.com': ", "");
        assert_eq!(pass.kind, PromptKind::Secret);
        assert_eq!(pass.subject.as_deref(), Some("https://me@example.com"));
        assert_eq!(pass.label(), "Password");
    }

    #[test]
    fn ssh_passphrases_and_passwords() {
        let key = Prompt::parse("Enter passphrase for key '/Users/me/.ssh/id_ed25519': ", "");
        assert_eq!(key.kind, PromptKind::Secret);
        assert_eq!(key.subject.as_deref(), Some("/Users/me/.ssh/id_ed25519"));
        assert_eq!(key.label(), "Passphrase for key");

        let bare = Prompt::parse("Enter passphrase for /Users/me/.ssh/id_rsa:", "");
        assert_eq!(bare.subject.as_deref(), Some("/Users/me/.ssh/id_rsa"));

        let host = Prompt::parse("git@example.com's password: ", "");
        assert_eq!(host.kind, PromptKind::Secret);
        assert_eq!(host.subject.as_deref(), Some("git@example.com"));
        assert_eq!(host.label(), "Password");

        let interactive = Prompt::parse("(git@example.com) Password: ", "");
        assert_eq!(interactive.kind, PromptKind::Secret);
        assert_eq!(interactive.subject.as_deref(), Some("git@example.com"));
        assert_eq!(interactive.label(), "Password");
    }

    #[test]
    fn host_key_question() {
        let prompt = Prompt::parse(
            "The authenticity of host 'github.com (140.82.112.3)' can't be established.\n\
             ED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.\n\
             This key is not known by any other names.\n\
             Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "",
        );
        assert_eq!(prompt.kind, PromptKind::YesNo);
        assert_eq!(prompt.subject.as_deref(), Some("github.com"));
        assert_eq!(
            prompt.detail.as_deref(),
            Some("ED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.")
        );
    }

    #[test]
    fn ssh_hints_win() {
        assert_eq!(
            Prompt::parse("Allow use of key /k?", "confirm").kind,
            PromptKind::Confirm
        );
        assert_eq!(
            Prompt::parse("Confirm user presence for key ED25519-SK", "none").kind,
            PromptKind::Notice
        );
    }

    #[test]
    fn cancel_is_recognised_through_context() {
        let error = anyhow::anyhow!(CANCELLED).context("failed to push to origin");
        assert!(is_cancelled(&format!("{error:#}")));
        assert!(!is_cancelled("failed to push to origin: exit 128"));
    }

    #[cfg(unix)]
    #[test]
    fn round_trip_through_the_socket() {
        start().unwrap();
        let mut events = take_events().unwrap();
        let server = SERVER.get().unwrap();
        let scope = Scope::new();
        let env = scope.environment();
        assert_eq!(
            env.get(&OsString::from("GIT_TERMINAL_PROMPT")),
            Some(&OsString::from("0"))
        );
        let socket = server.socket.clone();
        let token = server.token.clone();
        let scope_id = scope.id.to_string();

        // Answered.
        let asker = {
            let (socket, token, scope_id) = (socket.clone(), token.clone(), scope_id.clone());
            std::thread::spawn(move || {
                imp::ask(&socket, &token, &scope_id, "", "Username for 'https://h': ")
            })
        };
        let AskpassEvent::Prompt(request) =
            futures::executor::block_on(futures::StreamExt::next(&mut events)).unwrap()
        else {
            panic!("expected a prompt");
        };
        assert_eq!(request.prompt.subject.as_deref(), Some("https://h"));
        answer(request.id, Some("me".to_owned()));
        assert_eq!(asker.join().unwrap().unwrap().as_deref(), Some("me"));
        assert!(!scope.cancelled());

        // Cancelled: the scope remembers, and its next prompt is refused
        // without reaching the user.
        let asker = {
            let (socket, token, scope_id) = (socket.clone(), token.clone(), scope_id.clone());
            std::thread::spawn(move || imp::ask(&socket, &token, &scope_id, "", "Password: "))
        };
        let AskpassEvent::Prompt(request) =
            futures::executor::block_on(futures::StreamExt::next(&mut events)).unwrap()
        else {
            panic!("expected a prompt");
        };
        answer(request.id, None);
        assert_eq!(asker.join().unwrap().unwrap(), None);
        assert!(scope.cancelled());
        assert_eq!(
            imp::ask(&socket, &token, &scope_id, "", "Password: ").unwrap(),
            None
        );
        assert!(is_cancelled(
            &scope.explain(anyhow::anyhow!("exit 128")).to_string()
        ));

        // A wrong token is turned away.
        assert!(
            imp::ask(&socket, "nope", &scope_id, "", "Password: ")
                .map(|reply| reply.is_none())
                .unwrap_or(true)
        );
    }
}
