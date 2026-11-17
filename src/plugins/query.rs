//! A plugin's reads of a jj repository, through the jj CLI diffui embeds.
//!
//! Every read is one CLI run with `--ignore-working-copy --at-op <op>`, where
//! `<op>` is the operation the plugin's handle was made at: it never
//! snapshots, never touches the repository actor, and sees the repository as
//! the user saw it when the plugin was called, whatever happened since.
//!
//! `repo.jj.query` takes a jj command line from the script, so it is held to
//! a list of commands that only read ([`READ_ONLY`]) and refused the options
//! that would point a read somewhere else or start a program
//! ([`REFUSED_OPTIONS`]).

use diffui_plugins::protocol::{Diff, DiffFormat, Row};
use serde::Deserialize;

/// The jj commands `repo.jj.query` runs: each only reads. A script's line
/// must begin with one of them.
pub(crate) const READ_ONLY: &[&[&str]] = &[
    &["log"],
    &["show"],
    &["diff"],
    &["interdiff"],
    &["evolog"],
    &["status"],
    &["file", "show"],
    &["file", "list"],
    &["file", "annotate"],
    &["bookmark", "list"],
    &["tag", "list"],
    &["op", "log"],
    &["op", "show"],
    &["config", "get"],
    &["git", "remote", "list"],
    &["workspace", "list"],
];

/// Options a script's line may not carry. The first four are the host's to
/// set; `--tool` and the `--config` family can name a program for jj to run
/// (a diff tool, a formatter), which would be `exec` without its permission.
const REFUSED_OPTIONS: &[&str] = &[
    "--repository",
    "--at-operation",
    "--at-op",
    "--ignore-working-copy",
    "--tool",
    "--config",
    "--config-file",
    "--config-toml",
];

/// Where a read runs: a workspace, at one operation.
#[derive(Debug, Clone)]
pub(crate) struct At {
    pub(crate) root: std::path::PathBuf,
    pub(crate) op: String,
}

impl At {
    /// The options every read runs with, before the command: this workspace,
    /// this operation, no snapshot, plain output. jj takes each of them once,
    /// so a line that names another repository or operation fails.
    fn pinned(&self) -> Vec<String> {
        [
            "--ignore-working-copy",
            "--at-op",
            &self.op,
            "-R",
            &self.root.display().to_string(),
            "--color=never",
            "--no-pager",
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

/// The jj arguments that run a script's `args` at `at`, or why they are
/// refused.
pub(crate) fn query_line(at: &At, args: &[String]) -> Result<Vec<String>, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if !READ_ONLY.iter().any(|command| words.starts_with(command)) {
        let allowed: Vec<String> = READ_ONLY.iter().map(|command| command.join(" ")).collect();
        return Err(format!(
            "repo.jj.query runs read-only commands only, and the line starts with the command: \
             {}. Use repo.jj.run to change the repository",
            allowed.join(", ")
        ));
    }
    // Past `--` everything is a path.
    for word in words.iter().take_while(|word| **word != "--") {
        let name = word.split('=').next().unwrap_or(word);
        if REFUSED_OPTIONS.contains(&name) || (word.starts_with("-R") && !word.starts_with("--")) {
            return Err(format!("repo.jj.query doesn't take {name}"));
        }
    }
    let mut line = at.pinned();
    line.extend(args.iter().cloned());
    Ok(line)
}

/// One row of [`rows_line`]'s output.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRow {
    change_id: String,
    commit_id: String,
    description: String,
    author: String,
    /// Seconds since the epoch, as jj formats `%s`.
    timestamp: String,
    bookmarks: Vec<String>,
    working_copy: bool,
    immutable: bool,
    empty: bool,
    conflict: bool,
}

/// A JSON object per revision, a line each. Written with `escape_json` rather
/// than `json()` so it reads the same on every jj that has templates.
const ROW_TEMPLATE: &str = concat!(
    r#""{\"changeId\":" ++ stringify(change_id).escape_json()"#,
    r#" ++ ",\"commitId\":" ++ stringify(commit_id).escape_json()"#,
    r#" ++ ",\"description\":" ++ description.first_line().escape_json()"#,
    r#" ++ ",\"author\":" ++ stringify(author.name()).escape_json()"#,
    r#" ++ ",\"timestamp\":" ++ stringify(author.timestamp().format("%s")).escape_json()"#,
    r#" ++ ",\"bookmarks\":[" ++ local_bookmarks.map(|b| stringify(b.name()).escape_json()).join(",") ++ "]""#,
    r#" ++ ",\"workingCopy\":" ++ if(current_working_copy, "true", "false")"#,
    r#" ++ ",\"immutable\":" ++ if(immutable, "true", "false")"#,
    r#" ++ ",\"empty\":" ++ if(empty, "true", "false")"#,
    r#" ++ ",\"conflict\":" ++ if(conflict, "true", "false")"#,
    r#" ++ "}\n""#,
);

/// The jj arguments for `repo.rows`.
pub(crate) fn rows_line(at: &At, revset: &str, limit: u32) -> Vec<String> {
    let mut line = at.pinned();
    // The revset rides in its option's own word, so one that starts with a
    // dash is still a revset.
    line.extend(
        [
            "log",
            "--no-graph",
            &format!("--revisions={revset}"),
            "-n",
            &limit.to_string(),
            "-T",
            ROW_TEMPLATE,
        ]
        .map(str::to_owned),
    );
    line
}

/// What [`rows_line`] printed, as rows.
pub(crate) fn parse_rows(stdout: &str) -> Result<Vec<Row>, String> {
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let raw: RawRow = serde_json::from_str(line)
                .map_err(|error| format!("jj printed a row that doesn't read: {error}"))?;
            Ok(Row {
                change_id: raw.change_id,
                commit_id: raw.commit_id,
                description: raw.description,
                author: raw.author,
                timestamp: raw.timestamp.parse::<i64>().ok().map(|s| s * 1000),
                bookmarks: raw.bookmarks,
                working_copy: raw.working_copy,
                immutable: raw.immutable,
                empty: raw.empty,
                conflict: raw.conflict,
            })
        })
        .collect()
}

/// The jj arguments for `repo.diff`.
pub(crate) fn diff_line(at: &At, revset: &str, format: DiffFormat) -> Vec<String> {
    let format = match format {
        DiffFormat::Git => "--git",
        DiffFormat::Stat => "--stat",
        DiffFormat::Summary => "--summary",
        DiffFormat::NameOnly => "--name-only",
    };
    let mut line = at.pinned();
    line.extend(["diff", format, &format!("--revisions={revset}")].map(str::to_owned));
    line
}

/// `text` cut at `max_bytes`, on a character boundary.
pub(crate) fn cut_diff(mut text: String, max_bytes: u64) -> Diff {
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let truncated = text.len() > max;
    if truncated {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Diff { text, truncated }
}

/// The jj arguments for `repo.readFile`: `path` is relative to the workspace
/// root, whatever it holds.
pub(crate) fn file_line(at: &At, rev: &str, path: &str) -> Vec<String> {
    let quoted = path.replace('\\', "\\\\").replace('"', "\\\"");
    let mut line = at.pinned();
    line.extend(
        [
            "file",
            "show",
            &format!("--revision={rev}"),
            "--",
            &format!("root-file:\"{quoted}\""),
        ]
        .map(str::to_owned),
    );
    line
}

/// Whether a failed `jj file show` said the revision has no such file.
pub(crate) fn is_missing_file(stderr: &str) -> bool {
    stderr.contains("No such path")
}

/// How one jj run ended.
pub(crate) struct Ran {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Run the embedded jj CLI with `line`, in the workspace.
pub(crate) async fn run(at: At, line: Vec<String>) -> Result<Ran, String> {
    let mut command = crate::user_commands::embedded_jj();
    let output = command
        .args(&line)
        .current_dir(&at.root)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|error| format!("Couldn't run jj: {error}"))?;
    Ok(Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|word| (*word).to_owned()).collect()
    }

    fn at() -> At {
        At {
            root: "/repo".into(),
            op: "abc123".to_owned(),
        }
    }

    #[test]
    fn a_query_is_pinned_to_its_operation_and_never_snapshots() {
        let line = query_line(&at(), &words(&["log", "-r", "@", "--no-graph"])).unwrap();
        assert_eq!(
            line,
            words(&[
                "--ignore-working-copy",
                "--at-op",
                "abc123",
                "-R",
                "/repo",
                "--color=never",
                "--no-pager",
                "log",
                "-r",
                "@",
                "--no-graph",
            ])
        );
    }

    #[test]
    fn every_listed_command_is_let_through() {
        for command in READ_ONLY {
            assert!(query_line(&at(), &words(command)).is_ok(), "{command:?}");
        }
    }

    #[test]
    fn a_command_that_writes_is_refused() {
        for line in [
            &["new"][..],
            &["describe", "-m", "x"],
            &["bookmark", "set", "main"],
            &["file", "track", "x"],
            &["op", "restore", "abc"],
            &["config", "set", "--user", "a", "b"],
            &["git", "push"],
            &["git", "remote", "add", "x", "y"],
            &["undo"],
            // The command leads: an option in front could hide another one.
            &["--no-pager", "log"],
            &[],
        ] {
            let refused = query_line(&at(), &words(line)).unwrap_err();
            assert!(refused.contains("read-only commands only"), "{line:?}");
        }
    }

    #[test]
    fn options_that_repoint_a_read_or_start_a_program_are_refused() {
        for (line, option) in [
            (&["log", "--at-op", "@-"][..], "--at-op"),
            (&["log", "--at-operation=@-"], "--at-operation"),
            (&["log", "-R", "/elsewhere"], "-R"),
            (&["log", "-R/elsewhere"], "-R/elsewhere"),
            (&["log", "--repository", "/elsewhere"], "--repository"),
            (&["diff", "--tool", "sh"], "--tool"),
            (&["diff", "--tool=sh"], "--tool"),
            (&["show", "--config", "ui.diff-formatter=sh"], "--config"),
            (&["show", "--config-file=/tmp/x.toml"], "--config-file"),
            (&["log", "--config-toml", "x=1"], "--config-toml"),
        ] {
            let refused = query_line(&at(), &words(line)).unwrap_err();
            assert_eq!(refused, format!("repo.jj.query doesn't take {option}"));
        }
        // A path that looks like one of them is a path.
        assert!(query_line(&at(), &words(&["file", "show", "--", "--tool"])).is_ok());
    }

    #[test]
    fn rows_read_back_from_the_template() {
        let stdout = concat!(
            r#"{"changeId":"kxyz","commitId":"12ab","description":"say \"hi\"","author":"Ann","#,
            r#""timestamp":"1700000000","bookmarks":["main","wip"],"workingCopy":true,"#,
            r#""immutable":false,"empty":false,"conflict":false}"#,
            "\n",
        );
        let rows = parse_rows(stdout).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].description, "say \"hi\"");
        assert_eq!(rows[0].timestamp, Some(1_700_000_000_000));
        assert_eq!(rows[0].bookmarks, ["main", "wip"]);
        assert!(rows[0].working_copy && !rows[0].immutable);
        assert!(parse_rows("not json\n").is_err());
    }

    #[test]
    fn a_diff_is_cut_on_a_character() {
        let whole = cut_diff("héllo".to_owned(), 100);
        assert_eq!((whole.text.as_str(), whole.truncated), ("héllo", false));
        // Byte 2 is inside the two-byte é.
        let cut = cut_diff("héllo".to_owned(), 2);
        assert_eq!((cut.text.as_str(), cut.truncated), ("h", true));
    }

    #[test]
    fn a_file_is_named_from_the_root_whatever_its_name() {
        let line = file_line(&at(), "@-", "docs/a \"b\".md");
        assert_eq!(line[line.len() - 1], "root-file:\"docs/a \\\"b\\\".md\"");
        assert_eq!(line[line.len() - 2], "--");
    }
}
