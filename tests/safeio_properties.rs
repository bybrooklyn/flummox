//! Properties of the path-containment check.
//!
//! [`is_contained`] is a security boundary, not a convenience. It is the
//! filter in front of `openat2`, and it decides whether a relative path that
//! came out of a walk — or out of a pack manifest, which is a file on disk
//! that a game's installer could have written — is allowed to be opened
//! against the install directory's handle. Everything downstream of it is a
//! job that *rewrites* the files it opens.
//!
//! A boundary like that is exactly the wrong place for three examples. Three
//! examples say `..` is rejected; a property says *nothing built from `..` is
//! ever accepted, whatever it is wrapped in*. The difference is the case
//! nobody wrote down: `a/../../b`, `./..`, `foo/..bar/../..`, a component that
//! merely looks like a parent reference, a path whose escape is hidden behind
//! a dozen harmless-looking names.
//!
//! The last property here is the one that states the guarantee in the terms
//! the caller actually depends on: anything this function accepts, when joined
//! to the install directory, still names something inside it.

use std::path::{Component, Path};

use flummox::safeio::is_contained;
use proptest::prelude::*;

/// One ordinary path component: non-empty, no separator, and not `..`.
///
/// The character class is deliberately almost unrestricted. Game directories
/// hold names with spaces, dots, colons, emoji and whatever else a publisher
/// felt like shipping, and none of that should make a path look like an
/// escape attempt.
fn normal_component() -> impl Strategy<Value = String> {
    "[^/\\x00]{1,8}".prop_filter("`..` is not an ordinary component", |s| s != "..")
}

/// Path-shaped text with no rules at all, for the direction that quantifies
/// over everything rather than over well-formed paths.
fn arbitrary_path_text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("..".to_owned()),
        Just(".".to_owned()),
        Just("...".to_owned()),
        Just("/".to_owned()),
        Just(String::new()),
        Just("..\\".to_owned()),
        "[^\\x00]{1,6}",
    ];
    prop::collection::vec(piece, 0..8).prop_map(|pieces| pieces.join("/"))
}

proptest! {
    // `failure_persistence: None`: an integration test has no `src` directory
    // for proptest to keep a `.proptest-regressions` file beside, and left on
    // it warns and writes a stray file into `tests/`.
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Ordinary names are allowed however strange they look.
    ///
    /// The failure mode this guards against is a check that is *too* strict:
    /// rejecting `..weird`, `Portal 2`, or a Chinese-titled directory would
    /// make the tool silently skip real game files, and a skipped file looks
    /// exactly like a file that was not worth compressing.
    #[test]
    fn ordinary_names_are_accepted_however_odd(
        parts in prop::collection::vec(normal_component(), 1..6),
    ) {
        let joined = parts.join("/");
        prop_assert!(is_contained(Path::new(&joined)), "{joined:?} should be allowed");
    }

    /// A parent reference anywhere in the path is refused.
    ///
    /// Not just a leading `..`: the component can be buried at any depth,
    /// behind any number of names that individually look fine. A check that
    /// only inspected the first component — the obvious way to write this —
    /// would pass every example anyone tends to write and still let
    /// `data/../../../.ssh/id_ed25519` through.
    #[test]
    fn a_parent_reference_at_any_depth_is_refused(
        parts in prop::collection::vec(normal_component(), 0..6),
        at in 0usize..7,
    ) {
        let mut parts = parts;
        let at = at.min(parts.len());
        parts.insert(at, "..".to_owned());
        let joined = parts.join("/");
        prop_assert!(!is_contained(Path::new(&joined)), "{joined:?} escapes and must be refused");
    }

    /// An absolute path is refused, whatever follows the slash.
    ///
    /// `openat` ignores its directory handle entirely when handed an absolute
    /// path, so a leading `/` does not merely escape the install directory: it
    /// makes the anchor irrelevant and turns the whole mechanism off.
    #[test]
    fn an_absolute_path_is_refused(
        parts in prop::collection::vec(normal_component(), 0..6),
    ) {
        let joined = format!("/{}", parts.join("/"));
        prop_assert!(!is_contained(Path::new(&joined)), "{joined:?} is absolute");
    }

    /// Anything accepted still names something inside the install directory.
    ///
    /// This is the claim the caller relies on, stated over completely
    /// arbitrary text rather than over paths that were built to be valid. It
    /// checks the guarantee rather than the implementation: whatever the
    /// function says yes to must, once joined to the anchor, both start at the
    /// anchor and contain no component that could walk back out of it.
    #[test]
    fn whatever_is_accepted_stays_under_the_anchor(raw in arbitrary_path_text()) {
        let rel = Path::new(&raw);
        if !is_contained(rel) {
            return Ok(());
        }
        let base = Path::new("/srv/games/SteamLibrary/steamapps/common/Terraria");
        let joined = base.join(rel);
        prop_assert!(
            joined.starts_with(base),
            "{raw:?} was accepted but {joined:?} left the install directory"
        );
        prop_assert!(
            joined.components().all(|c| !matches!(c, Component::ParentDir)),
            "{raw:?} was accepted but still carries a parent reference"
        );
    }

    /// The empty path is never a file.
    ///
    /// An empty relative path resolves to the anchor directory itself, so
    /// accepting it would hand a job a directory where it expected a file.
    #[test]
    fn nothing_that_names_no_file_is_accepted(pad in prop::collection::vec(Just(String::new()), 0..4)) {
        let joined = pad.join("");
        prop_assert!(!is_contained(Path::new(&joined)), "the empty path names no file");
    }
}
