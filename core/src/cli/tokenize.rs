//! Shell-style word splitting with byte spans.
//!
//! The rules are POSIX `sh` word splitting without the shell: whitespace
//! separates words, `'…'` is literal, `"…"` honours `\"`, `\\`, `\$` and
//! `` \` ``, and a backslash outside quotes escapes the next character. Every
//! other character is literal, so an unquoted revset such as `mine()&~empty()`
//! stays one word even though a real shell would choke on the parentheses.

use std::ops::Range;

/// One word of the input after quote removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The word as the program receives it.
    pub value: String,
    /// The word's bytes in the input, quotes included.
    pub span: Range<usize>,
    /// `offsets[i]` is the input offset of the character that produced value
    /// byte `i`; `offsets[value.len()]` is where the word's content ends
    /// (before a closing quote). Lets a range of the value be mapped back
    /// onto the input, which is how errors and completions find their span.
    offsets: Vec<usize>,
    /// The word is still inside a quote at the end of the input.
    pub unterminated: Option<char>,
}

impl Token {
    /// A word not typed yet, at `at`: where completion inserts one.
    pub(crate) fn empty_at(at: usize) -> Self {
        Self {
            value: String::new(),
            span: at..at,
            offsets: vec![at],
            unterminated: None,
        }
    }

    /// Map `range`, byte offsets into [`value`](Self::value), onto the input.
    pub fn input_range(&self, range: Range<usize>) -> Range<usize> {
        let at = |i: usize| self.offsets[i.min(self.value.len())];
        at(range.start)..at(range.end)
    }

    /// The value offset an input `cursor` falls at: how many value bytes come
    /// from input before it.
    pub fn value_offset(&self, cursor: usize) -> usize {
        self.offsets[..self.value.len()]
            .iter()
            .take_while(|&&offset| offset < cursor)
            .count()
    }

    /// Whether the word was written with any quoting or escaping.
    pub fn is_quoted(&self) -> bool {
        self.span.len() != self.value.len()
    }
}

/// A quote left open at the end of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnterminatedQuote {
    /// From the opening quote to the end of the input.
    pub span: Range<usize>,
    pub quote: char,
}

/// Split `input` into words. An unterminated quote still produces its word
/// (marked [`Token::unterminated`]) so completion can work inside it; use
/// [`check_terminated`] where it has to be an error.
pub fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    let mut current: Option<Token> = None;
    let mut quote: Option<(char, usize)> = None;

    fn push(token: &mut Token, at: usize, ch: char) {
        for _ in 0..ch.len_utf8() {
            token.offsets.push(at);
        }
        token.value.push(ch);
    }

    while let Some((at, ch)) = chars.next() {
        if quote.is_none() && ch.is_whitespace() {
            if let Some(mut token) = current.take() {
                token.span.end = at;
                token.offsets.push(at);
                tokens.push(token);
            }
            continue;
        }
        let token = current.get_or_insert_with(|| Token {
            value: String::new(),
            span: at..at,
            offsets: Vec::new(),
            unterminated: None,
        });
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some((ch, at)),
            (None, '\\') => match chars.next() {
                Some((_, escaped)) => push(token, at, escaped),
                None => push(token, at, '\\'),
            },
            (Some(('\'', _)), '\'') | (Some(('"', _)), '"') => {
                quote = None;
                // A closing quote ends the content; if the word continues,
                // its next character overrides this.
                if chars.peek().is_none_or(|(_, next)| next.is_whitespace()) {
                    token.span.end = at + 1;
                    token.offsets.push(at);
                    tokens.push(current.take().expect("the token was just used"));
                }
            }
            (Some(('"', _)), '\\') => match chars.peek() {
                Some(&(_, next @ ('"' | '\\' | '$' | '`'))) => {
                    chars.next();
                    push(token, at, next);
                }
                Some(&(_, '\n')) => {
                    chars.next();
                }
                _ => push(token, at, '\\'),
            },
            _ => push(token, at, ch),
        }
    }
    if let Some(mut token) = current.take() {
        token.span.end = input.len();
        token.offsets.push(input.len());
        token.unterminated = quote.map(|(quote, _)| quote);
        tokens.push(token);
    }
    tokens
}

/// The first quote left open, if any.
pub fn check_terminated(input: &str, tokens: &[Token]) -> Result<(), UnterminatedQuote> {
    match tokens.last() {
        Some(token) if token.unterminated.is_some() => {
            let quote = token.unterminated.expect("checked above");
            // The opening quote is the last unescaped one of its kind; the
            // word's content offsets can't see it, so search the raw text.
            let start = input[token.span.clone()]
                .rfind(quote)
                .map_or(token.span.start, |i| token.span.start + i);
            Err(UnterminatedQuote {
                span: start..input.len(),
                quote,
            })
        }
        _ => Ok(()),
    }
}

/// Quote `arg` for a POSIX shell only when it needs it: plain words stay
/// bare, anything else goes in single quotes (with `'` written as `'\''`).
/// [`tokenize`] reads the result back to `arg`.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b',' | b'-' | b'.' | b'/' | b':' | b'@' | b'_' | b'+' | b'=' | b'%'
                )
        });
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(input: &str) -> Vec<String> {
        tokenize(input).into_iter().map(|t| t.value).collect()
    }

    #[test]
    fn splits_on_whitespace_and_removes_quotes() {
        assert_eq!(
            values(r#"jj rebase -s xyz -o 'trunk()'  "a b"c\ d"#),
            ["jj", "rebase", "-s", "xyz", "-o", "trunk()", "a bc d"]
        );
        assert_eq!(
            values(r#"say "a\"b\\c\n" 'it'\''s'"#),
            ["say", r#"a"b\c\n"#, "it's"]
        );
        assert!(values("   ").is_empty());
        assert_eq!(values("''"), [""]);
    }

    #[test]
    fn spans_cover_the_quotes_and_offsets_map_back() {
        let input = r#"-o 'trunk()' --to="a b""#;
        let tokens = tokenize(input);
        assert_eq!(tokens[1].span, 3..12);
        assert_eq!(&input[tokens[1].input_range(0..5)], "trunk");
        assert_eq!(tokens[1].input_range(0..7), 4..11);
        assert_eq!(tokens[2].value, "--to=a b");
        // The value after `=` maps inside the quotes.
        assert_eq!(&input[tokens[2].input_range(5..8)], "a b");
        assert!(tokens[1].is_quoted() && !tokens[0].is_quoted());
    }

    #[test]
    fn value_offset_counts_bytes_before_the_cursor() {
        let tokens = tokenize("-r 'abc'");
        let token = &tokens[1];
        assert_eq!(token.value_offset(3), 0); // before the quote
        assert_eq!(token.value_offset(5), 1);
        assert_eq!(token.value_offset(7), 3); // before the closing quote
        assert_eq!(token.value_offset(8), 3);
    }

    #[test]
    fn unterminated_quotes_still_tokenize_and_are_reported() {
        let input = "jj log -r 'mine() & ";
        let tokens = tokenize(input);
        assert_eq!(tokens.last().unwrap().value, "mine() & ");
        assert_eq!(tokens.last().unwrap().unterminated, Some('\''));
        let error = check_terminated(input, &tokens).unwrap_err();
        assert_eq!(error.span, 10..input.len());
        assert!(check_terminated("a 'b'", &tokenize("a 'b'")).is_ok());
    }

    #[test]
    fn shell_quote_round_trips() {
        for arg in [
            "plain",
            "trunk()",
            "a b",
            "it's",
            "",
            "x@origin",
            "--onto=@-",
            "ü",
        ] {
            let quoted = shell_quote(arg);
            assert_eq!(values(&quoted), [arg], "{quoted}");
        }
        assert_eq!(shell_quote("abc-1"), "abc-1");
        assert_eq!(shell_quote("trunk()"), "'trunk()'");
    }
}
