//! Unified-diff (`git diff`/`git show`) parser into a `DiffDocument`, plus the
//! shared hunk-header formatter. Pure `std`; highlighting is applied via
//! `crate::syntax` as each file is flushed.

use crate::model::{DiffDocument, DiffFile, DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind};

#[derive(Debug, Clone)]
struct PendingHunk {
    header: String,
    rows: Vec<DiffLine>,
    next_old_line: usize,
    next_new_line: usize,
    /// Old and new lines the header says are still to come. `None` when the
    /// header gives no counts (a forge's placeholder), and then the hunk runs
    /// to the next header, as every hunk used to.
    left: Option<(usize, usize)>,
    /// Marker columns before each line's text: 1, or one per parent in a
    /// combined (`diff --cc`) diff.
    columns: usize,
}

impl PendingHunk {
    /// Whether the next line is still the hunk's. Once its counts are used
    /// up, what follows (a mail signature, commentary between files) isn't
    /// diff content.
    fn open(&self) -> bool {
        self.left.is_none_or(|(old, new)| old > 0 || new > 0)
    }

    fn take(&mut self, old: bool, new: bool) {
        if let Some((left_old, left_new)) = &mut self.left {
            *left_old = left_old.saturating_sub(usize::from(old));
            *left_new = left_new.saturating_sub(usize::from(new));
        }
    }
}

pub fn format_hunk_header(
    old_range: &std::ops::Range<usize>,
    new_range: &std::ops::Range<usize>,
) -> String {
    format!(
        "@@ -{} +{} @@",
        format_hunk_range(old_range),
        format_hunk_range(new_range)
    )
}

fn format_hunk_range(range: &std::ops::Range<usize>) -> String {
    let len = range.end.saturating_sub(range.start);
    let start = if len == 0 {
        range.start
    } else {
        range.start + 1
    };

    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

/// Incremental unified-diff parser: feed lines as they arrive (e.g. off a
/// `gh pr diff` pipe) and receive each completed [`DiffFile`] — highlighted,
/// ready to display — as soon as the next file's header shows it's done, so a
/// huge diff renders progressively instead of after the whole download.
/// [`parse_unified_diff`] is the one-shot wrapper over this.
///
/// Besides git's own output it reads what other tools write: plain
/// `---`/`+++` pairs (`diff -u`, `diff -ruN`, svn, hg) with whatever
/// headers they put before them (`Index:`, `===`, `diff -r …`), a
/// `git format-patch` email around its diff, combined merge diffs
/// (`diff --cc`), shown against the first parent, and CRLF line ends.
/// A hunk ends when its header's line counts are used up, so text after
/// it is never read as rows.
#[derive(Default)]
pub struct DiffStreamParser {
    current_file: Option<DiffFile>,
    current_hunk: Option<PendingHunk>,
    /// The current file's header lines are still coming: from its
    /// `diff --git` to its `+++` or first hunk.
    in_header: bool,
    /// The file came from a `---`/`+++` pair alone, with no `diff --git`
    /// header to say whether it was added or deleted.
    plain: bool,
    /// A `---` line outside any header: a plain diff's old side, waiting
    /// for its `+++`.
    pending_old: Option<String>,
    /// Fed bare hunks by [`Self::begin_file`]: a forge's per-file patch,
    /// with nothing after its hunks, so they run to the next header as they
    /// always have, whatever their counts say.
    bare: bool,
    /// Every line ends in CRLF; decided by the first line, so git's own
    /// output (whose first line never does) keeps a CR that's content.
    crlf: Option<bool>,
}

impl DiffStreamParser {
    /// Consume one line (without its trailing newline). Returns the previous
    /// file when `line` starts a new one.
    pub fn push_line(&mut self, line: &str) -> Option<DiffFile> {
        let crlf = *self.crlf.get_or_insert_with(|| line.ends_with('\r'));
        let line = if crlf {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };

        if let Some(paths) = line.strip_prefix("diff --git ") {
            let (old_path, path) = parse_diff_git_paths(paths);
            return self.start_file(path, old_path);
        }
        if let Some(path) = line
            .strip_prefix("diff --cc ")
            .or_else(|| line.strip_prefix("diff --combined "))
        {
            let path = unquote_git_path(path);
            return self.start_file(path.clone(), Some(path));
        }

        if line.starts_with("@@") {
            let file = self.current_file.as_mut()?;
            flush_current_hunk(file, &mut self.current_hunk);
            self.in_header = false;
            let mut hunk = parse_hunk_header(line);
            if self.bare {
                hunk.left = None;
            }
            if hunk.columns > 1 && file.hunks.is_empty() {
                hunk.rows.push(note_row(format!(
                    "Combined diff of {} parents, shown against the first",
                    hunk.columns
                )));
            }
            self.current_hunk = Some(hunk);
            return None;
        }

        if let (Some(file), Some(hunk)) = (self.current_file.as_mut(), self.current_hunk.as_mut())
            && (hunk.open() || line.starts_with('\\'))
        {
            push_hunk_row(file, hunk, line);
            return None;
        }

        if self.in_header {
            let file = self.current_file.as_mut()?;
            if line.starts_with("+++ ") {
                self.in_header = false;
            }
            update_file_metadata(file, line);
            return None;
        }

        // Between files: a plain diff's `---`/`+++` pair starts the next
        // one; anything else (an email's headers and message, `Index:`
        // lines, commentary) is skipped.
        if let Some(old) = line.strip_prefix("--- ") {
            self.pending_old = Some(old.to_owned());
            return None;
        }
        let pending_old = self.pending_old.take();
        if let (Some(old), Some(new)) = (pending_old, line.strip_prefix("+++ ")) {
            let finished = self.start_file(String::new(), None);
            self.in_header = false;
            self.plain = true;
            if let Some(file) = self.current_file.as_mut() {
                set_plain_paths(file, &old, new);
            }
            return finished;
        }
        if let Some(sides) = line
            .strip_prefix("Binary files ")
            .and_then(|rest| rest.strip_suffix(" differ"))
            && let Some((old, new)) = sides.split_once(" and ")
        {
            let finished = self.start_file(String::new(), None);
            self.in_header = false;
            if let Some(file) = self.current_file.as_mut() {
                set_plain_paths(file, old, new);
                file.hunks.push(binary_hunk());
            }
            return finished;
        }
        None
    }

    /// Seed the parser as if a `diff --git` header for `file` was just
    /// consumed, so bare hunk sequences — the shape GitHub's files API returns
    /// in its `patch` field — can be fed straight in. Returns the previously
    /// in-progress file like [`push_line`](Self::push_line) does.
    pub fn begin_file(&mut self, file: DiffFile) -> Option<DiffFile> {
        let finished = self.take_file();
        self.current_file = Some(file);
        self.bare = true;
        finished
    }

    /// End of input: the file still in progress, if any.
    pub fn finish(mut self) -> Option<DiffFile> {
        self.take_file()
    }

    /// Finish the file in progress and start one at `path`, its header to
    /// follow.
    fn start_file(&mut self, path: String, old_path: Option<String>) -> Option<DiffFile> {
        let finished = self.take_file();
        self.current_file = Some(DiffFile {
            path,
            old_path,
            status: DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
            collapsed: None,
            blob: None,
            mode_change: None,
        });
        self.in_header = true;
        finished
    }

    fn take_file(&mut self) -> Option<DiffFile> {
        let mut file = self.current_file.take()?;
        flush_current_hunk(&mut file, &mut self.current_hunk);
        self.in_header = false;
        self.pending_old = None;
        if std::mem::take(&mut self.plain) {
            infer_plain_status(&mut file);
        }
        // No highlighting here: it reads each file's full sources, so it runs
        // in the background after the document is already on screen (see
        // `syntax::highlight_file`).
        Some(file)
    }
}

/// Parse `git diff` / `git show` style unified diff output into a
/// `DiffDocument`. Used by the git backend; the jj backend builds files
/// directly from materialized trees and only feeds individual hunks through
/// `format_hunk_header` + line construction.
pub fn parse_unified_diff(output: &str) -> DiffDocument {
    let mut parser = DiffStreamParser::default();
    let mut files = Vec::new();
    for line in output.lines() {
        files.extend(parser.push_line(line));
    }
    files.extend(parser.finish());

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    DiffDocument {
        files,
        total_additions,
        total_deletions,
        range: None,
    }
}

fn flush_current_hunk(file: &mut DiffFile, current_hunk: &mut Option<PendingHunk>) {
    if let Some(mut hunk) = current_hunk.take() {
        mark_intra_line_changes(&mut hunk.rows);
        file.hunks.push(DiffHunkView {
            header: hunk.header,
            lines: hunk.rows,
        });
    }
}

fn update_file_metadata(file: &mut DiffFile, line: &str) {
    // A copy shows as the jj backend shows one: under both its paths, the
    // way a rename does.
    if let Some(path) = line
        .strip_prefix("rename from ")
        .or_else(|| line.strip_prefix("copy from "))
    {
        file.old_path = Some(unquote_git_path(path));
        file.status = DiffFileStatus::Renamed;
    } else if let Some(path) = line
        .strip_prefix("rename to ")
        .or_else(|| line.strip_prefix("copy to "))
    {
        file.path = unquote_git_path(path);
        file.status = DiffFileStatus::Renamed;
    } else if let Some(mode) = line.strip_prefix("old mode ").and_then(parse_mode) {
        file.mode_change = Some((mode, file.mode_change.map_or(mode, |(_, new)| new)));
    } else if let Some(mode) = line.strip_prefix("new mode ").and_then(parse_mode) {
        file.mode_change = Some((file.mode_change.map_or(mode, |(old, _)| old), mode));
    } else if line.starts_with("new file mode ") || line == "--- /dev/null" {
        file.status = DiffFileStatus::Added;
    } else if line.starts_with("deleted file mode ") || line == "+++ /dev/null" {
        file.status = DiffFileStatus::Deleted;
    } else if let Some(path) = line
        .strip_prefix("--- ")
        .and_then(|rest| side_path(rest, "a/"))
    {
        file.old_path = Some(path);
    } else if let Some(path) = line
        .strip_prefix("+++ ")
        .and_then(|rest| side_path(rest, "b/"))
    {
        file.path = path;
    } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
        // git prints this instead of hunks and the file would otherwise render
        // as an empty Modified. One header-only hunk, the shape the jj backend
        // gives a binary change (see `jj::load_jj_diff`), so both backends and
        // the PR path show the same row.
        file.hunks.push(binary_hunk());
    }
}

/// A git file mode as its octal digits print it (`100755`).
pub fn parse_mode(digits: &str) -> Option<u32> {
    u32::from_str_radix(digits.trim(), 8).ok()
}

/// One header-only hunk standing in for content the diff can't show.
fn binary_hunk() -> DiffHunkView {
    DiffHunkView {
        header: "binary files differ".to_owned(),
        lines: Vec::new(),
    }
}

/// The path out of a `--- a/name` / `+++ b/name` line. git appends a tab after
/// a name containing a space (GNU patch compatibility) and C-quotes names
/// holding a quote, a backslash or a control character, so neither the tail
/// nor the prefix survives a plain `strip_prefix`. `None` for `/dev/null` and
/// for any other prefix, leaving the path as the `diff --git` header set it.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let path = unquote_git_path(rest.trim_end_matches('\t'));
    path.strip_prefix(prefix).map(str::to_owned)
}

/// The paths of a plain diff's `--- old` / `+++ new` pair. Each may carry a
/// tab and a timestamp (`diff -u`) or a revision (`(revision 12)`, svn)
/// after it. Like `patch -p1`, a leading folder that differs between the
/// sides (`a/` and `b/`, `old/` and `new/`) is the tool's, not the file's.
fn set_plain_paths(file: &mut DiffFile, old: &str, new: &str) {
    let name = |side: &str| unquote_git_path(side.split('\t').next().unwrap_or(side).trim_end());
    let (old, new) = (name(old), name(new));
    let absent = |path: &str| path == "/dev/null";
    let strip = match (old.split_once('/'), new.split_once('/')) {
        (Some((old_top, _)), Some((new_top, _))) => old_top != new_top,
        (None, Some(_)) if absent(&old) => new.starts_with("b/"),
        (Some(_), None) if absent(&new) => old.starts_with("a/"),
        _ => false,
    };
    let trim = |path: String| match path.split_once('/') {
        Some((_, rest)) if strip && !absent(&path) => rest.to_owned(),
        _ => path,
    };
    let (old, new) = (trim(old), trim(new));
    if absent(&old) {
        file.status = DiffFileStatus::Added;
        file.path = new;
    } else if absent(&new) {
        file.status = DiffFileStatus::Deleted;
        file.path = old.clone();
        file.old_path = Some(old);
    } else {
        file.path = new;
        file.old_path = Some(old);
    }
}

/// A plain diff names no added or deleted files: `diff -N` writes the
/// absent side as an empty file. Its one hunk then starts at line 0 there.
fn infer_plain_status(file: &mut DiffFile) {
    if file.status != DiffFileStatus::Modified || file.hunks.len() != 1 {
        return;
    }
    let mut ranges = file.hunks[0].header.split_whitespace().skip(1);
    match (ranges.next(), ranges.next()) {
        (Some("-0,0"), _) => file.status = DiffFileStatus::Added,
        (_, Some("+0,0")) => file.status = DiffFileStatus::Deleted,
        _ => {}
    }
}

/// A row with no line numbers: a `\ No newline` marker, or something the
/// parser says about the file.
fn note_row(content: String) -> DiffLine {
    DiffLine {
        kind: DiffLineKind::Note,
        old_line: None,
        new_line: None,
        content,
        syntax: Vec::new(),
        emphasis: Vec::new(),
    }
}

/// One line of a hunk. A combined diff's line has a marker per parent and
/// shows as a diff against the first: a line only another parent had is
/// left out.
fn push_hunk_row(file: &mut DiffFile, hunk: &mut PendingHunk, line: &str) {
    if hunk.columns > 1 && !line.starts_with('\\') {
        let markers = line.get(..hunk.columns).unwrap_or(line);
        let text = line.get(hunk.columns..).unwrap_or_default();
        let in_result = !markers.contains('-');
        let first = markers.chars().next().unwrap_or(' ');
        let row = match (in_result, first) {
            (true, '+') => format!("+{text}"),
            (true, _) => format!(" {text}"),
            (false, '-') => format!("-{text}"),
            (false, _) => {
                hunk.take(false, false);
                return;
            }
        };
        let columns = std::mem::replace(&mut hunk.columns, 1);
        push_hunk_row(file, hunk, &row);
        hunk.columns = columns;
        return;
    }
    match line.chars().next() {
        Some('+') => {
            file.additions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
            hunk.next_new_line += 1;
            hunk.take(false, true);
        }
        Some('-') => {
            file.deletions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Deletion,
                old_line: Some(hunk.next_old_line),
                new_line: None,
                content: line[1..].to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
            hunk.next_old_line += 1;
            hunk.take(true, false);
        }
        // An empty line is a context line that lost its leading space —
        // `diff.suppressBlankEmpty`, or a diff that went through a
        // whitespace-stripping pipe. As a numberless Note it would shift the
        // line numbers, and the full-source highlight mapping with them, for
        // the rest of the hunk.
        Some(' ') | None => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(hunk.next_old_line),
                new_line: Some(hunk.next_new_line),
                content: line.get(1..).unwrap_or_default().to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
            hunk.next_old_line += 1;
            hunk.next_new_line += 1;
            hunk.take(true, true);
        }
        Some('\\') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Note,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
        }
        _ => {
            let kind = if is_conflict_marker(line) {
                DiffLineKind::Conflict
            } else {
                DiffLineKind::Note
            };

            hunk.rows.push(DiffLine {
                kind,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
        }
    }
}

fn is_conflict_marker(line: &str) -> bool {
    line.starts_with("<<<<<<<")
        || line.starts_with("|||||||")
        || line.starts_with("=======")
        || line.starts_with(">>>>>>>")
}

fn parse_diff_git_paths(paths: &str) -> (Option<String>, String) {
    let (old, new) = split_diff_git_paths(paths);
    let old_path = (!old.is_empty()).then(|| clean_git_diff_path(old));
    let path = new
        .map(clean_git_diff_path)
        .or_else(|| old_path.clone())
        .unwrap_or_else(|| "<unknown>".to_owned());

    (old_path, path)
}

/// Split the two names in a `diff --git ` header.
///
/// Whitespace is not a separator: git only quotes a name that holds a quote,
/// a backslash or a control character (non-ASCII too, unless the caller pins
/// `core.quotePath=false`), so `a/mode only.sh b/mode only.sh` arrives with
/// three spaces and used to read as a rename of `mode` to `only.sh`. Use
/// `git apply`'s rule — the split is the whitespace where both sides, minus
/// their `a/`/`b/` prefix, spell the same name — plus the unambiguous case of
/// a quote opening the second name. A rename's two names differ, so nothing
/// matches and the first whitespace is the fallback; its real names arrive on
/// the `rename from`/`rename to` lines.
fn split_diff_git_paths(paths: &str) -> (&str, Option<&str>) {
    if paths.starts_with('"')
        && let Some(end) = quoted_path_end(paths)
    {
        let rest = paths[end..].trim_start();
        return (&paths[..end], (!rest.is_empty()).then_some(rest));
    }

    for (index, _) in paths.match_indices([' ', '\t']) {
        let (left, right) = (&paths[..index], &paths[index + 1..]);
        if right.starts_with('"') || strip_path_prefix(left) == strip_path_prefix(right) {
            return (left, Some(right));
        }
    }

    match paths.split_once([' ', '\t']) {
        Some((left, right)) => (left, Some(right)),
        None => (paths, None),
    }
}

/// Byte offset just past the closing quote of a C-quoted name, honouring
/// backslash escapes so a name ending in `\"` doesn't close early.
fn quoted_path_end(paths: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, byte) in paths.bytes().enumerate().skip(1) {
        match byte {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'"' => return Some(index + 1),
            _ => {}
        }
    }
    None
}

fn clean_git_diff_path(path: &str) -> String {
    strip_path_prefix(&unquote_git_path(path)).to_owned()
}

fn strip_path_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

/// Undo git's C-style path quoting (`"caf\303\251.txt"`). The escapes carry
/// bytes, not characters, so they are collected as bytes and decoded lossily —
/// a latin-1 filename then still names something, where the old raw form
/// showed `caf\303\251.txt` and lost syntax highlighting with it.
fn unquote_git_path(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return path.to_owned();
    };

    let raw = inner.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        index += 1;
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let Some(&escape) = raw.get(index) else {
            bytes.push(b'\\');
            break;
        };
        index += 1;
        match escape {
            b'a' => bytes.push(0x07),
            b'b' => bytes.push(0x08),
            b't' => bytes.push(b'\t'),
            b'n' => bytes.push(b'\n'),
            b'v' => bytes.push(0x0b),
            b'f' => bytes.push(0x0c),
            b'r' => bytes.push(b'\r'),
            b'"' | b'\\' => bytes.push(escape),
            b'0'..=b'7' => {
                let mut value = escape - b'0';
                for _ in 0..2 {
                    match raw.get(index) {
                        Some(digit @ b'0'..=b'7') => {
                            value = value.wrapping_mul(8).wrapping_add(digit - b'0');
                            index += 1;
                        }
                        _ => break,
                    }
                }
                bytes.push(value);
            }
            // Not an escape git emits: keep both bytes so the name still
            // resembles what's on disk.
            _ => bytes.extend_from_slice(&[b'\\', escape]),
        }
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

/// A hunk from its header: `@@ -12,3 +12,4 @@`, or a combined diff's
/// `@@@ -12,3 -12,3 +12,4 @@@` with a range per parent, of which the
/// first counts.
fn parse_hunk_header(header: &str) -> PendingHunk {
    let mut parts = header.split_whitespace();
    let columns = parts
        .next()
        .map_or(1, |marker| marker.len().saturating_sub(1).max(1));
    let old = parts.next().and_then(|part| parse_hunk_range(part, '-'));
    let new = parts
        .find(|part| part.starts_with('+'))
        .and_then(|part| parse_hunk_range(part, '+'));

    PendingHunk {
        header: header.to_owned(),
        rows: Vec::new(),
        next_old_line: old.map_or(0, |(start, _)| start),
        next_new_line: new.map_or(0, |(start, _)| start),
        left: old.zip(new).map(|((_, old), (_, new))| (old, new)),
        columns,
    }
}

/// `-start,count` (or `-start`, a count of one) as its two numbers.
fn parse_hunk_range(part: &str, prefix: char) -> Option<(usize, usize)> {
    let range = part.strip_prefix(prefix)?;
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start, count.parse().ok()?),
        None => (range, 1),
    };
    Some((start.parse().ok()?, count))
}

// ---- Intra-line (word-level) change emphasis ----

/// Skip token-diffing pathological lines so emphasis never slows a load: a
/// minified one-liner blows past the byte cap, generated code past the token
/// cap. Such lines simply render without emphasis.
const EMPHASIS_MAX_LINE_BYTES: usize = 4096;
const EMPHASIS_MAX_TOKENS: usize = 256;

/// Fill [`DiffLine::emphasis`] for the deletion/addition lines of one hunk.
///
/// Each maximal run of `-` lines followed by a run of `+` lines is paired
/// index-wise (the shape unified diffs always emit for a modification) and
/// every pair is token-diffed; leftover unpaired lines are pure
/// removals/insertions whose line tint already says everything. The jj
/// backend doesn't come through here — jj-lib's word-level refinement
/// already yields per-token ranges (see `jj::diff_tokens_to_line`) — but
/// both paths share [`finish_line_emphasis`] so gating stays consistent.
pub fn mark_intra_line_changes(lines: &mut [DiffLine]) {
    let mut scratch = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != DiffLineKind::Deletion {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == DiffLineKind::Deletion {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == DiffLineKind::Addition {
            i += 1;
        }
        let pair_count = (add_start - del_start).min(i - add_start);
        for offset in 0..pair_count {
            let (old_emphasis, new_emphasis) = intra_line_emphasis(
                &lines[del_start + offset].content,
                &lines[add_start + offset].content,
                &mut scratch,
            );
            lines[del_start + offset].emphasis = old_emphasis;
            lines[add_start + offset].emphasis = new_emphasis;
        }
    }
}

/// Byte ranges of the changed tokens within one line — the type behind
/// [`crate::model::DiffLine::emphasis`].
type EmphasisRanges = Vec<(usize, usize)>;

/// Token-diff one old/new line pair into per-side changed byte ranges.
/// `scratch` is the LCS table buffer, reused across pairs to keep the hot
/// parse path allocation-light.
fn intra_line_emphasis(
    old: &str,
    new: &str,
    scratch: &mut Vec<u16>,
) -> (EmphasisRanges, EmphasisRanges) {
    let none = (Vec::new(), Vec::new());
    if old.len() > EMPHASIS_MAX_LINE_BYTES || new.len() > EMPHASIS_MAX_LINE_BYTES {
        return none;
    }
    let old_tokens = token_ranges(old);
    let new_tokens = token_ranges(new);
    let (n, m) = (old_tokens.len(), new_tokens.len());
    if n == 0 || m == 0 || n > EMPHASIS_MAX_TOKENS || m > EMPHASIS_MAX_TOKENS {
        return none;
    }

    // Classic LCS table over token text, walked back to flag the tokens
    // outside the common subsequence. Quadratic, but bounded by the token
    // cap (256² u16 = 128KiB, reused via `scratch`).
    fn tok(s: &str, r: (usize, usize)) -> &str {
        &s[r.0..r.1]
    }
    let cols = m + 1;
    scratch.clear();
    scratch.resize((n + 1) * cols, 0);
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            scratch[i * cols + j] = if tok(old, old_tokens[i]) == tok(new, new_tokens[j]) {
                scratch[(i + 1) * cols + j + 1] + 1
            } else {
                scratch[(i + 1) * cols + j].max(scratch[i * cols + j + 1])
            };
        }
    }

    let mut old_raw = Vec::new();
    let mut new_raw = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if tok(old, old_tokens[i]) == tok(new, new_tokens[j]) {
            i += 1;
            j += 1;
        } else if scratch[(i + 1) * cols + j] >= scratch[i * cols + j + 1] {
            old_raw.push(old_tokens[i]);
            i += 1;
        } else {
            new_raw.push(new_tokens[j]);
            j += 1;
        }
    }
    old_raw.extend_from_slice(&old_tokens[i..]);
    new_raw.extend_from_slice(&new_tokens[j..]);

    (
        finish_line_emphasis(old, old_raw),
        finish_line_emphasis(new, new_raw),
    )
}

/// Split a line into emphasis tokens: identifier runs (alphanumeric + `_`),
/// whitespace runs, and individual punctuation characters — so `;` → `,`
/// emphasizes one character, not the whole operator neighborhood.
fn token_ranges(content: &str) -> Vec<(usize, usize)> {
    #[derive(PartialEq, Clone, Copy)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class_of = |ch: char| {
        if ch.is_alphanumeric() || ch == '_' {
            Class::Word
        } else if ch.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };

    let mut tokens = Vec::new();
    let mut start = 0;
    let mut current: Option<Class> = None;
    for (idx, ch) in content.char_indices() {
        let class = class_of(ch);
        // `Other` never extends: each punctuation char is its own token.
        if current == Some(class) && class != Class::Other {
            continue;
        }
        if current.is_some() {
            tokens.push((start, idx));
        }
        start = idx;
        current = Some(class);
    }
    if current.is_some() {
        tokens.push((start, content.len()));
    }
    tokens
}

/// Normalize raw changed-token ranges into display-ready emphasis: clamp to
/// the content, merge ranges separated only by whitespace, trim whitespace
/// edges, and drop the emphasis entirely when it covers nearly the whole
/// line — there the line tint already tells the story and per-token paint
/// is pure noise. Shared by the parser path above and the jj backend.
pub(crate) fn finish_line_emphasis(content: &str, raw: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut raw: Vec<(usize, usize)> = raw
        .into_iter()
        .map(|(start, end)| (start.min(content.len()), end.min(content.len())))
        .filter(|&(start, end)| {
            start < end && content.is_char_boundary(start) && content.is_char_boundary(end)
        })
        .collect();
    if raw.is_empty() {
        return raw;
    }
    raw.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(raw.len());
    for (start, end) in raw {
        match merged.last_mut() {
            Some(last)
                if start <= last.1 || content[last.1..start].chars().all(char::is_whitespace) =>
            {
                last.1 = last.1.max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    let mut result = Vec::with_capacity(merged.len());
    for (start, end) in merged {
        let segment = &content[start..end];
        let trimmed_start = start + (segment.len() - segment.trim_start().len());
        let trimmed_end = end - (segment.len() - segment.trim_end().len());
        if trimmed_start < trimmed_end {
            result.push((trimmed_start, trimmed_end));
        }
    }

    let total: usize = content.chars().filter(|c| !c.is_whitespace()).count();
    let changed: usize = result
        .iter()
        .map(|&(start, end)| {
            content[start..end]
                .chars()
                .filter(|c| !c.is_whitespace())
                .count()
        })
        .sum();
    // "Nearly the whole line": ≥85% of its non-whitespace characters.
    if total == 0 || changed * 20 >= total * 17 {
        return Vec::new();
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_diff_into_hunks_and_rows() {
        let document = parse_unified_diff(
            "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,2 +10,3 @@ fn demo()\n let old_value = 0;\n-let old_value = 1;\n+let new_value = 1;\n+let second_line = 2;\n",
        );

        assert_eq!(document.files.len(), 1);
        assert_eq!(document.total_additions, 2);
        assert_eq!(document.total_deletions, 1);

        let file = &document.files[0];
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
        assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
        assert_eq!(file.hunks[0].lines[1].kind, DiffLineKind::Deletion);
        assert_eq!(file.hunks[0].lines[1].old_line, Some(11));
        assert_eq!(file.hunks[0].lines[1].new_line, None);
        assert_eq!(file.hunks[0].lines[2].kind, DiffLineKind::Addition);
        assert_eq!(file.hunks[0].lines[2].old_line, None);
        assert_eq!(file.hunks[0].lines[2].new_line, Some(11));
        // Highlighting no longer runs in the parser — it's applied in the
        // background after the document is on screen.
        assert!(file.hunks[0].lines[2].syntax.is_empty());
    }

    #[test]
    fn parses_conflict_markers() {
        let document = parse_unified_diff(
            "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n<<<<<<< mine\n",
        );

        assert_eq!(
            document.files[0].hunks[0].lines[0].kind,
            DiffLineKind::Conflict
        );
    }

    #[test]
    fn parses_rename_metadata() {
        let document = parse_unified_diff(
            "diff --git a/src/old.rs b/src/new.rs\nsimilarity index 100%\nrename from src/old.rs\nrename to src/new.rs\n",
        );

        let file = &document.files[0];
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(file.path, "src/new.rs");
    }

    /// A mode-only change: no `---`/`+++` lines to correct the header, and
    /// three spaces in it. Splitting on whitespace read this as a rename of
    /// `mode` to `only.sh`.
    #[test]
    fn parses_spaced_path_in_a_mode_only_change() {
        let document = parse_unified_diff(
            "diff --git a/mode only.sh b/mode only.sh\nold mode 100644\nnew mode 100755\n",
        );

        let file = &document.files[0];
        assert_eq!(file.path, "mode only.sh");
        assert_eq!(file.old_path.as_deref(), Some("mode only.sh"));
        assert_eq!(file.mode_change, Some((0o100644, 0o100755)));
        assert_eq!(file.status, DiffFileStatus::Modified);
    }

    /// A spaced path again, this time with the tab git appends to the
    /// `---`/`+++` lines for GNU patch — and a context line that lost its
    /// leading space (`diff.suppressBlankEmpty`), which must not shift the
    /// numbering of the lines below it.
    #[test]
    fn parses_spaced_path_headers_and_unprefixed_blank_lines() {
        let document = parse_unified_diff(
            "diff --git a/space name.txt b/space name.txt\nindex a1a53b5..bc8fe6d 100644\n--- a/space name.txt\t\n+++ b/space name.txt\t\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n",
        );

        let file = &document.files[0];
        assert_eq!(file.path, "space name.txt");
        assert_eq!(file.old_path.as_deref(), Some("space name.txt"));
        let lines = &file.hunks[0].lines;
        assert_eq!(lines[1].kind, DiffLineKind::Context);
        assert_eq!(lines[1].content, "");
        assert_eq!(lines[1].old_line, Some(2));
        assert_eq!(lines[1].new_line, Some(2));
        assert_eq!(lines[2].kind, DiffLineKind::Deletion);
        assert_eq!(lines[2].old_line, Some(3));
        assert_eq!(lines[3].new_line, Some(3));
    }

    /// A C-quoted non-ASCII path — what a PR diff or a repo we don't control
    /// the config of still sends. Unquoted it names a real file (and gets
    /// highlighting); raw it read as a rename to `b/caf\303\251.txt`.
    #[test]
    fn unquotes_non_ascii_paths() {
        let document = parse_unified_diff(
            "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n--- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n-héllo\n+héllo2\n",
        );

        let file = &document.files[0];
        assert_eq!(file.path, "café.txt");
        assert_eq!(file.old_path.as_deref(), Some("café.txt"));
        assert_eq!(file.status, DiffFileStatus::Modified);
    }

    /// Quoting survives `core.quotePath=false` when the name holds a quote,
    /// and only one side of this rename is quoted.
    #[test]
    fn parses_rename_with_a_quoted_old_path() {
        let document = parse_unified_diff(
            "diff --git \"a/has\\\"quote.txt\" b/renamed file.txt\nsimilarity index 100%\nrename from \"has\\\"quote.txt\"\nrename to renamed file.txt\n",
        );

        let file = &document.files[0];
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("has\"quote.txt"));
        assert_eq!(file.path, "renamed file.txt");
    }

    #[test]
    fn binary_file_gets_the_shared_binary_hunk() {
        let document = parse_unified_diff(
            "diff --git a/blob.bin b/blob.bin\nindex 20f982d..ac09f6c 100644\nBinary files a/blob.bin and b/blob.bin differ\n",
        );

        let file = &document.files[0];
        assert_eq!(file.path, "blob.bin");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].header, "binary files differ");
        assert!(file.hunks[0].lines.is_empty());
        assert_eq!(document.total_additions, 0);
    }

    /// A stream cut mid-hunk (a killed `gh pr diff`, a truncated read) still
    /// yields the file with the rows that did arrive.
    #[test]
    fn truncated_hunk_still_yields_its_file() {
        let document =
            parse_unified_diff("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b");

        let file = &document.files[0];
        assert_eq!(file.hunks[0].lines.len(), 2);
        assert_eq!(file.hunks[0].lines[1].kind, DiffLineKind::Deletion);
        assert_eq!(document.total_deletions, 1);
    }

    #[test]
    fn empty_diff_yields_no_files() {
        let document = parse_unified_diff("");

        assert!(document.files.is_empty());
        assert_eq!(document.total_additions, 0);
        assert_eq!(document.total_deletions, 0);
    }

    #[test]
    fn marks_changed_tokens_in_paired_lines() {
        let document = parse_unified_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-let old_value = 1;\n+let new_value = 1;\n",
        );

        let lines = &document.files[0].hunks[0].lines;
        assert_eq!(lines[0].emphasis, vec![(4, 13)]);
        assert_eq!(lines[1].emphasis, vec![(4, 13)]);
    }

    #[test]
    fn bridges_whitespace_between_adjacent_changed_tokens() {
        let document = parse_unified_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a b c\n+x y c\n",
        );

        let lines = &document.files[0].hunks[0].lines;
        assert_eq!(lines[0].emphasis, vec![(0, 3)]);
        assert_eq!(lines[1].emphasis, vec![(0, 3)]);
    }

    #[test]
    fn rewrites_and_unpaired_lines_get_no_emphasis() {
        let document = parse_unified_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,1 @@\n-foo bar baz\n-second removed line\n+qux quux corge\n",
        );

        let lines = &document.files[0].hunks[0].lines;
        // The (0 ↔ 2) pair shares no tokens, so the coverage gate drops both
        // sides; the second deletion has no partner at all.
        assert!(lines[0].emphasis.is_empty());
        assert!(lines[1].emphasis.is_empty());
        assert!(lines[2].emphasis.is_empty());
    }

    #[test]
    fn finish_line_emphasis_clamps_trims_and_gates() {
        assert_eq!(finish_line_emphasis("foo bar", vec![(0, 3)]), vec![(0, 3)]);
        // Out-of-range end clamps to the line, which then covers everything
        // and gets gated away.
        assert!(finish_line_emphasis("foo", vec![(0, 4)]).is_empty());
        // Whitespace-only emphasis trims to nothing.
        assert!(finish_line_emphasis("a  b", vec![(1, 3)]).is_empty());
    }

    fn fixture(name: &str) -> DiffDocument {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/patches")
                .join(name),
        )
        .expect("the fixture");
        parse_unified_diff(&text)
    }

    fn rows(file: &DiffFile) -> Vec<(DiffLineKind, Option<usize>, Option<usize>, &str)> {
        file.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|row| (row.kind, row.old_line, row.new_line, row.content.as_str()))
            .collect()
    }

    /// The `-- ` and version under a `git format-patch` email's diff are
    /// its signature, not a deleted line, and the message above isn't
    /// diff content either.
    #[test]
    fn a_format_patch_email_ends_its_hunk_at_the_signature() {
        let document = fixture("format-patch.patch");
        assert_eq!(document.files.len(), 1);
        let file = &document.files[0];
        assert_eq!(file.path, "list.txt");
        assert_eq!((file.additions, file.deletions), (2, 1));
        let rows = rows(file);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[4], (DiffLineKind::Addition, None, Some(4), "delta"));
    }

    /// A series: three emails, the cover letter's diffstat skipped, the
    /// copy read as its two paths.
    #[test]
    fn a_patch_series_reads_each_patch_and_its_copy() {
        let document = fixture("series.mbox");
        let paths: Vec<_> = document
            .files
            .iter()
            .map(|file| (file.old_path.as_deref(), file.path.as_str(), file.status))
            .collect();
        assert_eq!(
            paths,
            [
                (Some("list.txt"), "list.txt", DiffFileStatus::Modified),
                (Some("list.txt"), "copy.txt", DiffFileStatus::Renamed),
            ]
        );
        let copy = rows(&document.files[1]);
        assert_eq!(copy.len(), 4);
        assert_eq!(copy[0], (DiffLineKind::Context, Some(2), Some(2), "BETA"));
        assert_eq!(document.total_deletions, 1);
    }

    #[test]
    fn a_plain_diff_u_takes_the_new_side_s_name() {
        let document = fixture("diff-u.diff");
        let file = &document.files[0];
        assert_eq!(file.path, "notes.txt");
        assert_eq!(file.old_path.as_deref(), Some("notes.txt"));
        assert_eq!(file.status, DiffFileStatus::Modified);
        assert_eq!((file.additions, file.deletions), (2, 1));
    }

    /// `diff -ruN`: its `diff -ruN …` lines between files, the folders
    /// stripped, and the files `-N` wrote as empty read as added and
    /// deleted.
    #[test]
    fn a_recursive_plain_diff_reads_each_file() {
        let document = fixture("diff-ruN.diff");
        let files: Vec<_> = document
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.status,
                    file.additions,
                    file.deletions,
                )
            })
            .collect();
        assert_eq!(
            files,
            [
                ("notes.txt", DiffFileStatus::Modified, 2, 1),
                ("src/added.rs", DiffFileStatus::Added, 1, 0),
                ("src/gone.rs", DiffFileStatus::Deleted, 0, 1),
                ("src/main.rs", DiffFileStatus::Modified, 1, 1),
            ]
        );
    }

    /// svn: `Index:` and `===` headers, `(revision N)` after the names,
    /// which keep their folder (both sides have the same one).
    #[test]
    fn an_svn_diff_keeps_its_paths_whole() {
        let document = fixture("svn.diff");
        let paths: Vec<_> = document
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["trunk/src/parse.c", "trunk/README"]);
        assert_eq!(
            rows(&document.files[0])[3],
            (
                DiffLineKind::Deletion,
                Some(13),
                None,
                "    if (line == NULL)"
            )
        );
        assert_eq!(rows(&document.files[1]).len(), 3);
    }

    /// hg: its export header, `diff -r` lines, a blank context line, and an
    /// added file against `/dev/null` with a date after it.
    #[test]
    fn an_hg_export_reads_each_file() {
        let document = fixture("hg.diff");
        let files: Vec<_> = document
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.status,
                    file.additions,
                    file.deletions,
                )
            })
            .collect();
        assert_eq!(
            files,
            [
                ("bugs.py", DiffFileStatus::Modified, 2, 1),
                ("moth.txt", DiffFileStatus::Added, 1, 0),
            ]
        );
        assert_eq!(rows(&document.files[0]).len(), 5);
    }

    /// `git show --cc` of a merge: shown against the first parent, with a
    /// note saying so; the line only the second parent had is left out.
    #[test]
    fn a_combined_diff_shows_against_its_first_parent() {
        let document = fixture("combined.diff");
        assert_eq!(document.files.len(), 1);
        let file = &document.files[0];
        assert_eq!(file.path, "list.txt");
        assert_eq!(
            rows(file),
            [
                (
                    DiffLineKind::Note,
                    None,
                    None,
                    "Combined diff of 2 parents, shown against the first"
                ),
                (DiffLineKind::Context, Some(1), Some(1), "alpha"),
                (DiffLineKind::Context, Some(2), Some(2), "BETA"),
                (DiffLineKind::Context, Some(3), Some(3), "gamma"),
                (DiffLineKind::Context, Some(4), Some(4), "delta"),
                (DiffLineKind::Addition, None, Some(5), "zeta"),
            ]
        );
    }

    #[test]
    fn a_crlf_patch_reads_without_its_carriage_returns() {
        let document = fixture("crlf.patch");
        let file = &document.files[0];
        assert_eq!(file.path, "win.txt");
        assert_eq!(
            rows(file),
            [
                (DiffLineKind::Context, Some(1), Some(1), "first"),
                (DiffLineKind::Deletion, Some(2), None, "second"),
                (DiffLineKind::Addition, None, Some(2), "2nd"),
            ]
        );
    }

    /// A `\ No newline` marker after a hunk's last counted line stays in it.
    #[test]
    fn a_no_newline_marker_after_the_counts_stays_in_the_hunk() {
        let document = parse_unified_diff(
            "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\nnot diff\n",
        );
        let file = &document.files[0];
        assert_eq!(file.path, "f");
        assert_eq!(rows(file).len(), 4);
        assert_eq!(rows(file)[3].0, DiffLineKind::Note);
    }
}
