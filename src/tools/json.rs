//! Order-preserving JSON pretty printer.
//!
//! Validation runs through `serde_json`, so failures carry line and column.
//! Formatting then re-indents the validated token stream instead of
//! round-tripping through `serde_json::Value`, which keeps object key order,
//! duplicate keys, number literals (`1e2`, integers beyond `u64`), and string
//! escapes exactly as written — a formatter lays out a payload, it does not
//! rewrite it. Indentation is two spaces, matching Devcroft's JSON output.

/// Two-space indent unit.
const INDENT: &str = "  ";

/// Pretty-print `input`. Empty or whitespace-only input formats to an empty
/// string: there is nothing to lay out, and an empty editor is not an error.
pub(crate) fn format(input: &str) -> Result<String, String> {
    if input.trim().is_empty() {
        return Ok(String::new());
    }
    serde_json::from_str::<serde::de::IgnoredAny>(input)
        .map_err(|error| format!("Invalid JSON: {error}"))?;
    Ok(reindent(input))
}

/// One lexical element of a *validated* document. Strings and atoms keep
/// their original text; the structural tokens only drive layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token<'a> {
    /// A string literal, quotes and escapes included.
    Text(&'a str),
    /// A number, `true`, `false`, or `null`.
    Atom(&'a str),
    Open(char),
    Close(char),
    Comma,
    Colon,
}

/// Split validated JSON into tokens. Every byte that is not a string,
/// delimiter, or whitespace is part of an atom; that is exactly how the
/// validator sees numbers and literals.
fn tokenize(input: &str) -> Vec<Token<'_>> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => index += 1,
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        // The escape's payload is ASCII (or `\uXXXX`), so
                        // stepping two bytes stays on a char boundary.
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                tokens.push(Token::Text(&input[start..index]));
            }
            b'{' | b'[' => {
                tokens.push(Token::Open(bytes[index] as char));
                index += 1;
            }
            b'}' | b']' => {
                tokens.push(Token::Close(bytes[index] as char));
                index += 1;
            }
            b',' => {
                tokens.push(Token::Comma);
                index += 1;
            }
            b':' => {
                tokens.push(Token::Colon);
                index += 1;
            }
            _ => {
                let start = index;
                while index < bytes.len()
                    && !matches!(
                        bytes[index],
                        b'"' | b'{'
                            | b'}'
                            | b'['
                            | b']'
                            | b','
                            | b':'
                            | b' '
                            | b'\t'
                            | b'\n'
                            | b'\r'
                    )
                {
                    index += 1;
                }
                tokens.push(Token::Atom(&input[start..index]));
            }
        }
    }
    tokens
}

/// Lay the token stream out with one value per line. An empty container
/// (`{}`/`[]`) stays on one line, and nothing is appended after the last
/// token.
fn reindent(input: &str) -> String {
    let tokens = tokenize(input);
    let mut out = String::with_capacity(input.len() + input.len() / 4);
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Open(brace) => {
                out.push(*brace);
                // An opening brace directly followed by its close is empty:
                // no line break and no depth change.
                if !matches!(tokens.get(index + 1), Some(Token::Close(_))) {
                    depth += 1;
                    push_line_break(&mut out, depth);
                }
            }
            Token::Close(brace) => {
                if !matches!(
                    index
                        .checked_sub(1)
                        .and_then(|previous| tokens.get(previous)),
                    Some(Token::Open(_))
                ) {
                    depth = depth.saturating_sub(1);
                    push_line_break(&mut out, depth);
                }
                out.push(*brace);
            }
            Token::Comma => {
                out.push(',');
                push_line_break(&mut out, depth);
            }
            Token::Colon => out.push_str(": "),
            Token::Text(text) | Token::Atom(text) => out.push_str(text),
        }
    }
    out
}

fn push_line_break(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_objects_and_arrays_with_two_space_indent() {
        assert_eq!(
            format(r#"{"a":1,"b":[1,2]}"#).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ]\n}"
        );
        assert_eq!(
            format(r#"[{"a":1}]"#).unwrap(),
            "[\n  {\n    \"a\": 1\n  }\n]"
        );
        assert_eq!(
            format("{\"a\":{\"b\":{\"c\":null}}}").unwrap(),
            "{\n  \"a\": {\n    \"b\": {\n      \"c\": null\n    }\n  }\n}"
        );
    }

    #[test]
    fn keeps_empty_containers_on_one_line() {
        assert_eq!(format("{}").unwrap(), "{}");
        assert_eq!(format("[]").unwrap(), "[]");
        assert_eq!(
            format(r#"{"a":{},"b":[],"c":[[]]}"#).unwrap(),
            "{\n  \"a\": {},\n  \"b\": [],\n  \"c\": [\n    []\n  ]\n}"
        );
    }

    #[test]
    fn preserves_key_order_duplicates_and_literals() {
        // Key order and duplicate keys survive; values are not rebuilt.
        assert_eq!(
            format(r#"{"z":1,"a":2,"z":3}"#).unwrap(),
            "{\n  \"z\": 1,\n  \"a\": 2,\n  \"z\": 3\n}"
        );
        // Number literals keep their spelling, including exponents and
        // integers beyond `u64`; string escapes stay escaped.
        assert_eq!(
            format(r#"[1e2,123456789012345678901234567890,1.0]"#).unwrap(),
            "[\n  1e2,\n  123456789012345678901234567890,\n  1.0\n]"
        );
        assert_eq!(
            format(r#"{"s":"a\"b\u0041\\","u":"\u00e9"}"#).unwrap(),
            "{\n  \"s\": \"a\\\"b\\u0041\\\\\",\n  \"u\": \"\\u00e9\"\n}"
        );
    }

    #[test]
    fn braces_and_colons_inside_strings_do_not_change_layout() {
        assert_eq!(
            format(r#"{"a":"}{:,","b":1}"#).unwrap(),
            "{\n  \"a\": \"}{:,\",\n  \"b\": 1\n}"
        );
        assert_eq!(
            format("{\"key\\\"with\\\"quotes\":[true,false]}").unwrap(),
            "{\n  \"key\\\"with\\\"quotes\": [\n    true,\n    false\n  ]\n}"
        );
    }

    #[test]
    fn accepts_any_already_valid_whitespace() {
        assert_eq!(format("  \n\t ").unwrap(), "");
        assert_eq!(format("{\"a\"  :\n1}").unwrap(), "{\n  \"a\": 1\n}");
        assert_eq!(format("42").unwrap(), "42");
        assert_eq!(format(r#""hi""#).unwrap(), r#""hi""#);
        assert_eq!(format("null").unwrap(), "null");
    }

    #[test]
    fn reports_invalid_json_with_position() {
        let error = format(r#"{"a":}"#).unwrap_err();
        assert!(error.starts_with("Invalid JSON: "), "{error}");
        assert!(error.contains("line 1 column 6"), "{error}");
        // Empty input is not an error; malformed input always is.
        assert!(format("{").is_err());
        assert!(format("").is_ok());
        assert!(format("[1,2,]").is_err());
        assert!(format("{\"a\":1} trailing").is_err());
    }

    #[test]
    fn formatting_is_idempotent() {
        let once = format(r#"{"a":[1,{"b":[]}],"c":"x"}"#).unwrap();
        assert_eq!(format(&once).unwrap(), once);
    }

    #[test]
    fn output_parses_to_the_same_document() {
        let input = r#"{"a":[1,2,{"b":"c"}],"d":null}"#;
        let expected: serde_json::Value = serde_json::from_str(input).unwrap();
        let actual: serde_json::Value = serde_json::from_str(&format(input).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }
}
