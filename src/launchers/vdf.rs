//! Parser for Valve's text KeyValues format (`.vdf`, `.acf`).
//!
//! Steam's own parser is lenient, so this one is too: keys are matched
//! case-insensitively, duplicate keys are kept in order (Steam takes the
//! first), `//` comments and `[$PLATFORM]` conditionals are skipped, and an
//! unquoted token runs to the next whitespace or brace.
//!
//! Only the text format lives here. `shortcuts.vdf` is binary and gets its own
//! module when non-Steam shortcuts are supported.

use std::fmt;

/// A parse failure, with the 1-based line it happened on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {kind}")]
pub struct Error {
    /// 1-based line number.
    pub line: u32,
    /// What went wrong.
    pub kind: ErrorKind,
}

/// The kinds of malformed input the parser rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// A quoted string had no closing quote.
    UnterminatedString,
    /// A `[$COND]` conditional had no closing bracket.
    UnterminatedConditional,
    /// Input ended inside an object.
    UnexpectedEof,
    /// A `}` appeared with no open object.
    UnexpectedBrace,
    /// A value was expected but something else appeared.
    ExpectedValue,
    /// The document did not start with `"key" { ... }`.
    ExpectedRoot,
    /// Objects were nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::UnterminatedString => "unterminated string",
            Self::UnterminatedConditional => "unterminated [$conditional]",
            Self::UnexpectedEof => "unexpected end of input",
            Self::UnexpectedBrace => "unexpected '}'",
            Self::ExpectedValue => "expected a value",
            Self::ExpectedRoot => "expected a top-level \"key\" { ... }",
            Self::TooDeep => "objects nested too deeply",
        };
        f.write_str(s)
    }
}

/// Either a string or a nested object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A leaf string. Numbers are strings in this format.
    Str(String),
    /// A nested object.
    Obj(Object),
}

impl Value {
    /// The string, if this is a leaf.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Obj(_) => None,
        }
    }

    /// The object, if this is not a leaf.
    pub fn as_obj(&self) -> Option<&Object> {
        match self {
            Self::Obj(o) => Some(o),
            Self::Str(_) => None,
        }
    }
}

/// An ordered list of key/value pairs, allowing duplicate keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Object {
    entries: Vec<(String, Value)>,
}

impl Object {
    /// Every entry, in file order.
    pub fn entries(&self) -> &[(String, Value)] {
        &self.entries
    }

    /// The first value for `key`, matched case-insensitively.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    /// The first string value for `key`.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    /// The first object value for `key`.
    pub fn get_obj(&self, key: &str) -> Option<&Object> {
        self.get(key).and_then(Value::as_obj)
    }

    /// The first value for `key` parsed as a `u64`.
    ///
    /// Steam writes every number as a decimal string; a value that does not
    /// parse is treated as absent, which is what Steam does too.
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get_str(key).and_then(|s| s.trim().parse().ok())
    }

    /// The first value for `key` parsed as a `u32`.
    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get_str(key).and_then(|s| s.trim().parse().ok())
    }

    /// Walks a chain of nested objects, e.g. `path(&["HKCU", "Software"])`.
    pub fn path(&self, keys: &[&str]) -> Option<&Object> {
        let mut cur = self;
        for key in keys {
            cur = cur.get_obj(key)?;
        }
        Some(cur)
    }

    /// Depth-first search for the first string under `key`, at any depth.
    ///
    /// `registry.vdf` nests its keys differently across Steam versions, so
    /// looking a scalar up by name is more robust than a fixed path.
    pub fn find_str(&self, key: &str) -> Option<&str> {
        self.find_str_depth(key, 16)
    }

    fn find_str_depth(&self, key: &str, depth: u32) -> Option<&str> {
        if let Some(v) = self.get_str(key) {
            return Some(v);
        }
        if depth == 0 {
            return None;
        }
        self.entries.iter().find_map(|(_, v)| match v {
            Value::Obj(o) => o.find_str_depth(key, depth - 1),
            Value::Str(_) => None,
        })
    }
}

/// How deeply objects may nest before parsing gives up.
///
/// The parser descends once per `{`, so without a limit the *input file*
/// decides how much stack to consume. Measured on a default test thread,
/// 3,200 levels parsed fine and 4,800 aborted the whole process with a stack
/// overflow. That is an abort, not a panic, so nothing could catch it. Roughly 20 KB of
/// `.acf` reaches that depth, and Steam's manifests live in a directory the
/// user (or anything running as them) can write, so a corrupt or hostile file
/// could take down a library scan with no message.
///
/// Real manifests nest four or five deep; `libraryfolders.vdf` reaches three.
/// A few hundred levels costs nothing and leaves the limit far out of the way
/// of any genuine file. Dropping the parsed tree recurses the same way, so
/// refusing to build a deep one protects that too.
pub const MAX_DEPTH: u32 = 128;

/// Parses a whole document and returns its single top-level object.
///
/// The top-level key itself (`"AppState"`, `"libraryfolders"`, …) is dropped;
/// callers never need it.
pub fn parse(text: &str) -> Result<Object, Error> {
    let (_, obj) = parse_root(text)?;
    Ok(obj)
}

/// Like [`parse`], but also returns the top-level key.
pub fn parse_root(text: &str) -> Result<(String, Object), Error> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lexer = Lexer::new(body);
    let key = match lexer.next_token()? {
        Some(Token::Str(s)) => s,
        _ => return Err(lexer.err(ErrorKind::ExpectedRoot)),
    };
    match lexer.next_token()? {
        Some(Token::Open) => {}
        _ => return Err(lexer.err(ErrorKind::ExpectedRoot)),
    }
    let obj = lexer.parse_object(1)?;
    Ok((key, obj))
}

enum Token {
    Open,
    Close,
    Str(String),
    /// A `[$WIN32]`-style conditional, always ignored.
    Cond,
}

struct Lexer<'a> {
    rest: std::str::Chars<'a>,
    peeked: Option<char>,
    line: u32,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text.chars(), peeked: None, line: 1 }
    }

    fn err(&self, kind: ErrorKind) -> Error {
        Error { line: self.line, kind }
    }

    fn bump(&mut self) -> Option<char> {
        let c = match self.peeked.take() {
            Some(c) => Some(c),
            None => self.rest.next(),
        };
        if c == Some('\n') {
            self.line = self.line.saturating_add(1);
        }
        c
    }

    fn peek(&mut self) -> Option<char> {
        if self.peeked.is_none() {
            self.peeked = self.rest.next();
        }
        self.peeked
    }

    /// Skips whitespace and `//` comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') => {
                    self.bump();
                    if self.peek() == Some('/') {
                        while let Some(c) = self.bump() {
                            if c == '\n' {
                                break;
                            }
                        }
                    } else {
                        // A lone '/' starts an unquoted token; put it back by
                        // treating it as the start of one.
                        self.peeked = Some('/');
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<Token>, Error> {
        self.skip_trivia();
        let Some(c) = self.peek() else { return Ok(None) };
        match c {
            '{' => {
                self.bump();
                Ok(Some(Token::Open))
            }
            '}' => {
                self.bump();
                Ok(Some(Token::Close))
            }
            '"' => {
                self.bump();
                Ok(Some(Token::Str(self.quoted()?)))
            }
            '[' => {
                self.bump();
                loop {
                    match self.bump() {
                        Some(']') => break,
                        Some(_) => {}
                        None => return Err(self.err(ErrorKind::UnterminatedConditional)),
                    }
                }
                Ok(Some(Token::Cond))
            }
            _ => Ok(Some(Token::Str(self.unquoted()))),
        }
    }

    /// Reads the body of a quoted string; the opening quote is already eaten.
    fn quoted(&mut self) -> Result<String, Error> {
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    // Steam keeps unknown escapes verbatim, e.g. the `\s` that
                    // shows up in some Windows paths.
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => return Err(self.err(ErrorKind::UnterminatedString)),
                },
                Some(c) => out.push(c),
                None => return Err(self.err(ErrorKind::UnterminatedString)),
            }
        }
    }

    /// Reads a bare token, up to whitespace or a brace.
    fn unquoted(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || c == '{' || c == '}' || c == '"' {
                break;
            }
            out.push(c);
            self.bump();
        }
        out
    }

    /// Parses entries until the matching `}`; the `{` is already eaten.
    ///
    /// `depth` is this object's nesting level, starting at 1 for the
    /// top-level object, and is capped at [`MAX_DEPTH`] so the input cannot
    /// choose how much stack to use.
    fn parse_object(&mut self, depth: u32) -> Result<Object, Error> {
        if depth > MAX_DEPTH {
            return Err(self.err(ErrorKind::TooDeep));
        }
        let mut entries = Vec::new();
        loop {
            let key = match self.next_token()? {
                Some(Token::Str(s)) => s,
                Some(Token::Close) => return Ok(Object { entries }),
                Some(Token::Cond) => continue,
                Some(Token::Open) => return Err(self.err(ErrorKind::ExpectedValue)),
                None => return Err(self.err(ErrorKind::UnexpectedEof)),
            };
            let value = match self.next_token()? {
                Some(Token::Str(s)) => {
                    // An optional [$COND] may follow a value; drop it.
                    Value::Str(s)
                }
                Some(Token::Open) => Value::Obj(self.parse_object(depth.saturating_add(1))?),
                Some(Token::Close) => return Err(self.err(ErrorKind::UnexpectedBrace)),
                Some(Token::Cond) => return Err(self.err(ErrorKind::ExpectedValue)),
                None => return Err(self.err(ErrorKind::UnexpectedEof)),
            };
            entries.push((key, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::{Ctx, TestResult, check_eq};

    use super::*;

    #[test]
    fn parses_an_app_manifest() -> TestResult {
        let text = r#"
"AppState"
{
	"appid"		"105600"
	"name"		"Terraria"
	"StateFlags"		"4"
	"InstalledDepots"
	{
		"105602"
		{
			"manifest"		"4206021387845829879"
			"size"		"831365954"
		}
	}
}
"#;
        let (root, obj) = parse_root(text).ctx("parsing the app manifest")?;
        check_eq(root.as_str(), "AppState", "the top-level key")?;
        check_eq(obj.get_str("name"), Some("Terraria"), "the name")?;
        check_eq(obj.get_u32("StateFlags"), Some(4), "StateFlags")?;
        // Keys are case-insensitive, like Steam's own lookups.
        check_eq(obj.get_u64("appid"), Some(105600), "appid")?;
        check_eq(obj.get_u64("AppID"), Some(105600), "appid, looked up as AppID")?;
        let depot = obj
            .path(&["InstalledDepots", "105602"])
            .ctx("the InstalledDepots/105602 object")?;
        check_eq(depot.get_u64("size"), Some(831365954), "the depot size")
    }

    #[test]
    fn skips_comments_and_conditionals() -> TestResult {
        let text = r#"
"root" // trailing comment
{
	// whole-line comment
	"a"	"1" [$WIN32]
	"b"	"2"
}
"#;
        let obj = parse(text).ctx("parsing a document with comments")?;
        check_eq(obj.get_str("a"), Some("1"), "the value before the conditional")?;
        check_eq(obj.get_str("b"), Some("2"), "the value after the conditional")
    }

    #[test]
    fn keeps_duplicate_keys_in_order_and_returns_the_first() -> TestResult {
        let obj = parse("\"root\" { \"k\" \"one\" \"k\" \"two\" }")
            .ctx("parsing a document with duplicate keys")?;
        check_eq(obj.get_str("k"), Some("one"), "the first value wins")?;
        check_eq(obj.entries().len(), 2, "both entries are kept")
    }

    #[test]
    fn handles_escapes_and_unquoted_tokens() -> TestResult {
        let obj = parse(r#""root" { "p" "C:\\games\\x" bare value }"#)
            .ctx("parsing escapes and bare tokens")?;
        check_eq(obj.get_str("p"), Some(r"C:\games\x"), "the unescaped path")?;
        check_eq(obj.get_str("bare"), Some("value"), "the unquoted key/value pair")
    }

    #[test]
    fn find_str_searches_nested_objects() -> TestResult {
        let text = r#""Registry" { "HKCU" { "Software" { "Valve" { "Steam" {
            "RunningAppID"  "105600"
        } } } } }"#;
        let obj = parse(text).ctx("parsing a nested registry document")?;
        check_eq(obj.find_str("RunningAppID"), Some("105600"), "the nested search")?;
        check_eq(
            obj.path(&["HKCU", "Software", "Valve", "Steam"])
                .and_then(|o| o.get_u32("RunningAppID")),
            Some(105600),
            "the same value by explicit path",
        )
    }

    #[test]
    fn reports_the_line_of_a_failure() -> TestResult {
        let err = parse("\"root\"\n{\n\t\"a\" \"unterminated\n")
            .err()
            .ctx("expected a parse error")?;
        check_eq(err.kind, ErrorKind::UnterminatedString, "the error kind")?;
        check_eq(err.line, 4, "the error line")
    }

    #[test]
    fn rejects_a_document_without_a_root_object() -> TestResult {
        let err = parse("\"a\" \"b\"").err().ctx("expected a parse error")?;
        check_eq(err.kind, ErrorKind::ExpectedRoot, "the error kind")
    }
}
