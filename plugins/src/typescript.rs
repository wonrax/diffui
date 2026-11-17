//! TypeScript modules: the types are erased in place.
//!
//! This is type stripping, not compiling: annotations, interfaces, generics
//! and `import type` are replaced by spaces, and what is left runs as
//! written. Every remaining character keeps its line and column, so the
//! engine's stack traces point into the file the author wrote and no source
//! map is needed. Syntax that would need code generated for it (`enum`,
//! `namespace`, parameter properties) is refused; that is TypeScript's own
//! `erasableSyntaxOnly` subset.

use swc_next_type_strip::{Lang, Options, Severity, strip_types};

/// Whether a module name is TypeScript's to strip.
pub(crate) fn is_typescript(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".mts")
}

/// Why a module's types could not be erased, with where: 1-based line and
/// character column.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Problem {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

/// Blank out the type syntax of a TypeScript module.
pub(crate) fn erase_types(source: &str) -> Result<String, Problem> {
    let options = Options {
        lang: Lang::Ts,
        ..Options::default()
    };
    let stripped = strip_types(source, options);
    let Some(problem) = stripped
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.severity == Severity::Error)
    else {
        return Ok(stripped.code.into_owned());
    };
    let message = if problem.message.contains("erasableSyntaxOnly") {
        "enum, namespace, parameter properties, `import =` and `<T>value` casts are not supported: \
         diffui erases types and runs the rest as written"
            .to_string()
    } else {
        match &problem.help {
            Some(help) => format!("{} ({help})", problem.message),
            None => problem.message.to_string(),
        }
    };
    let before = source.get(..problem.span.start as usize).unwrap_or(source);
    Err(Problem {
        message,
        line: before.matches('\n').count() + 1,
        col: before.rsplit('\n').next().unwrap_or("").chars().count() + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_go_and_everything_else_stays_where_it_was() {
        let source = "import type { Repo } from \"diffui\";\nimport { type Checks, forge } from \"diffui\";\ninterface Options { limit?: number }\nconst pick = <T>(items: T[], at: number): T => items[at]!;\nexport const first = pick([1, 2] as number[], 0) satisfies number;\n";
        let erased = erase_types(source).unwrap();
        assert_eq!(erased.len(), source.len());
        let lines: Vec<&str> = erased.lines().collect();
        assert_eq!(lines.len(), 5);
        // A type-only statement leaves at most its semicolon.
        assert_eq!(lines[0].trim().trim_matches(';'), "");
        assert_eq!(
            lines[1].split_whitespace().collect::<Vec<_>>().join(" "),
            "import { forge } from \"diffui\";"
        );
        assert_eq!(lines[2].trim().trim_matches(';'), "");
        // What survives sits at the column it had.
        assert_eq!(
            lines[3].find("items[at]"),
            source.lines().nth(3).unwrap().find("items[at]")
        );
        assert_eq!(
            lines[4].find("pick("),
            source.lines().nth(4).unwrap().find("pick(")
        );
        assert!(!erased.contains("number") && !erased.contains("satisfies"));
    }

    #[test]
    fn what_needs_a_compiler_is_refused_with_its_place() {
        let problem = erase_types("const a = 1;\n  enum Colour { Red }\n").unwrap_err();
        assert_eq!((problem.line, problem.col), (2, 3));
        assert!(
            problem
                .message
                .starts_with("enum, namespace, parameter properties")
        );

        let problem = erase_types("const broken: number = ;\n").unwrap_err();
        assert_eq!(
            problem,
            Problem {
                message: "Unexpected token ';' (Expected an expression)".into(),
                line: 1,
                col: 24
            }
        );
    }
}
