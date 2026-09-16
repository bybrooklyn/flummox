//! Properties of the btrfs disk-cost model.
//!
//! [`BtrfsModel::disk_cost`] is the arithmetic every number the user sees is
//! built from: the estimate on the Games page, the "you will save 14 GB"
//! before a job, and the comparison against what the job actually freed. It is
//! also the one part of the estimator with no feedback loop. If it is wrong,
//! nothing downstream notices, the tool just quietly promises the wrong
//! number and loses the user's trust the first time they check with `df`.
//!
//! Examples can only pin down the three or four block sizes someone thought
//! of. What matters are the invariants that make the model *the filesystem's*
//! arithmetic rather than zstd's:
//!
//! * btrfs allocates in whole 4 KiB sectors, so a cost that is not a multiple
//!   of 4096 describes something the filesystem cannot do;
//! * compression can never make a block occupy more space than storing it raw,
//!   because btrfs falls back to raw when it would;
//! * compressing better must never cost more, or the estimator would reward
//!   the wrong zstd level;
//! * a block that saves less than one whole sector is stored uncompressed, and
//!   saves exactly nothing.

use flummox::estimate::{BtrfsModel, UnitModel};
use proptest::prelude::*;

/// btrfs's allocation granularity, restated here so the test does not simply
/// echo whatever constant the code happens to hold.
const SECTOR: u64 = 4096;

/// The size a block of `n` bytes occupies once rounded up to whole sectors.
fn rounded(n: u32) -> u64 {
    u64::from(n.div_ceil(4096)) * SECTOR
}

/// Block sizes worth trying.
///
/// Weighted towards the 0..128 KiB range the model is actually fed, but mixed
/// with the whole `u32` range so the saturating arithmetic near `u32::MAX` is
/// covered, and with the exact sector boundaries where off-by-one errors live.
fn sizes() -> impl Strategy<Value = u32> {
    prop_oneof![
        3 => 0u32..=131_072u32,
        1 => any::<u32>(),
        1 => prop_oneof![
            Just(0u32),
            Just(1u32),
            Just(4095u32),
            Just(4096u32),
            Just(4097u32),
            Just(8192u32),
            Just(131_071u32),
            Just(131_072u32),
            Just(u32::MAX),
        ],
    ]
}

proptest! {
    // Pure arithmetic with no allocation: a thousand cases costs microseconds.
    //
    // `failure_persistence: None` because an integration test has no `src`
    // directory beside it for proptest to keep a `.proptest-regressions` file
    // in; left on, it warns on every failure and drops a stray file into
    // `tests/`. The strategies below pin the interesting edges (the sector
    // boundaries and `u32::MAX`) as explicit `Just` cases, so no shrunk
    // counter-example depends on a persisted seed to be found again.
    #![proptest_config(ProptestConfig {
        cases: 1024,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Disk usage is always a whole number of sectors, and never more than
    /// storing the block raw would cost.
    ///
    /// The first half is what makes the estimate describable in the units the
    /// filesystem allocates in. The second is the safety direction: an
    /// estimate that could exceed the raw size would let the tool tell someone
    /// that compressing their library will make it *bigger*, which btrfs will
    /// never actually do because it keeps the raw block instead.
    #[test]
    fn a_block_costs_whole_sectors_and_never_more_than_raw(
        uncompressed in sizes(),
        compressed in sizes(),
    ) {
        let cost = BtrfsModel { level: 9 }.disk_cost(uncompressed, compressed);
        prop_assert_eq!(cost % SECTOR, 0, "{} is not a whole number of sectors", cost);
        prop_assert!(
            cost <= rounded(uncompressed),
            "{cost} exceeds the raw cost {}",
            rounded(uncompressed)
        );
    }

    /// Compressing a block further can never cost more disk.
    ///
    /// The estimator compares zstd levels against each other, and the Drives
    /// page offers the user a choice between them. If the cost function were
    /// not monotonic in the compressed size there would be some block where a
    /// better ratio scored worse, and the tool would recommend the weaker
    /// setting on the strength of an arithmetic artefact.
    #[test]
    fn compressing_further_never_costs_more(
        uncompressed in sizes(),
        a in sizes(),
        b in sizes(),
    ) {
        let model = BtrfsModel { level: 9 };
        let (smaller, larger) = if a <= b { (a, b) } else { (b, a) };
        prop_assert!(
            model.disk_cost(uncompressed, smaller) <= model.disk_cost(uncompressed, larger),
            "cost({smaller}) > cost({larger}) for a {uncompressed}-byte block"
        );
    }

    /// Savings smaller than one sector are not savings.
    ///
    /// This is the rule that separates this model from a naive
    /// `compressed / uncompressed` ratio, and it is why the estimator's
    /// numbers survive contact with `compsize`. Shaving 300 bytes off a 128
    /// KiB block frees nothing at all, because btrfs cannot hand back part of
    /// a sector, so it keeps the block raw. The other direction is asserted
    /// too: once the block does clear the bar, it must free at least the whole
    /// sector that made it worth taking.
    ///
    /// The "does this free a sector?" test is computed here in exact `u64`
    /// arithmetic, not by repeating the implementation's
    /// `compressed.saturating_add(SECTOR) > uncompressed`. Restating the
    /// expression would make any mistake in it invisible, which is what a
    /// property is for. The two really do disagree at
    /// `uncompressed == compressed == u32::MAX`, where the saturating add
    /// pins at `u32::MAX` and the comparison decides a block that saves
    /// nothing is worth compressing. The *cost* is the same on both branches
    /// there, so nothing is actually mispriced, and `disk_cost` is only ever
    /// called with blocks of at most 128 KiB, so the saturating range is
    /// unreachable in practice. Asserting on the returned cost rather than on
    /// which branch was taken keeps the property honest about all three
    /// facts.
    #[test]
    fn a_saving_under_one_sector_is_never_claimed(
        uncompressed in sizes(),
        compressed in sizes(),
    ) {
        let model = BtrfsModel { level: 9 };
        let cost = model.disk_cost(uncompressed, compressed);
        let frees_a_sector = u64::from(compressed) + SECTOR <= u64::from(uncompressed);
        if frees_a_sector {
            prop_assert!(
                cost.saturating_add(SECTOR) <= rounded(uncompressed),
                "a block worth compressing must free at least one whole sector"
            );
        } else {
            prop_assert_eq!(
                cost,
                rounded(uncompressed),
                "a block saving under a sector must cost what storing it raw costs"
            );
        }
    }

    /// The block size the model advertises is the unit its costs are about.
    ///
    /// Sampling slices files into `block_size()` chunks and then prices each
    /// one with `disk_cost`. If the advertised unit were larger than what the
    /// model actually prices, every estimate would be scaled by the ratio
    /// between them.
    #[test]
    fn a_full_block_of_incompressible_data_costs_exactly_the_block(level in -5i32..=22i32) {
        let model = BtrfsModel { level };
        let block = model.block_size();
        prop_assert_eq!(
            model.disk_cost(block, block),
            u64::from(block),
            "a 128 KiB block that does not shrink costs 128 KiB"
        );
        prop_assert_eq!(model.level(), level, "the model reports the level it was built with");
    }
}
