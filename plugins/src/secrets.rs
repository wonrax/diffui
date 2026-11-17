//! Where diffui keeps what a plugin's `secret` settings hold (a forge's
//! token): the macOS Keychain, or a file that stands in for it.
//!
//! An entry is a generic password under the service `diffui`, its account
//! `<plugin id>@<host>` for a per-host secret (a token) and
//! `<plugin id>.<key>` or `<plugin id>.<key>@<host>` for any other. Scripts
//! never write one; the app and `diffui plugin auth` do, and a script reads
//! through `auth.token` / `auth.secret`.
//!
//! [`store`] picks the Keychain only for the user's own home: with
//! `DIFFUI_SECRET_STORE=file:<path>` set, or with `HOME` anywhere but the
//! account's home directory (every test and scratch run), it is a file.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The Keychain service every entry is under.
pub const SERVICE: &str = "diffui";

/// The environment variable that points the store at a file:
/// `DIFFUI_SECRET_STORE=file:/path/to/secrets.json`.
pub const STORE_ENV: &str = "DIFFUI_SECRET_STORE";

/// A place secrets are kept, by account.
pub trait SecretStore: Send + Sync + fmt::Debug {
    /// The secret of `account`, or `None` when there is none.
    fn get(&self, account: &str) -> Result<Option<String>, String>;
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;
    /// Remove `account`'s secret; removing one that isn't there is fine.
    fn delete(&self, account: &str) -> Result<(), String>;
    /// Every account that has a secret, in any order.
    fn accounts(&self) -> Result<Vec<String>, String>;
    /// Whether this is the user's real store. Anything else (a file under a
    /// scratch home) also keeps `auth.token` away from git's credential
    /// helpers, which could reach the real Keychain on their own.
    fn is_real(&self) -> bool;
}

/// The account a per-host secret `key` of `plugin` is kept under: the
/// token, `key` = `token`, is `<plugin>@<host>`.
pub fn account(plugin: &str, key: &str, host: Option<&str>) -> String {
    match (key, host) {
        ("token", Some(host)) => format!("{plugin}@{host}"),
        (key, Some(host)) => format!("{plugin}.{key}@{host}"),
        (key, None) => format!("{plugin}.{key}"),
    }
}

/// What `plugin` has a token for, from the store's accounts: each a host,
/// or `host/name` for another account on it (see [`scope`]).
pub fn token_scopes(store: &dyn SecretStore, plugin: &str) -> Vec<String> {
    let prefix = format!("{plugin}@");
    let mut scopes: Vec<String> = store
        .accounts()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|account| account.strip_prefix(&prefix).map(str::to_string))
        .collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

/// The hosts `plugin` has a token for, whether the host's own or another
/// account's.
pub fn token_hosts(store: &dyn SecretStore, plugin: &str) -> Vec<String> {
    let mut hosts: Vec<String> = token_scopes(store, plugin)
        .iter()
        .map(|scope| split_scope(scope).0.to_owned())
        .collect();
    hosts.dedup();
    hosts
}

/// The other accounts `plugin` has a token for on `host`, by the names the
/// user gave them.
pub fn token_accounts(store: &dyn SecretStore, plugin: &str, host: &str) -> Vec<String> {
    token_scopes(store, plugin)
        .iter()
        .filter_map(|scope| match split_scope(scope) {
            (on, Some(name)) if on.eq_ignore_ascii_case(host) => Some(name.to_owned()),
            _ => None,
        })
        .collect()
}

/// Whose token it is, as its account in the store names it: `host` for the
/// host's own sign-in, `host/name` for another account there. A name is
/// whatever the user calls it ("work"), kept in lower case. It says nothing
/// about which repositories the account can see: the forge finds that out.
pub fn scope(host: &str, name: Option<&str>) -> String {
    match name.and_then(|name| name.split('/').find(|part| !part.trim().is_empty())) {
        Some(name) => format!("{host}/{}", name.trim().to_ascii_lowercase()),
        None => host.to_owned(),
    }
}

/// A [`scope`] back as its host and the other account's name.
pub fn split_scope(scope: &str) -> (&str, Option<&str>) {
    match scope.split_once('/') {
        Some((host, name)) if !name.is_empty() => (host, Some(name)),
        Some((host, _)) => (host, None),
        None => (scope, None),
    }
}

/// The store this process uses: see the module docs for which.
pub fn store() -> Arc<dyn SecretStore> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Arc::from(choose(
        std::env::var(STORE_ENV).ok().as_deref(),
        home.as_deref(),
        account_home().as_deref(),
    ))
}

/// The pick, from the env var, `$HOME` and the account's home directory.
fn choose(env: Option<&str>, home: Option<&Path>, own: Option<&Path>) -> Box<dyn SecretStore> {
    if let Some(path) = env.and_then(|value| value.strip_prefix("file:")) {
        return Box::new(FileStore::new(PathBuf::from(path)));
    }
    let scratch = |home: &Path| home.join(".diffui-secrets.json");
    match (home, own) {
        (Some(home), Some(own)) if same_dir(home, own) => keychain(home),
        (Some(home), _) => Box::new(FileStore::new(scratch(home))),
        (None, _) => Box::new(FileStore::new(
            std::env::temp_dir().join("diffui-secrets.json"),
        )),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(a) == canonical(b)
}

#[cfg(all(target_os = "macos", not(test)))]
fn keychain(_home: &Path) -> Box<dyn SecretStore> {
    Box::new(Keychain)
}

/// This crate's own tests never get the Keychain, whatever `HOME` says.
#[cfg(any(not(target_os = "macos"), test))]
fn keychain(home: &Path) -> Box<dyn SecretStore> {
    Box::new(FileStore::new(home.join(".diffui-secrets.json")))
}

/// The account's home directory, as the user database has it (not `$HOME`).
#[cfg(unix)]
fn account_home() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: getpwuid_r writes into `entry` and `buffer`, both ours and
    // sized as passed; `found` points at `entry` or is null.
    unsafe {
        let mut entry: libc::passwd = std::mem::zeroed();
        let mut buffer = vec![0 as libc::c_char; 4096];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwuid_r(
            libc::getuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        );
        if status != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        let dir = CStr::from_ptr(entry.pw_dir);
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
    }
}

#[cfg(not(unix))]
fn account_home() -> Option<PathBuf> {
    None
}

/// Secrets in a JSON file, readable by the user alone: the stand-in for the
/// Keychain in tests and scratch runs.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    /// One read-modify-write at a time within the process.
    lock: Mutex<()>,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<BTreeMap<String, String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| format!("{}: {error}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(format!("{}: {error}", self.path.display())),
        }
    }

    fn write(&self, secrets: &BTreeMap<String, String>) -> Result<(), String> {
        use std::io::Write;
        let fail = |error: std::io::Error| format!("{}: {error}", self.path.display());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let text = serde_json::to_string_pretty(secrets).map_err(|error| error.to_string())?;
        let partial = self.path.with_extension("partial");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&partial).map_err(fail)?;
        file.write_all(text.as_bytes()).map_err(fail)?;
        std::fs::rename(&partial, &self.path).map_err(fail)
    }
}

impl SecretStore for FileStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        let _held = crate::actor::lock(&self.lock);
        Ok(self.read()?.remove(account))
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        let _held = crate::actor::lock(&self.lock);
        let mut secrets = self.read()?;
        secrets.insert(account.to_string(), secret.to_string());
        self.write(&secrets)
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        let _held = crate::actor::lock(&self.lock);
        let mut secrets = self.read()?;
        if secrets.remove(account).is_some() {
            self.write(&secrets)?;
        }
        Ok(())
    }

    fn accounts(&self) -> Result<Vec<String>, String> {
        let _held = crate::actor::lock(&self.lock);
        Ok(self.read()?.into_keys().collect())
    }

    fn is_real(&self) -> bool {
        false
    }
}

/// The login Keychain, through Security.framework.
#[cfg(all(target_os = "macos", not(test)))]
#[derive(Debug)]
struct Keychain;

#[cfg(all(target_os = "macos", not(test)))]
impl SecretStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(SERVICE, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| format!("the Keychain entry {account} isn't text")),
            Err(error) if error.code() == ERR_ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(format!("Keychain: {error}")),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        security_framework::passwords::set_generic_password(SERVICE, account, secret.as_bytes())
            .map_err(|error| format!("Keychain: {error}"))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(SERVICE, account) {
            Err(error) if error.code() != ERR_ITEM_NOT_FOUND => Err(format!("Keychain: {error}")),
            _ => Ok(()),
        }
    }

    fn accounts(&self) -> Result<Vec<String>, String> {
        use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
        // Attributes only: listing names doesn't read a secret, so it never
        // asks the user.
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(SERVICE)
            .load_attributes(true)
            .limit(Limit::All)
            .search();
        match found {
            Ok(results) => Ok(results
                .iter()
                .filter_map(|result| result.simplify_dict()?.remove("acct"))
                .collect()),
            Err(error) if error.code() == ERR_ITEM_NOT_FOUND => Ok(Vec::new()),
            Err(error) => Err(format!("Keychain: {error}")),
        }
    }

    fn is_real(&self) -> bool {
        true
    }
}

/// `errSecItemNotFound`.
#[cfg(all(target_os = "macos", not(test)))]
const ERR_ITEM_NOT_FOUND: i32 = -25300;

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("diffui-secrets-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_file_store_saves_reads_and_signs_out() {
        let dir = scratch("file");
        let store = FileStore::new(dir.join("nested/secrets.json"));
        assert_eq!(store.get("github@github.com").unwrap(), None);
        store.set("github@github.com", "tok-1").unwrap();
        store.set("github@github.acme.com", "tok-2").unwrap();
        store.set("gitlab@gitlab.com", "tok-3").unwrap();
        assert_eq!(
            store.get("github@github.com").unwrap().as_deref(),
            Some("tok-1")
        );
        assert_eq!(
            token_hosts(&store, "github"),
            ["github.acme.com", "github.com"]
        );
        store.delete("github@github.com").unwrap();
        store.delete("github@github.com").unwrap();
        assert_eq!(store.get("github@github.com").unwrap(), None);
        assert_eq!(token_hosts(&store, "github"), ["github.acme.com"]);

        // Another account's token is its host's too, under its name.
        store
            .set(
                &account(
                    "github",
                    "token",
                    Some(&scope("github.com", Some(" Work "))),
                ),
                "t",
            )
            .unwrap();
        assert_eq!(
            token_scopes(&store, "github"),
            ["github.acme.com", "github.com/work"]
        );
        assert_eq!(
            token_hosts(&store, "github"),
            ["github.acme.com", "github.com"]
        );
        assert_eq!(token_accounts(&store, "github", "GitHub.com"), ["work"]);
        assert!(token_accounts(&store, "github", "github.acme.com").is_empty());
        assert_eq!(split_scope("github.com/work"), ("github.com", Some("work")));
        assert_eq!(split_scope("github.com"), ("github.com", None));
        assert_eq!(scope("github.com", Some("")), "github.com");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "only the user reads it");
        }
        assert!(!store.is_real());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accounts_name_the_plugin_the_key_and_the_host() {
        assert_eq!(
            account("github", "token", Some("github.com")),
            "github@github.com"
        );
        assert_eq!(account("x", "key", Some("h.test")), "x.key@h.test");
        assert_eq!(account("x", "key", None), "x.key");
    }

    /// The Keychain is only for the account's own home with nothing set;
    /// a scratch `HOME` or the env var is a file, and this crate's tests
    /// never get it at all.
    #[test]
    fn only_the_users_own_home_gets_the_keychain() {
        let own = Path::new("/Users/someone");
        let pick = |env: Option<&str>, home: Option<&Path>| choose(env, home, Some(own));
        let file = pick(Some("file:/tmp/x.json"), Some(own));
        assert!(!file.is_real());
        assert!(format!("{file:?}").contains("/tmp/x.json"), "{file:?}");
        let scratch = pick(None, Some(Path::new("/tmp/fake-home")));
        assert!(!scratch.is_real());
        assert!(format!("{scratch:?}").contains("/tmp/fake-home/.diffui-secrets.json"));
        assert!(
            !pick(None, Some(own)).is_real(),
            "a test build has no Keychain"
        );
        assert!(!store().is_real());
    }
}
