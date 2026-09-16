//! Properties of the per-file compress/skip decision.
//!
//! [`decide`] runs once per file on a walk of an install directory, hundreds
//! of thousands of times on a full Steam library, and every `Compress` it
//! returns becomes a file the tool rewrites. Two of its rules exist purely to
//! stop work that cannot pay off, and both are easy to break in a way no
//! example test would notice:
//!
//! * the size floor. Below it, compressing costs more than it saves: on the
//!   pack tier the file's own store object outweighs the gain, and on a native
//!   filesystem a file too small to free a whole sector frees nothing. A file
//!   that slips under the floor is not a wrong number, it is wasted I/O and a
//!   pointlessly rewritten extent.
//! * the already-compressed extension list. Running zstd over an `.mp4` or a
//!   `.flac` burns CPU and, because compression rewrites every extent, can
//!   leave a snapshotted subvolume *fuller* than it started.
//!
//! The properties below quantify over both floors at once, because the two
//! tiers share this function and a change made for one must not quietly alter
//! the other.

use std::path::PathBuf;

use flummox::inventory::{Action, WalkOpts, decide, is_precompressed_name};
use proptest::prelude::*;

/// Extensions the walker is expected to treat as already compressed.
///
/// Spelled out here rather than imported so the test states an independent
/// expectation: if someone deletes an entry from the crate's own list, this
/// list still says `.mp4` must not be recompressed.
const PRECOMPRESSED: &[&str] = &[
    "zst", "xz", "gz", "bz2", "7z", "rar", "zip", "lz4", "mp4", "mkv", "webm", "bik", "mp3",
    "ogg", "flac", "wem", "jpg", "png", "webp", "dds", "ktx2", "woff2",
];

/// Both size floors the workspace ships: the pack tier's and the native one's.
fn floors() -> impl Strategy<Value = WalkOpts> {
    prop_oneof![Just(WalkOpts::default()), Just(WalkOpts::native())]
}

/// A plausible file name stem, including the dots and spaces real games use.
fn stem() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.-]{1,12}"
}

/// A relative path whose extension says the contents are already compressed,
/// in either letter case. Steam depots ship both `.DDS` and `.dds`.
fn precompressed_path() -> impl Strategy<Value = PathBuf> {
    (stem(), 0usize..PRECOMPRESSED.len(), any::<bool>()).prop_filter_map(
        "a known precompressed extension",
        |(stem, index, upper)| {
            let ext = PRECOMPRESSED.get(index)?;
            let ext = if upper { ext.to_ascii_uppercase() } else { (*ext).to_owned() };
            Some(PathBuf::from(format!("data/{stem}.{ext}")))
        },
    )
}

/// A relative path that nothing about its name marks as already compressed.
///
/// `.pak` is included on purpose: the crate deliberately leaves game archives
/// to content sampling, because some are compressed and some are not.
fn plain_path() -> impl Strategy<Value = PathBuf> {
    (
        stem(),
        prop_oneof![
            Just(String::new()),
            Just("pak".to_owned()),
            Just("dat".to_owned()),
            Just("exe".to_owned()),
            Just("bin".to_owned()),
            Just("assets".to_owned()),
            Just("uasset".to_owned()),
        ],
    )
        .prop_map(|(stem, ext)| {
            if ext.is_empty() {
                PathBuf::from(format!("data/{stem}"))
            } else {
                PathBuf::from(format!("data/{stem}.{ext}"))
            }
        })
        .prop_filter("the stem itself must not look precompressed", |p| !is_precompressed_name(p))
}

/// Any relative path at all.
fn any_path() -> impl Strategy<Value = PathBuf> {
    prop_oneof![plain_path(), precompressed_path()]
}

/// File sizes spanning both floors, the exact boundaries, and the whole `u64`
/// range so nothing depends on a size fitting somewhere convenient.
fn file_sizes() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => 0u64..=200_000u64,
        1 => any::<u64>(),
        1 => prop_oneof![
            Just(0u64),
            Just(4095u64),
            Just(4096u64),
            Just(4097u64),
            Just(65_535u64),
            Just(65_536u64),
            Just(65_537u64),
            Just(u64::MAX),
        ],
    ]
}

proptest! {
    // `failure_persistence: None`: an integration test has no `src` directory
    // for proptest to keep a `.proptest-regressions` file beside, and left on
    // it warns and writes a stray file into `tests/`. The floors and their
    // exact boundaries are explicit `Just` cases below instead.
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Nothing at or below the floor is ever queued for compression.
    ///
    /// Note the boundary: the floor itself is excluded, because a file exactly
    /// the size of one sector cannot free one. A `<` where the code has `<=`
    /// would pass every hand-written example and still schedule work that
    /// provably saves nothing.
    #[test]
    fn nothing_at_or_below_the_floor_is_compressed(
        path in any_path(),
        size in file_sizes(),
        opts in floors(),
    ) {
        let action = decide(&path, size, &opts);
        if size <= opts.min_size {
            prop_assert!(
                !action.is_compress(),
                "{path:?} at {size} bytes is under the {} floor",
                opts.min_size
            );
            prop_assert_eq!(action, Action::SkipTiny, "and the reason shown must say so");
        }
    }

    /// A name that says the contents are already compressed is never
    /// compressed, at any size and under either floor.
    ///
    /// Size must not be able to override the extension. It would be a natural
    /// mistake to decide that a big enough archive is worth a try anyway, and
    /// large media files are exactly the ones where rewriting every extent
    /// costs the most and gains the least.
    #[test]
    fn a_precompressed_name_is_never_compressed(
        path in precompressed_path(),
        size in file_sizes(),
        opts in floors(),
    ) {
        let action = decide(&path, size, &opts);
        prop_assert!(!action.is_compress(), "{path:?} is already compressed");
        prop_assert!(
            matches!(action, Action::SkipTiny | Action::SkipPrecompressed),
            "and it is skipped for one of the two stated reasons, not silently"
        );
    }

    /// The native floor selects a superset of what the pack floor selects.
    ///
    /// The two tiers differ only in where the floor sits, and the native one
    /// sits lower because there is no per-file store object to pay for. So any
    /// file worth packing must also be worth compressing in place. If this
    /// ever inverted, switching a drive from ext4 to btrfs would make the tool
    /// compress *fewer* files, which is the opposite of what the tiering
    /// promises.
    #[test]
    fn the_native_floor_keeps_everything_the_pack_floor_keeps(
        path in any_path(),
        size in file_sizes(),
    ) {
        let pack = decide(&path, size, &WalkOpts::default());
        let native = decide(&path, size, &WalkOpts::native());
        if pack.is_compress() {
            prop_assert!(
                native.is_compress(),
                "{path:?} at {size} bytes is packed but not compressed in place"
            );
        }
    }

    /// An ordinary file above the floor is compressed.
    ///
    /// The other properties are all prohibitions, and a `decide` that returned
    /// `SkipTiny` for everything would satisfy every one of them while doing
    /// nothing at all. This is the claim that the function still says yes.
    #[test]
    fn an_ordinary_file_above_the_floor_is_compressed(
        path in plain_path(),
        opts in floors(),
        over in 1u64..=1_000_000u64,
    ) {
        let size = opts.min_size.saturating_add(over);
        prop_assert_eq!(
            decide(&path, size, &opts),
            Action::Compress,
            "an ordinary {}-byte file should be compressed",
            size
        );
    }

    /// Every decision carries a reason the user can be shown.
    ///
    /// The Games page lists skipped files with a cause. A blank or duplicated
    /// reason turns "12,431 files skipped" into something nobody can act on.
    #[test]
    fn every_decision_explains_itself(path in any_path(), size in file_sizes(), opts in floors()) {
        let reason = decide(&path, size, &opts).reason();
        prop_assert!(!reason.is_empty(), "a decision with no reason cannot be displayed");
    }
}
