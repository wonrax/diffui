//! Host for `@pierre/highlights`, a syntax highlighter whose lexers are written
//! in WebAssembly text. `build.rs` fetches the module and translates it to C
//! with wabt's `wasm2c`, so it runs as native code with no wasm runtime; this
//! file speaks the module's control-block protocol (see
//! `lib/highlighter.ts` and `src/memory.wat` upstream).

mod tables;

use std::{cell::RefCell, ptr::NonNull};

use crate::model::{SyntaxKind, SyntaxSpan};

const PAGE: usize = 65536;
/// Output mode 3: `(end_utf16, token_id)` records split at line terminators.
const MODE_LINE_RECORDS: u8 = 3;
/// The record id marking a line terminator; its end includes the terminator.
const LINE_END: u32 = u32::MAX;
/// Bytes past the input the lexers may read ahead into.
const LOOKAHEAD: usize = 96;
/// A thread's instance keeps its linear memory (it only grows), so one that
/// lexed a large file is dropped instead of sitting on that memory.
const RETAIN_LIMIT: usize = 8 << 20;

/// A highlights language id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language(u8);

/// The language for `path`, by file name first, then extension.
pub fn detect_language(path: &str) -> Option<Language> {
    let name = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    let by_name = match name.as_str() {
        "cmakelists.txt" => Some("cmake"),
        "gnumakefile" => Some("makefile"),
        "gemfile" | "rakefile" | "podfile" | "brewfile" | "vagrantfile" => Some("ruby"),
        "pkgbuild" | ".bashrc" | ".bash_profile" | ".profile" | ".zshrc" | ".zprofile"
        | ".zshenv" | ".envrc" => Some("bash"),
        "flake.lock" => Some("json"),
        "cargo.lock" => Some("toml"),
        _ => None,
    };
    // A name without a dot is its own extension:
    // `Dockerfile`, `Makefile`.
    let ext = name.rsplit('.').next()?;
    let alias = by_name.unwrap_or(match ext {
        "py3" | "pyi" | "pyw" | "bzl" | "bazel" | "star" => "python",
        "rake" | "gemspec" => "ruby",
        "mysql" | "postgres" | "postgresql" | "sqlite" => "sql",
        "rkt" | "ss" => "scheme",
        "xhtml" => "html",
        "plist" | "csproj" | "fsproj" | "vcxproj" | "xaml" | "xib" | "storyboard" => "xml",
        // No lexer of their own; the nearest one still gets comments,
        // strings and keys right.
        "fish" => "bash",
        "ini" | "cfg" | "conf" | "editorconfig" | "gitconfig" => "toml",
        other => other,
    });
    tables::LANGUAGES
        .binary_search_by(|(name, _)| name.cmp(&alias))
        .ok()
        .map(|index| Language(tables::LANGUAGES[index].1))
        .filter(|language| language.0 != 0)
}

/// Syntax spans over `source`, as absolute byte ranges in order. Spans never
/// cross a line terminator. Empty if the module traps.
pub fn highlight(language: Language, source: &str) -> Vec<SyntaxSpan> {
    thread_local! {
        static INSTANCE: RefCell<Option<Instance>> = const { RefCell::new(None) };
    }
    INSTANCE.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Instance::new();
        }
        let Some(instance) = slot.as_mut() else {
            return Vec::new();
        };
        let spans = instance.highlight(language, source);
        if spans.is_none() || instance.memory().len() > RETAIN_LIMIT {
            *slot = None;
        }
        spans.unwrap_or_default()
    })
}

fn token_kind(id: u32) -> Option<SyntaxKind> {
    let name = *tables::TOKENS.get(id as usize)?;
    let (root, _) = name.split_once('.').unwrap_or((name, ""));
    Some(match root {
        "comment" => SyntaxKind::Comment,
        "string" => SyntaxKind::String,
        "text" if name == "text.literal" => SyntaxKind::String,
        "number" | "boolean" | "constant" | "enum" | "variant" => SyntaxKind::Number,
        "keyword" | "operator" | "preproc" => SyntaxKind::Keyword,
        "function" | "constructor" => SyntaxKind::Function,
        "type" | "namespace" => SyntaxKind::Type,
        "variable" if name == "variable.special" => SyntaxKind::Type,
        "variable" if name == "variable.parameter" => SyntaxKind::Property,
        "property" | "attribute" | "tag" | "label" | "selector" => SyntaxKind::Property,
        "punctuation" => SyntaxKind::Punctuation,
        _ => return None,
    })
}

mod ffi {
    #[repr(C)]
    pub struct Handle {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        pub fn diffui_hl_new() -> *mut Handle;
        pub fn diffui_hl_free(handle: *mut Handle);
        pub fn diffui_hl_memory(handle: *mut Handle, len: *mut u64) -> *mut u8;
        pub fn diffui_hl_grow(handle: *mut Handle, pages: u32) -> i32;
        pub fn diffui_hl_highlight(handle: *mut Handle) -> i32;
    }
}

/// `ID_Start` / `ID_Continue` for the one UTF-8 code point at `bytes`, asked
/// by the ECMAScript lexers for non-ASCII identifiers.
#[unsafe(no_mangle)]
extern "C" fn diffui_highlights_is_id(bytes: *const u8, len: u32, cont: u32) -> u32 {
    // SAFETY: the shim bounds-checked `bytes..bytes + len` against the
    // instance's linear memory.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
    let Some(c) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.chars().next())
    else {
        return 0;
    };
    let yes = if cont != 0 {
        unicode_ident::is_xid_continue(c) || matches!(c, '\u{200c}' | '\u{200d}')
    } else {
        unicode_ident::is_xid_start(c)
    };
    yes as u32
}

/// One module instance. Not `Send`: the wasm2c runtime keeps its trap state
/// in thread-locals, so an instance stays on the thread that made it.
struct Instance(NonNull<ffi::Handle>);

impl Instance {
    fn new() -> Option<Self> {
        // SAFETY: no preconditions; null on allocation failure.
        NonNull::new(unsafe { ffi::diffui_hl_new() }).map(Self)
    }

    fn memory(&mut self) -> &mut [u8] {
        let mut len = 0u64;
        // SAFETY: the handle is live; the memory is valid for `len` bytes
        // until the next call into the module.
        unsafe {
            let data = ffi::diffui_hl_memory(self.0.as_ptr(), &mut len);
            std::slice::from_raw_parts_mut(data, len as usize)
        }
    }

    /// `None` on a trap or when the memory can't grow to fit `source`.
    fn highlight(&mut self, language: Language, source: &str) -> Option<Vec<SyntaxSpan>> {
        let len = source.len();
        let input_len = u32::try_from(len).ok()?;
        let needed = 1 + (len + LOOKAHEAD).div_ceil(PAGE);
        let have = self.memory().len() / PAGE;
        if needed > have {
            let grow = u32::try_from(needed - have).ok()?;
            // SAFETY: the handle is live.
            if unsafe { ffi::diffui_hl_grow(self.0.as_ptr(), grow) } != 0 {
                return None;
            }
        }

        let memory = self.memory();
        memory[0] = language.0;
        memory[1] = MODE_LINE_RECORDS;
        memory[2..6].copy_from_slice(&input_len.to_le_bytes());
        memory[PAGE..PAGE + len].copy_from_slice(source.as_bytes());
        memory[PAGE + len] = 0;

        // SAFETY: the handle is live; a trap unwinds inside the shim.
        let trap = unsafe { ffi::diffui_hl_highlight(self.0.as_ptr()) };
        if trap != 0 {
            tracing::warn!(trap, ?language, "highlights trapped");
            return None;
        }

        let memory = self.memory();
        let read = |at: usize| u32::from_le_bytes(memory[at..at + 4].try_into().unwrap()) as usize;
        let (start, out_len) = (read(6), read(10));
        let records = memory.get(start..start.checked_add(out_len)?)?;
        Some(records_to_spans(records, source))
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the handle came from `diffui_hl_new` and is freed once.
        unsafe { ffi::diffui_hl_free(self.0.as_ptr()) }
    }
}

/// Turn `(end_utf16, token_id)` records into byte spans, walking `source`
/// alongside to convert the UTF-16 ends.
fn records_to_spans(records: &[u8], source: &str) -> Vec<SyntaxSpan> {
    let bytes = source.as_bytes();
    let mut spans = Vec::with_capacity(records.len() / 16);
    let (mut utf16, mut byte, mut start) = (0usize, 0usize, 0usize);
    let (records, _) = records.as_chunks::<8>();
    for &[e0, e1, e2, e3, i0, i1, i2, i3] in records {
        let end = u32::from_le_bytes([e0, e1, e2, e3]) as usize;
        let id = u32::from_le_bytes([i0, i1, i2, i3]);
        while utf16 < end && byte < bytes.len() {
            let (width, units) = match bytes[byte] {
                0x00..=0x7f => (1, 1),
                0xc0..=0xdf => (2, 1),
                0xe0..=0xef => (3, 1),
                _ => (4, 2),
            };
            byte += width;
            utf16 += units;
        }
        // Whitespace after a token extends its record; keep it out of the
        // span, as the old highlighter did.
        let mut end = byte;
        while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        if id != LINE_END
            && end > start
            && let Some(kind) = token_kind(id)
        {
            spans.push(SyntaxSpan { start, end, kind });
        }
        start = byte;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(path: &str, source: &str) -> Vec<(String, SyntaxKind)> {
        let language = detect_language(path).expect("language");
        highlight(language, source)
            .into_iter()
            .map(|span| (source[span.start..span.end].to_owned(), span.kind))
            .collect()
    }

    #[test]
    fn detects_by_name_and_extension() {
        let id = |path| detect_language(path).map(|language| language.0);
        assert_eq!(id("src/main.rs"), id("x.rust"));
        assert_eq!(id("a/Dockerfile"), id("x.dockerfile"));
        assert_eq!(id("CMakeLists.txt"), id("x.cmake"));
        assert_eq!(id("Makefile"), id("x.mk"));
        assert_eq!(id("lib/foo.TSX"), id("x.tsx"));
        assert_eq!(id("notes.txt"), None);
        assert_eq!(id("LICENSE"), None);
    }

    #[test]
    fn tables_are_sorted() {
        assert!(tables::LANGUAGES.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn rust_tokens() {
        let spans = kinds("x.rs", "// hi\nfn main() { let s = \"x\"; }\n");
        assert!(
            spans.contains(&("// hi".into(), SyntaxKind::Comment)),
            "{spans:?}"
        );
        assert!(
            spans.contains(&("fn".into(), SyntaxKind::Keyword)),
            "{spans:?}"
        );
        assert!(
            spans.contains(&("\"x\"".into(), SyntaxKind::String)),
            "{spans:?}"
        );
    }

    /// Non-ASCII text before a token: the record ends are UTF-16, the spans
    /// must land on the right bytes.
    #[test]
    fn utf16_ends_map_to_bytes() {
        let source = "const é = \"🦀\"; // ü\n";
        let spans = kinds("x.ts", source);
        assert!(
            spans.contains(&("\"🦀\"".into(), SyntaxKind::String)),
            "{spans:?}"
        );
        assert!(
            spans.contains(&("// ü".into(), SyntaxKind::Comment)),
            "{spans:?}"
        );
    }

    #[test]
    fn spans_stay_on_their_line() {
        let source = "/* a\nb */\nx\r\n";
        for span in highlight(detect_language("x.c").unwrap(), source) {
            assert!(!source[span.start..span.end].contains('\n'), "{span:?}");
            assert!(!source[span.start..span.end].contains('\r'), "{span:?}");
        }
    }

    /// One instance per thread, reused across calls and languages, and
    /// replaced after a file that grew its memory past the limit.
    #[test]
    fn reuses_and_replaces_the_instance() {
        let big = "let x = 1;\n".repeat(1 << 20);
        let rust = detect_language("x.rs").unwrap();
        assert!(!highlight(rust, &big).is_empty());
        let spans = kinds("x.py", "def f(): pass\n");
        assert!(
            spans.contains(&("def".into(), SyntaxKind::Keyword)),
            "{spans:?}"
        );
    }
}
