//! Properties of the Valve KeyValues (`.vdf`, `.acf`) parser.
//!
//! This parser is the only place in gamecompressor that reads bytes written by
//! someone else. Steam owns `appmanifest_*.acf` and rewrites it whenever a game
//! installs, updates or is interrupted mid-write; a half-flushed manifest, a
//! manifest from a newer Steam, or a file a user copied in by hand are all
//! things the tool will meet in the field. A scan runs over every manifest in
//! every library, so one bad file must cost the user one game, not the whole
//! scan.
//!
//! That turns "the parser handles malformed input" into a claim that cannot be
//! checked with examples: the interesting inputs are the ones nobody thought
//! to write down. The properties here therefore quantify over arbitrary input
//! and assert the things that must hold for *all* of it:
//!
//! * a call to `parse` always comes back — no panic, no hang, no stack
//!   overflow from nesting the input controls;
//! * an error never points at a line the file does not have;
//! * the parser never invents more structure than it read, so a small hostile
//!   file cannot inflate into an out-of-memory;
//! * anything written in quoted form is read back unchanged.

use gc_launchers::vdf::{self, Object};
use gc_testutil::{TestResult, check, check_eq};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// Counts every key/value pair in a document, at every depth.
///
/// Deliberately iterative. A recursive walk here would overflow the stack on
/// exactly the inputs these properties exist to probe, and the test would then
/// be reporting its own bug rather than the parser's.
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
/// the parser's own vocabulary — braces, quotes, backslashes, `[$COND]`
/// brackets, `//`, newlines, a BOM — drives it deep into the states that only
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

    /// Whatever a manifest says, the tool must read back what was written.
    ///
    /// Steam stores Windows paths (`C:\\games\\x`), display names with quotes
    /// and multi-line descriptions. If the escape rules do not survive a
    /// round-trip, an install path silently changes meaning — and an install
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
    let cases: Vec<String> = vec![
        // Truncated mid-object: the shape of a manifest Steam was killed while
        // writing.
        "\"AppState\" {".to_owned(),
        // A quote that never closes, at end of input.
        "\"".to_owned(),
        "\"AppState\" { \"name\" \"unterminated".to_owned(),
        // A trailing backslash: the escape reader must not read past the end.
        "\"AppState\" { \"name\" \"path\\".to_owned(),
        // A conditional with no closing bracket.
        "\"AppState\" { [$UNTERMINATED".to_owned(),
        // A key and a token far larger than any real manifest holds.
        format!("\"AppState\" {{ \"{}\" \"v\" }}", "k".repeat(200_000)),
        format!("\"{}\"", "x".repeat(200_000)),
        "x".repeat(200_000),
        // Braces with nothing to bind them to, in both directions.
        "{".repeat(10_000),
        "}".repeat(10_000),
        // A great many entries from a small file.
        format!("\"AppState\" {{ {} }}", "\"a\" ".repeat(50_000)),
        // A comment that never ends, and control characters in a value.
        "// no newline ever".to_owned(),
        "\"AppState\" { \"a\" \"\u{0}\u{1}\u{feff}\" }".to_owned(),
    ];
    for (i, case) in cases.iter().enumerate() {
        // Reaching this assignment at all is the property under test.
        let outcome = match vdf::parse(case) {
            Ok(obj) => format!("case {i}: parsed {} entries", count_entries(&obj)),
            Err(e) => format!("case {i}: rejected at line {}: {}", e.line, e.kind),
        };
        check(!outcome.is_empty(), outcome)?;
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
/// to write — sixty kilobytes of text — and a stack overflow is not a
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
/// The claim has two halves on purpose. A parser that rejected everything
/// would satisfy the first, so the second pins the limit far enough away from
/// real files: manifests nest four or five deep, `libraryfolders.vdf` three.
#[test]
fn deeply_nested_objects_are_refused_rather_than_overflowing_the_stack() -> TestResult {
    let deep = nested_document(10_000);
    let Err(e) = vdf::parse(&deep) else {
        return check(false, "a 10,000-level document must be refused, not parsed");
    };
    check_eq(e.kind, vdf::ErrorKind::TooDeep, "it should be refused for depth, not by accident")?;

    let limit = usize::try_from(vdf::MAX_DEPTH).unwrap_or(usize::MAX);
    let within = nested_document(limit.saturating_sub(2));
    check(vdf::parse(&within).is_ok(), "a document inside the limit must still parse")
}
