//! Diffs the app opens without a repository: a patch (a file, stdin, the
//! clipboard), a series of `git format-patch` emails, or two folders or
//! files compared.
//!
//! A patch is split into its emails here ([`split`]); each email's diff goes
//! through [`crate::diff_parse`] like any other.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

/// Where a patch came from: what its tab and its header name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PatchOrigin {
    /// A file, read when the tab loads.
    File(PathBuf),
    /// This process's standard input, read to its end when the tab loads.
    Stdin,
    /// Text handed over whole: piped into another `diffui`, or pasted.
    Text {
        /// What it was: "stdin", "Clipboard".
        name: String,
        /// Tells two of them apart.
        id: u64,
        text: Arc<str>,
    },
}

impl PatchOrigin {
    /// How the header and the tab name it.
    pub fn name(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Stdin => "stdin".to_owned(),
            Self::Text { name, .. } => name.clone(),
        }
    }
}

/// A diff to open without a repository.
#[derive(Debug, Clone)]
pub enum DiffInput {
    /// A patch, a series of them, or any unified diff.
    Patch(PatchOrigin),
    /// Two files, or two folders file by file.
    Compare { left: PathBuf, right: PathBuf },
}

impl DiffInput {
    /// What tells this input apart from every other: the actor's id.
    pub fn key(&self) -> String {
        match self {
            Self::Patch(PatchOrigin::File(path)) => format!("patch:{}", path.display()),
            Self::Patch(PatchOrigin::Stdin) => "patch:stdin".to_owned(),
            Self::Patch(PatchOrigin::Text { id, .. }) => format!("patch:text:{id}"),
            Self::Compare { left, right } => {
                format!("compare:{}\u{0}{}", left.display(), right.display())
            }
        }
    }
}

// Text inputs are told apart by id: comparing (or hashing) the whole text of
// every open tab each time a tab is looked up would cost for nothing.
impl PartialEq for DiffInput {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for DiffInput {}

impl std::hash::Hash for DiffInput {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

/// An email's (or an `hg export`'s) header: who, when, and what it says.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mail {
    /// The subject, its `[PATCH n/m]` tag taken off.
    pub subject: String,
    /// `Name <email>`.
    pub author: String,
    /// `%Y-%m-%d %H:%M:%S %z`, the form revision headers read.
    pub date: Option<String>,
    pub millis: Option<i64>,
    /// The body above the diff: the commit message after its title, or a
    /// cover letter's text.
    pub message: String,
    /// The commit it was made from, when the email names one.
    pub commit: Option<String>,
}

/// One patch of a text: its email, when it has one, and where its diff is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub mail: Option<Mail>,
    /// Byte range of the part (email and diff) in the text.
    pub range: Range<usize>,
}

/// A text cut into its patches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patches {
    /// A series' cover letter: its first email, with no diff.
    pub cover: Option<Mail>,
    pub parts: Vec<Part>,
}

/// Cut `text` into its patches: an mbox of `git format-patch` emails gives
/// one per email (the first one a cover letter when it holds no diff);
/// anything else is one patch, with its email or `hg export` header read
/// when it starts with one.
pub fn split(text: &str) -> Patches {
    let starts = message_starts(text);
    if starts.len() < 2 {
        return Patches {
            cover: None,
            parts: vec![Part {
                mail: read_header(text),
                range: 0..text.len(),
            }],
        };
    }
    let mut ranges: Vec<Range<usize>> = starts
        .windows(2)
        .map(|pair| pair[0]..pair[1])
        .chain(starts.last().map(|&start| start..text.len()))
        .collect();
    // Anything ahead of the first email (a saved mailbox's preamble) joins it.
    ranges[0].start = 0;
    let mut patches = Patches::default();
    for (index, range) in ranges.into_iter().enumerate() {
        let part = &text[range.clone()];
        let mail = read_header(part);
        if !has_diff(part) {
            if index == 0 {
                patches.cover = mail;
            }
            continue;
        }
        patches.parts.push(Part { mail, range });
    }
    patches
}

/// Byte offsets of the mbox `From ` lines that start an email: one at the
/// top of the text or after a blank line, with a header line under it.
fn message_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0;
    let mut previous_blank = true;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let bare = line.trim_end_matches(['\n', '\r']);
        if previous_blank
            && bare.starts_with("From ")
            && lines.peek().is_some_and(|next| header_line(next).is_some())
        {
            starts.push(offset);
        }
        previous_blank = bare.is_empty();
        offset += line.len();
    }
    starts
}

/// `Name: value` of an email header line.
fn header_line(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.trim_end_matches(['\n', '\r']).split_once(':')?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some((name, value.trim()))
}

/// Whether a part holds a diff: a file header of any tool the parser reads.
fn has_diff(part: &str) -> bool {
    let mut previous_old = false;
    for line in part.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("diff --git ")
            || line.starts_with("diff --cc ")
            || (previous_old && line.starts_with("+++ "))
        {
            return true;
        }
        previous_old = line.starts_with("--- ");
    }
    false
}

/// Where a patch's message ends and its diff starts.
fn diff_start(line: &str) -> bool {
    line == "---"
        || line.starts_with("diff ")
        || line.starts_with("Index: ")
        || line.starts_with("--- ")
}

/// The header of an email (`From:`, `Date:`, `Subject:`, then the message)
/// or of an `hg export` (`# User`, `# Date`, then the message). `None` for
/// text that starts with neither.
fn read_header(part: &str) -> Option<Mail> {
    let mut lines = part
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .peekable();
    // The mbox separator, which names the commit in git's.
    let mut commit = None;
    if let Some(first) = lines.peek()
        && let Some(rest) = first.strip_prefix("From ")
    {
        commit = rest
            .split_whitespace()
            .next()
            .filter(|id| id.len() == 40 && id.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_owned);
        lines.next();
    }
    if lines.peek() == Some(&"# HG changeset patch") {
        return Some(read_hg_header(lines));
    }
    let mut mail = Mail {
        commit,
        ..Mail::default()
    };
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let (name, value) = header_line(line)?;
        fields.push((name.to_ascii_lowercase(), value.to_owned()));
    }
    if !fields
        .iter()
        .any(|(name, _)| name == "subject" || name == "from")
    {
        return None;
    }
    for (name, value) in fields {
        match name.as_str() {
            "subject" => mail.subject = strip_tag(&decode_words(&value)).to_owned(),
            "from" => mail.author = decode_words(&value),
            "date" => {
                if let Ok(time) = chrono::DateTime::parse_from_rfc2822(&value) {
                    mail.millis = Some(time.timestamp_millis());
                    mail.date = Some(time.format("%Y-%m-%d %H:%M:%S %z").to_string());
                }
            }
            _ => {}
        }
    }
    mail.message = message(lines);
    Some(mail)
}

/// An `hg export` header: `# User`, `# Date <seconds> <offset>`, `# Node
/// ID`, then the message.
fn read_hg_header<'a>(lines: impl Iterator<Item = &'a str>) -> Mail {
    let mut mail = Mail::default();
    let mut lines = lines.skip(1).peekable();
    while let Some(line) = lines.next_if(|line| line.starts_with('#')) {
        if let Some(user) = line.strip_prefix("# User ") {
            mail.author = user.trim().to_owned();
        } else if let Some(date) = line.strip_prefix("# Date ") {
            let mut parts = date.split_whitespace();
            if let Some(seconds) = parts.next().and_then(|seconds| seconds.parse::<i64>().ok()) {
                mail.millis = Some(seconds * 1000);
                // hg's offset is seconds west of UTC.
                let west: i32 = parts.next().and_then(|o| o.parse().ok()).unwrap_or(0);
                mail.date = chrono::FixedOffset::west_opt(west)
                    .and_then(|offset| {
                        chrono::DateTime::from_timestamp(seconds, 0)
                            .map(|time| time.with_timezone(&offset))
                    })
                    .map(|time| time.format("%Y-%m-%d %H:%M:%S %z").to_string());
            }
        } else if let Some(node) = line.strip_prefix("# Node ID ") {
            mail.commit = Some(node.trim().to_owned());
        }
    }
    let text = message(lines);
    let (subject, body) = text.split_once('\n').unwrap_or((text.as_str(), ""));
    mail.subject = subject.trim().to_owned();
    mail.message = body.trim().to_owned();
    mail
}

/// The message: the lines up to the diff (or, in a cover letter, up to its
/// shortlog or signature), trimmed.
fn message<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in lines {
        if diff_start(line) || line == "-- " || shortlog_line(line) {
            break;
        }
        out.push(line);
    }
    out.join("\n").trim().to_owned()
}

/// `Ada Lovelace (2):`, which opens the shortlog git writes into a cover
/// letter.
fn shortlog_line(line: &str) -> bool {
    line.strip_suffix("):")
        .and_then(|rest| rest.rsplit_once(" ("))
        .is_some_and(|(name, count)| {
            !name.is_empty()
                && !name.starts_with(' ')
                && !count.is_empty()
                && count.chars().all(|c| c.is_ascii_digit())
        })
}

/// `Fix it` out of `[PATCH v2 3/7] Fix it`.
fn strip_tag(subject: &str) -> &str {
    let subject = subject.trim();
    match subject
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        Some((tag, rest)) if tag.to_ascii_uppercase().contains("PATCH") => rest.trim(),
        _ => subject,
    }
}

/// RFC 2047 encoded words in a header (`=?UTF-8?q?Ada=20L=C3=B6?=`), the
/// way git writes a non-ASCII name or subject. Q encoding only: a
/// base64 word stays as written.
fn decode_words(value: &str) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let Some(word) = encoded_word(&rest[start..]) else {
            break;
        };
        let between = &rest[..start];
        // Whitespace between two encoded words is folding, not text.
        if !(last_was_word && between.trim().is_empty()) {
            out.push_str(between);
        }
        out.push_str(&word.0);
        rest = &rest[start + word.1..];
        last_was_word = true;
    }
    out.push_str(rest);
    out
}

/// One `=?charset?q?text?=` word: its text and its length.
fn encoded_word(word: &str) -> Option<(String, usize)> {
    let inner = word.strip_prefix("=?")?;
    let (charset, inner) = inner.split_once('?')?;
    let (encoding, inner) = inner.split_once('?')?;
    if !encoding.eq_ignore_ascii_case("q") {
        return None;
    }
    let end = inner.find("?=")?;
    let raw = &inner.as_bytes()[..end];
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'_' => bytes.push(b' '),
            b'=' => {
                let hex = std::str::from_utf8(raw.get(index + 1..index + 3)?).ok()?;
                bytes.push(u8::from_str_radix(hex, 16).ok()?);
                index += 2;
            }
            byte => bytes.push(byte),
        }
        index += 1;
    }
    let length = 2 + charset.len() + 1 + encoding.len() + 1 + end + 2;
    Some((String::from_utf8_lossy(&bytes).into_owned(), length))
}

/// Whether `text` reads as a diff: a file header of any kind the parser
/// reads, or an email with one. What tells a patch named without a `.patch`
/// or `.diff` extension from any other file.
pub fn looks_like_patch(text: &str) -> bool {
    has_diff(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/patches")
                .join(name),
        )
        .expect("the fixture")
    }

    #[test]
    fn a_format_patch_email_gives_its_subject_author_date_and_message() {
        let text = fixture("format-patch.patch");
        let patches = split(&text);
        assert_eq!(patches.cover, None);
        assert_eq!(patches.parts.len(), 1);
        let mail = patches.parts[0].mail.clone().expect("its header");
        assert_eq!(mail.subject, "Shout beta and add delta");
        assert_eq!(mail.author, "Ada Lovelace <ada@example.com>");
        assert_eq!(mail.date.as_deref(), Some("2024-03-04 10:00:00 +0000"));
        assert_eq!(
            mail.message,
            "The list reads better with one loud entry.\nDelta was missing."
        );
        assert_eq!(
            mail.commit.as_deref(),
            Some("2bd9b96d002b47b81681b17953ff17c9f7113b06")
        );
    }

    #[test]
    fn a_series_gives_its_cover_letter_and_each_patch() {
        let text = fixture("series.mbox");
        let patches = split(&text);
        let cover = patches.cover.expect("the cover letter");
        assert_eq!(cover.subject, "Make the list louder");
        assert_eq!(
            cover.message,
            "Two small changes to the list.\n\nThe second copies it."
        );
        let subjects: Vec<_> = patches
            .parts
            .iter()
            .map(|part| part.mail.as_ref().map(|mail| mail.subject.as_str()))
            .collect();
        assert_eq!(
            subjects,
            [Some("Shout beta and add delta"), Some("Copy the list")]
        );
        let second = &text[patches.parts[1].range.clone()];
        assert!(second.starts_with("From c0b00974"));
        assert!(!second.contains("Shout beta"));
        assert_eq!(patches.parts[1].mail.as_ref().unwrap().message, "");
    }

    #[test]
    fn an_hg_export_gives_its_user_date_and_message() {
        let patches = split(&fixture("hg.diff"));
        let mail = patches.parts[0].mail.clone().expect("its header");
        assert_eq!(mail.subject, "Count the bugs");
        assert_eq!(mail.author, "Grace Hopper <grace@example.com>");
        assert_eq!(mail.millis, Some(1_709_546_400_000));
        assert_eq!(mail.date.as_deref(), Some("2024-03-04 10:00:00 +0000"));
    }

    #[test]
    fn a_plain_diff_is_one_patch_with_no_header() {
        let patches = split(&fixture("diff-ruN.diff"));
        assert_eq!(patches.parts.len(), 1);
        assert_eq!(patches.parts[0].mail, None);
    }

    #[test]
    fn encoded_words_and_patch_tags_come_off() {
        assert_eq!(
            decode_words("=?UTF-8?q?Ada=20L=C3=B6velace?= <ada@example.com>"),
            "Ada Lövelace <ada@example.com>"
        );
        assert_eq!(decode_words("=?UTF-8?q?a?= =?UTF-8?q?b?="), "ab");
        assert_eq!(strip_tag("[PATCH v2 3/7] Fix it"), "Fix it");
        assert_eq!(strip_tag("[RFC] Fix it"), "[RFC] Fix it");
    }

    #[test]
    fn what_reads_as_a_patch() {
        for name in [
            "format-patch.patch",
            "svn.diff",
            "diff-u.diff",
            "combined.diff",
        ] {
            assert!(looks_like_patch(&fixture(name)), "{name}");
        }
        assert!(!looks_like_patch("fn main() {}\n// --- not a diff\n"));
    }
}
