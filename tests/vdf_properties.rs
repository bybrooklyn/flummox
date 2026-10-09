//! Properties of the Valve KeyValues (`.vdf`, `.acf`) parser.
//!
//! This parser is the only place in flummox that reads bytes written by
//! someone else. Steam owns `appmanifest_*.acf` and rewrites it whenever a game
//! installs, updates or is interrupted mid-write; a half-flushed manifest, a
//! manifest from a newer Steam, or a file a user copied in by hand are all
//! things the tool will meet in the field. A scan runs over every manifest in
//! every library, so one bad file must cost the user one game, not the whole
//! scan.
//!
//! That turns "the parser handles malformed input" into a claim that cannot be
//! checked with examples: the interesting inputs are the ones no author thought
//! to write down. The properties here therefore quantify over arbitrary input
//! and assert the things that must hold for *all* of it:
//!
//! * a call to `parse` always comes back: no panic, no hang, no stack
//!   overflow from nesting the input controls;
//! * an error never points at a line the file does not have;
//! * the parser never invents more structure than it read, so a small hostile
//!   file cannot inflate into an out-of-memory;
//! * anything written in quoted form is read back unchanged.

#![cfg(target_os = "linux")]

use flummox::launchers::vdf::{self, Object};
use flummox::testutil::{TestResult, check, check_eq};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// Counts every key/value pair in a document, at every depth.
///
/// Iterative, because a recursive walk would overflow the stack on exactly
/// the inputs these properties exist to probe, and the test would then report
/// its own bug instead of the parser's.
fn count_entries(root: &Object) -> usize {
    let mut total = 0usize;
    let mut stack: Vec<&Object> = vec![root];
    while let Some(obj) = stack.pop() {
        total = total.saturating_add(obj.entries().len());
        for (_, value) in obj.entries() {
            if let Some(child) = value.as_obj() {
                stack.push(child);
            }
        }
    }
    total
}

/// Fragments of real KeyValues syntax, shuffled into nonsense.
///
/// Purely random bytes almost never produce a `"` followed by a `{`, so they
/// exercise little more than the first two tokens. Assembling documents out of
/// the parser's own vocabulary (braces, quotes, backslashes, `[$COND]`
/// brackets, `//`, newlines, a BOM) drives it deep into the states that only
/// occur part-way through a real manifest.
fn token_soup() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("{".to_owned()),
        Just("}".to_owned()),
        Just("\"".to_owned()),
        Just("\\".to_owned()),
        Just("[".to_owned()),
        Just("]".to_owned()),
        Just("[$WIN32]".to_owned()),
        Just("$LINUX".to_owned()),
        Just("//".to_owned()),
        Just("/".to_owned()),
        Just("\n".to_owned()),
        Just("\t".to_owned()),
        Just(" ".to_owned()),
        Just("\u{feff}".to_owned()),
        Just("\"AppState\"".to_owned()),
        Just("\"appid\"\t\"105600\"".to_owned()),
        "[A-Za-z0-9_.:\\\\-]{1,40}",
    ];
    prop::collection::vec(piece, 0..60).prop_map(|pieces| pieces.concat())
}

/// Every shape of hostile input at once: syntax soup, arbitrary Unicode, and
/// random bytes coerced to UTF-8 the way a truncated or binary file would be.
fn arbitrary_document() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => token_soup(),
        2 => any::<String>(),
        2 => prop::collection::vec(any::<u8>(), 0..512)
            .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    ]
}

/// Characters a Steam manifest actually carries, weighted towards the five
/// that the quoting rules have to escape.
fn vdf_char() -> impl Strategy<Value = char> {
    prop_oneof![
        8 => prop::char::range(' ', '~'),
        2 => prop_oneof![Just('\n'), Just('\t'), Just('\r'), Just('\\'), Just('"')],
    ]
}

/// A string of the characters a manifest may contain.
fn vdf_text() -> impl Strategy<Value = String> {
    prop::collection::vec(vdf_char(), 0..24).prop_map(|chars| chars.into_iter().collect())
}

/// Writes a string the way the format requires it to be quoted.
///
/// This is the encoder half of the round-trip: if it and the parser ever
/// disagree, a path such as `C:\games\x` comes back mangled and the tool
/// compresses the wrong directory.
fn vdf_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// Renders key/value pairs as a complete document with a root object.
fn write_document(pairs: &[(String, String)]) -> String {
    let mut text = String::from("\"root\"\n{\n");
    for (key, value) in pairs {
        text.push('"');
        text.push_str(&vdf_escape(key));
        text.push_str("\"\t\"");
        text.push_str(&vdf_escape(value));
        text.push_str("\"\n");
    }
    text.push_str("}\n");
    text
}

/// A value in a generated, structurally valid document.
#[derive(Debug, Clone)]
enum Val {
    /// A string, written quoted or as a bare token.
    Leaf(String, bool),
    /// An object of plain quoted pairs.
    Obj(Vec<(String, String)>),
}

/// A bare token the lexer must read back unchanged. It may start with a lone
/// slash and may hold `/`, but never begins a `//` comment.
fn bare_word() -> impl Strategy<Value = String> {
    prop_oneof![
        1 => Just("/".to_owned()),
        6 => "/?[A-Za-z0-9_.:-][A-Za-z0-9_./:-]{0,10}",
    ]
}

/// Entries of a valid document: a key, a value, and whether a comment follows.
fn valid_entries() -> impl Strategy<Value = Vec<(String, Val, bool)>> {
    let leaf = prop_oneof![
        (vdf_text(), Just(false)).prop_map(|(t, q)| Val::Leaf(t, q)),
        (bare_word(), Just(true)).prop_map(|(t, u)| Val::Leaf(t, u)),
    ];
    let obj = prop::collection::vec((vdf_text(), vdf_text()), 0..4).prop_map(Val::Obj);
    prop::collection::vec((vdf_text(), prop_oneof![leaf, obj], any::<bool>()), 0..8)
}

/// Renders [`valid_entries`]. `unquoted` in a leaf means the bare form.
fn write_valid(entries: &[(String, Val, bool)]) -> String {
    let mut text = String::from("\"root\"\n{\n");
    for (key, value, comment) in entries {
        text.push_str(&format!("\"{}\" ", vdf_escape(key)));
        match value {
            Val::Leaf(token, true) => text.push_str(token),
            Val::Leaf(token, false) => text.push_str(&format!("\"{}\"", vdf_escape(token))),
            Val::Obj(pairs) => {
                text.push_str("{\n");
                for (k, v) in pairs {
                    text.push_str(&format!("\"{}\" \"{}\"\n", vdf_escape(k), vdf_escape(v)));
                }
                text.push('}');
            }
        }
        text.push_str(if *comment { " // note\n" } else { "\n" });
    }
    text.push_str("}\n");
    text
}

proptest! {
    // Each case builds and parses a document up to a few hundred characters
    // long, so a few hundred cases stay well inside a second.
    //
    // `failure_persistence: None`: an integration test has no `src` directory
    // for proptest to keep a `.proptest-regressions` file beside, and left on
    // it warns and writes a stray file into `tests/`. The inputs that matter
    // most are enumerated outright in `pathological_documents_terminate_with_
    // a_result` rather than left to a persisted seed.
    #![proptest_config(ProptestConfig {
        cases: 400,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// A malformed manifest must cost one game, never the process.
    ///
    /// The headline claim is simply that `parse` *returns*: reaching the line
    /// after the call means it did not panic, did not loop forever and did not
    /// recurse off the end of the stack. The assertions then pin down what the
    /// two possible answers are allowed to say.
    #[test]
    fn arbitrary_input_never_takes_the_parser_down(text in arbitrary_document()) {
        let chars = text.chars().count();
        match vdf::parse(&text) {
            Ok(obj) => {
                // Every entry costs the lexer at least a key token and a value
                // token, and no token is produced without consuming a
                // character. So a document can never yield more pairs than it
                // has characters. Without this bound, a short hostile file
                // that made the parser emit entries without advancing would be
                // both an infinite loop and an out-of-memory.
                prop_assert!(
                    count_entries(&obj) <= chars,
                    "{} entries out of {chars} characters",
                    count_entries(&obj)
                );
                // Parsing is a pure function of the text. A parser that
                // depended on hidden state would make a scan's results depend
                // on the order libraries happen to be visited in.
                prop_assert_eq!(vdf::parse(&text).ok(), Some(obj), "parsing is deterministic");
            }
            Err(e) => {
                // The line number ends up in a warning the user reads, so it
                // must identify a line that exists. A counter that wrapped or
                // ran past the end of the file would send someone hunting
                // through a manifest for a problem that is somewhere else.
                let newlines = text.chars().filter(|c| *c == '\n').count();
                prop_assert!(e.line >= 1, "lines are 1-based, got {}", e.line);
                prop_assert!(
                    usize::try_from(e.line).unwrap_or(usize::MAX) <= newlines.saturating_add(1),
                    "error at line {} but the document has {} lines",
                    e.line,
                    newlines.saturating_add(1)
                );
            }
        }
    }

    /// Valid documents, bare tokens and comments included, read back exactly.
    ///
    /// The other generators rarely produce a well-formed document, so this one
    /// is where the entry count and every value are compared with what was
    /// written.
    #[test]
    fn valid_documents_with_bare_tokens_and_comments_read_back(entries in valid_entries()) {
        let text = write_valid(&entries);
        let obj = match vdf::parse(&text) {
            Ok(obj) => obj,
            Err(e) => return Err(TestCaseError::fail(format!("{e} while parsing {text:?}"))),
        };
        prop_assert_eq!(obj.entries().len(), entries.len(), "top-level count in {:?}", text);
        for (i, (key, value, _)) in entries.iter().enumerate() {
            let Some((got_key, got)) = obj.entries().get(i) else {
                return Err(TestCaseError::fail(format!("no entry {i} in {text:?}")));
            };
            prop_assert_eq!(got_key, key, "key {}", i);
            match value {
                Val::Leaf(token, _) => {
                    prop_assert_eq!(got.as_str(), Some(token.as_str()), "value {} in {:?}", i, text);
                }
                Val::Obj(pairs) => {
                    let inner = got.as_obj().map(|o| o.entries().len());
                    prop_assert_eq!(inner, Some(pairs.len()), "object {} in {:?}", i, text);
                }
            }
        }
        prop_assert_eq!(count_entries(&obj) <= text.chars().count(), true, "entry bound");
    }

    /// Cutting a valid document anywhere before its final brace is an error.
    ///
    /// The root object is then unclosed whatever the cut lands in, so a parser
    /// that accepted a truncated manifest as a shorter complete one fails here.
    #[test]
    fn truncated_valid_documents_are_rejected(entries in valid_entries(), cut in any::<prop::sample::Index>()) {
        let text = write_valid(&entries);
        let chars: Vec<char> = text.chars().collect();
        let limit = chars.len().saturating_sub(2);
        let keep = cut.index(limit.max(1)).min(limit);
        let truncated: String = chars.iter().take(keep).collect();
        prop_assert!(
            vdf::parse(&truncated).is_err(),
            "accepted a truncated document: {:?}",
            truncated
        );
    }

    /// Whatever a manifest says, the tool must read back what was written.
    ///
    /// Steam stores Windows paths (`C:\\games\\x`), display names with quotes
    /// and multi-line descriptions. If the escape rules do not survive a
    /// round-trip, an install path changes meaning without saying so, and an install
    /// path is the directory this tool is about to rewrite every file in.
    /// Positional comparison is deliberate: `get` is case-insensitive and
    /// returns the first match, which would hide a mismatch whenever the
    /// generator produced two keys that collide.
    #[test]
    fn quoted_pairs_survive_a_write_and_a_read(
        pairs in prop::collection::vec((vdf_text(), vdf_text()), 0..8),
    ) {
        let text = write_document(&pairs);
        let obj = match vdf::parse(&text) {
            Ok(obj) => obj,
            Err(e) => {
                return Err(TestCaseError::fail(format!("{e} while parsing {text:?}")));
            }
        };
        prop_assert_eq!(obj.entries().len(), pairs.len(), "every pair comes back");
        for (i, (key, value)) in pairs.iter().enumerate() {
            let Some((got_key, got_value)) = obj.entries().get(i) else {
                return Err(TestCaseError::fail(format!("no entry at {i} in {text:?}")));
            };
            prop_assert_eq!(got_key, key, "key {} round-trips", i);
            let Some(got) = got_value.as_str() else {
                return Err(TestCaseError::fail(format!("entry {i} is not a leaf")));
            };
            prop_assert_eq!(got, value.as_str(), "value {} round-trips", i);
        }
    }
}

/// Inputs chosen to break a parser in a specific way, each of which must still
/// come back with an answer.
///
/// These are the cases a random generator will essentially never produce: a
/// key of a quarter of a megabyte, a quote that opens at the very end of the
/// file, a conditional with no `]`, tens of thousands of braces. Each one
/// targets a loop that could run away or an allocation that could be driven by
/// the attacker's length rather than ours.
#[test]
fn pathological_documents_terminate_with_a_result() -> TestResult {
    use vdf::ErrorKind::{
        ExpectedRoot, UnexpectedEof, UnterminatedConditional, UnterminatedString,
    };
    // Each document with the answer it must give: `Ok(entries)` or the error.
    let cases: Vec<(String, Result<usize, vdf::ErrorKind>)> = vec![
        // Truncated mid-object: the shape of a manifest Steam was killed while
        // writing.
        ("\"AppState\" {".to_owned(), Err(UnexpectedEof)),
        // A quote that never closes, at end of input.
        ("\"".to_owned(), Err(UnterminatedString)),
        (
            "\"AppState\" { \"name\" \"unterminated".to_owned(),
            Err(UnterminatedString),
        ),
        // A trailing backslash: the escape reader must not read past the end.
        (
            "\"AppState\" { \"name\" \"path\\".to_owned(),
            Err(UnterminatedString),
        ),
        // A conditional with no closing bracket.
        (
            "\"AppState\" { [$UNTERMINATED".to_owned(),
            Err(UnterminatedConditional),
        ),
        // A key and a token far larger than any real manifest holds.
        (
            format!("\"AppState\" {{ \"{}\" \"v\" }}", "k".repeat(200_000)),
            Ok(1),
        ),
        (format!("\"{}\"", "x".repeat(200_000)), Err(ExpectedRoot)),
        ("x".repeat(200_000), Err(ExpectedRoot)),
        // Braces with nothing to bind them to, in both directions.
        ("{".repeat(10_000), Err(ExpectedRoot)),
        ("}".repeat(10_000), Err(ExpectedRoot)),
        // A great many entries from a small file: 25,000 key/value pairs.
        (
            format!("\"AppState\" {{ {} }}", "\"a\" ".repeat(50_000)),
            Ok(25_000),
        ),
        // A comment that never ends, and control characters in a value.
        ("// no newline ever".to_owned(), Err(ExpectedRoot)),
        (
            "\"AppState\" { \"a\" \"\u{0}\u{1}\u{feff}\" }".to_owned(),
            Ok(1),
        ),
    ];
    for (i, (case, expected)) in cases.iter().enumerate() {
        let outcome = vdf::parse(case)
            .map(|obj| count_entries(&obj))
            .map_err(|e| e.kind);
        check_eq(outcome, expected.clone(), format!("case {i}"))?;
    }
    Ok(())
}

/// Builds `"AppState" { "k" { "k" { ... } } }`, nested `depth` levels below
/// the root object.
fn nested_document(depth: usize) -> String {
    let mut text = String::with_capacity(depth.saturating_mul(8).saturating_add(16));
    text.push_str("\"AppState\"\n{\n");
    for _ in 0..depth {
        text.push_str("\"k\" {\n");
    }
    for _ in 0..depth {
        text.push_str("}\n");
    }
    text.push_str("}\n");
    text
}

/// Nesting is the one thing in this format whose cost the *file* chooses.
///
/// `parse_object` descends once per `{`, so without a cap a manifest is free
/// to ask for as many stack frames as it likes. Ten thousand levels is nothing
/// to write, sixty kilobytes of text, and a stack overflow is not a
/// catchable error in Rust: it is an abort that takes the whole process with
/// it, which for a tool that scans every library on the machine means a crash
/// with no message. Dropping the parsed tree recurses the same way, so
/// refusing to build a deep one protects that too.
///
/// This test found a real defect, now fixed: the parser caps nesting at
/// [`vdf::MAX_DEPTH`]. Before the fix, measured on a default test thread,
/// 3,200 levels parsed fine and 4,800 aborted with `fatal runtime error:
/// stack overflow` (SIGABRT), taking every other test in the binary with it;
/// the same document parsed happily under `RUST_MIN_STACK=67108864`, which
/// confirmed stack depth was the only cause. About 20 KiB of `.acf` reached
/// that depth, and Steam's manifests live in a directory the user (or
/// anything running as them) can write to.
///
/// The claim has two halves. A parser that rejected everything would satisfy
/// the first, so the second pins the limit far enough from real files:
/// manifests nest four or five deep, `libraryfolders.vdf` three.
#[test]
fn deeply_nested_objects_are_refused_rather_than_overflowing_the_stack() -> TestResult {
    let deep = nested_document(10_000);
    let Err(e) = vdf::parse(&deep) else {
        return check(false, "a 10,000-level document must be refused, not parsed");
    };
    check_eq(
        e.kind,
        vdf::ErrorKind::TooDeep,
        "it should be refused for depth, not by accident",
    )?;

    let limit = usize::try_from(vdf::MAX_DEPTH).unwrap_or(usize::MAX);
    let within = nested_document(limit.saturating_sub(2));
    check(
        vdf::parse(&within).is_ok(),
        "a document inside the limit must still parse",
    )
}
