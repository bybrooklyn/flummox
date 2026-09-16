//! Works out which filesystem an install directory sits on, and therefore how
//! it can be compressed.
//!
//! The filesystem type comes from `/proc/self/mountinfo` by longest-prefix
//! match, cross-checked against the `statfs` magic. `st_dev` would not work:
//! btrfs gives every subvolume its own device number, so a subvolume would
//! never match its mount entry.

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `statfs` magic numbers for the filesystems we have an opinion about.
pub mod magic {
    /// btrfs.
    pub const BTRFS: i64 = 0x9123_683E;
    /// bcachefs.
    pub const BCACHEFS: i64 = 0xca45_1a4e;
    /// ext2/3/4.
    pub const EXT4: i64 = 0xEF53;
    /// XFS.
    pub const XFS: i64 = 0x5846_5342;
    /// F2FS.
    pub const F2FS: i64 = 0xF2F5_2010;
    /// ZFS (the Linux port's fake magic).
    pub const ZFS: i64 = 0x2fc1_2fc1;
    /// tmpfs.
    pub const TMPFS: i64 = 0x0102_1994;
    /// overlayfs.
    pub const OVERLAYFS: i64 = 0x794c_7630;
    /// Any FUSE filesystem.
    pub const FUSE: i64 = 0x6573_5546;
    /// NFS.
    pub const NFS: i64 = 0x6969;
    /// SMB1/CIFS.
    pub const CIFS: i64 = 0xFF53_4D42;
    /// SMB2+.
    pub const SMB2: i64 = 0xFE53_4D42;
    /// SquashFS.
    pub const SQUASHFS: i64 = 0x7371_7368;
    /// EROFS.
    pub const EROFS: i64 = 0xE0F5_E1E2;
    /// exFAT.
    pub const EXFAT: i64 = 0x2011_BAB0;
    /// VFAT.
    pub const MSDOS: i64 = 0x4d44;
    /// NTFS via the in-kernel ntfs3 driver.
    pub const NTFS3: i64 = 0x7366_744e;
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Where it is mounted.
    pub mountpoint: PathBuf,
    /// Filesystem type, e.g. `btrfs`.
    pub fstype: String,
    /// Backing device or source, e.g. `/dev/nvme0n1p2`.
    pub source: String,
    /// Per-mount options (the ones before the ` - ` separator).
    pub options: Vec<String>,
    /// Per-superblock options (after the fstype), where `compress=zstd:1`
    /// lives.
    pub super_options: Vec<String>,
}

impl MountEntry {
    /// Whether the mount is read-only.
    pub fn read_only(&self) -> bool {
        self.options.iter().any(|o| o == "ro")
    }
}

/// What we know about the filesystem holding a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsInfo {
    /// Filesystem type name.
    pub fstype: String,
    /// `statfs` magic, for cross-checking and for types not in mountinfo.
    pub magic: i64,
    /// The mount point this path belongs to.
    pub mountpoint: PathBuf,
    /// Backing device or source.
    pub source: String,
    /// Mount and superblock options, concatenated.
    pub options: Vec<String>,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl FsInfo {
    /// The mount's transparent-compression setting, if any.
    ///
    /// Returns the algorithm and its level, e.g. `compress=zstd:1` →
    /// `("zstd", Some(1))`. Both `compress=` and `compress-force=` count.
    /// This matters for estimates: on such a mount the files are *already*
    /// compressed at that level, so the gain from recompressing is smaller
    /// than the raw ratio suggests.
    pub fn mount_compression(&self) -> Option<(String, Option<i32>)> {
        self.options.iter().find_map(|opt| {
            let value = opt
                .strip_prefix("compress-force=")
                .or_else(|| opt.strip_prefix("compress="))?;
            let (algo, level) = match value.split_once(':') {
                Some((a, l)) => (a, l.parse().ok()),
                None => (value, None),
            };
            Some((algo.to_owned(), level))
        })
    }
}

/// Which compression backend handles a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// btrfs, via the defrag ioctl.
    Btrfs,
    /// bcachefs, via per-file options.
    Bcachefs,
    /// A flummox pack store plus a FUSE mount.
    Pack,
}

impl BackendKind {
    /// The name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Btrfs => "btrfs",
            Self::Bcachefs => "bcachefs",
            Self::Pack => "pack",
        }
    }
}

/// How a filesystem can be compressed, if at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier {
    /// The filesystem compresses files itself; nothing has to be mounted.
    Native(BackendKind),
    /// Needs the pack store and a FUSE mount.
    Pack,
    /// Not supported, with a reason to show the user.
    Unsupported(&'static str),
}

impl Tier {
    /// The backend this tier selects, if it is supported at all.
    pub fn backend(&self) -> Option<BackendKind> {
        match self {
            Self::Native(kind) => Some(*kind),
            Self::Pack => Some(BackendKind::Pack),
            Self::Unsupported(_) => None,
        }
    }
}

/// Decides the tier for a filesystem.
///
/// Reasons are written for the Drives page, so they say what the user can do
/// about it where there is something to do.
pub fn tier_for(fs: &FsInfo) -> Tier {
    if fs.read_only {
        return Tier::Unsupported("mounted read-only");
    }
    match fs.fstype.as_str() {
        "btrfs" => Tier::Native(BackendKind::Btrfs),
        "bcachefs" => Tier::Native(BackendKind::Bcachefs),
        "ext2" | "ext3" | "ext4" | "xfs" | "f2fs" => Tier::Pack,
        // ZFS compresses per dataset and setting it needs root, so the pack
        // store is the only thing we can do unprivileged.
        "zfs" => Tier::Pack,
        "tmpfs" => Tier::Unsupported("RAM-backed: compressing it saves nothing"),
        "overlay" => Tier::Unsupported("stacked filesystem"),
        "fuse" | "fuseblk" => {
            Tier::Unsupported("already a FUSE filesystem (e.g. ntfs-3g); stacking is unsafe")
        }
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" => {
            Tier::Unsupported("network filesystem: too slow and unsafe to lock")
        }
        "squashfs" | "erofs" | "iso9660" => Tier::Unsupported("read-only image"),
        "exfat" | "vfat" | "msdos" => Tier::Unsupported("no POSIX permissions or symlinks"),
        "ntfs3" | "ntfs" => Tier::Unsupported("NTFS: case-insensitive, no POSIX permissions"),
        _ => Tier::Unsupported("unrecognised filesystem"),
    }
}

/// Reads and parses `/proc/self/mountinfo`.
pub fn mounts() -> io::Result<Vec<MountEntry>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")?;
    Ok(parse_mountinfo(&text))
}

/// Parses the contents of a `mountinfo` file.
///
/// Malformed lines are skipped rather than failing the scan.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines().filter_map(parse_mountinfo_line).collect()
}

fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    // 36 35 98:0 /mnt1 /mnt2 rw,noatime - ext4 /dev/sda1 rw,errors=continue
    let (before, after) = line.split_once(" - ")?;
    let mut fields = before.split(' ');
    let mountpoint = fields.nth(4).map(unescape_octal)?;
    let options = fields
        .next()
        .map(|o| o.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let mut rest = after.split(' ');
    let fstype = rest.next()?.to_owned();
    let source = rest.next().map(unescape_octal).unwrap_or_default();
    let super_options: Vec<String> = rest
        .next()
        .map(|o| o.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    Some(MountEntry {
        mountpoint: PathBuf::from(mountpoint),
        fstype,
        source,
        options,
        super_options,
    })
}

/// Undoes mountinfo's octal escaping of space, tab, newline and backslash.
fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u32::from_str_radix(&digits, 8).ok().and_then(char::from_u32) {
            Some(decoded) if digits.len() == 3 => {
                out.push(decoded);
                let _ = chars.nth(2);
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// Probes the filesystem holding `path`.
pub fn probe(path: &Path) -> io::Result<FsInfo> {
    let canonical = path.canonicalize()?;
    let magic = statfs_magic(&canonical)?;
    let entry = mounts()?
        .into_iter()
        .filter(|m| canonical.starts_with(&m.mountpoint))
        .max_by_key(|m| m.mountpoint.as_os_str().len());
    Ok(match entry {
        Some(m) => {
            let mut options = m.options.clone();
            options.extend(m.super_options.iter().cloned());
            FsInfo {
                fstype: m.fstype,
                magic,
                mountpoint: m.mountpoint,
                source: m.source,
                read_only: m.options.iter().any(|o| o == "ro"),
                options,
            }
        }
        // No mountinfo entry (a stripped container, say): fall back to magic.
        None => FsInfo {
            fstype: fstype_from_magic(magic).unwrap_or("unknown").to_owned(),
            magic,
            mountpoint: PathBuf::from("/"),
            source: String::new(),
            options: Vec::new(),
            read_only: false,
        },
    })
}

/// The filesystem type name for a `statfs` magic, where we know it.
pub fn fstype_from_magic(magic: i64) -> Option<&'static str> {
    Some(match magic {
        magic::BTRFS => "btrfs",
        magic::BCACHEFS => "bcachefs",
        magic::EXT4 => "ext4",
        magic::XFS => "xfs",
        magic::F2FS => "f2fs",
        magic::ZFS => "zfs",
        magic::TMPFS => "tmpfs",
        magic::OVERLAYFS => "overlay",
        magic::FUSE => "fuse",
        magic::NFS => "nfs",
        magic::CIFS | magic::SMB2 => "cifs",
        magic::SQUASHFS => "squashfs",
        magic::EROFS => "erofs",
        magic::EXFAT => "exfat",
        magic::MSDOS => "vfat",
        magic::NTFS3 => "ntfs3",
        _ => return None,
    })
}

#[cfg(test)]
mod snapshot_tests {
    // Kept separate from the module's main `tests` block purely so the
    // snapshot detector's fixtures stay self-contained.
    use std::path::PathBuf;

    use crate::testutil::{Ctx, TestResult, check, check_eq};

    use super::{FsInfo, magic, snapshot_risk_in};

    fn btrfs_at(mountpoint: PathBuf) -> FsInfo {
        FsInfo {
            fstype: "btrfs".to_owned(),
            magic: magic::BTRFS,
            mountpoint,
            source: "/dev/nvme0n1p2".to_owned(),
            options: Vec::new(),
            read_only: false,
        }
    }

    #[test]
    fn a_dot_snapshots_directory_is_a_warning() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let fs = btrfs_at(tmp.path().to_path_buf());
        check(
            snapshot_risk_in(&fs, tmp.path()).is_none(),
            "a plain subvolume should not warn",
        )?;
        std::fs::create_dir(tmp.path().join(".snapshots")).ctx("create .snapshots")?;
        let why = snapshot_risk_in(&fs, tmp.path()).ctx("expected a warning")?;
        check(why.contains(".snapshots"), "the warning should name what it found")
    }

    #[test]
    fn a_snapper_config_covering_the_mount_is_a_warning() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        let configs = tmp.path().join("etc/snapper/configs");
        std::fs::create_dir_all(&configs).ctx("create the snapper config directory")?;
        // Mount points are fake paths under the fixture: using the real "/"
        // made this test depend on whether the machine running it happens to
        // have /.snapshots, which is exactly what the other branch detects.
        let covered = tmp.path().join("covered");
        let bare = tmp.path().join("bare");
        // This mirrors this machine's real layout: one config covering a
        // single subvolume, while games live on another that nothing
        // snapshots.
        std::fs::write(
            configs.join("root"),
            format!("SUBVOLUME=\"{}\"\nTIMELINE_CREATE=\"no\"\n", covered.display()),
        )
        .ctx("write the snapper config")?;

        check(
            snapshot_risk_in(&btrfs_at(bare), tmp.path()).is_none(),
            "a config for another subvolume must not warn about this one",
        )?;

        let why = snapshot_risk_in(&btrfs_at(covered), tmp.path())
            .ctx("expected a warning for the covered subvolume")?;
        check(why.contains("snapper"), "the warning should mention snapper")
    }

    #[test]
    fn filesystems_without_snapshots_never_warn() -> TestResult {
        let tmp = tempfile::tempdir().ctx("temporary directory")?;
        std::fs::create_dir(tmp.path().join(".snapshots")).ctx("create .snapshots")?;
        let ext4 = FsInfo { fstype: "ext4".to_owned(), ..btrfs_at(tmp.path().to_path_buf()) };
        check_eq(snapshot_risk_in(&ext4, tmp.path()), None, "ext4 does not share extents")
    }
}

/// Warns when the subvolume holding a directory looks snapshotted.
///
/// Compressing rewrites every extent, and on most kernels that breaks the
/// sharing between a subvolume and its snapshots: the old copy stays pinned
/// in the snapshot while the new one is written, so the drive can end up
/// *fuller* than before. Snapshots are found without root by looking for a
/// `.snapshots` directory at the mount point and for a snapper config
/// covering it.
pub fn snapshot_risk(fs: &FsInfo) -> Option<String> {
    snapshot_risk_in(fs, Path::new("/"))
}

/// [`snapshot_risk`], with the system root injectable for tests.
pub fn snapshot_risk_in(fs: &FsInfo, sysroot: &Path) -> Option<String> {
    // Only copy-on-write filesystems share extents with snapshots.
    if fs.fstype != "btrfs" && fs.fstype != "bcachefs" {
        return None;
    }
    let dot_snapshots = fs.mountpoint.join(".snapshots");
    if dot_snapshots.is_dir() {
        return Some(format!(
            "{} exists, so this drive is being snapshotted",
            dot_snapshots.display()
        ));
    }
    let configs = sysroot.join("etc/snapper/configs");
    let entries = std::fs::read_dir(&configs).ok()?;
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let covers = text.lines().any(|line| {
            line.trim()
                .strip_prefix("SUBVOLUME=")
                .map(|v| Path::new(v.trim().trim_matches('"')) == fs.mountpoint)
                .unwrap_or(false)
        });
        if covers {
            return Some(format!(
                "snapper config {:?} covers {}",
                entry.file_name(),
                fs.mountpoint.display()
            ));
        }
    }
    None
}

/// Returns the `statfs` magic for a path.
pub fn statfs_magic(path: &Path) -> io::Result<i64> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut buf = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c_path` is a valid NUL-terminated string and `buf` is a
    // correctly sized, writable `statfs` allocation.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs returned 0, so it initialised the struct.
    let stat = unsafe { buf.assume_init() };
    // `f_type` is `__fsword_t`: already i64 on x86_64, but i32 on 32-bit
    // targets. Converting keeps both correct, and the lint is allowed because
    // the conversion is only redundant on the 64-bit half of that.
    #[allow(clippy::useless_conversion)]
    let magic = i64::try_from(stat.f_type).unwrap_or_default();
    Ok(magic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    const REAL_LINES: &str = "\
26 1 0:24 / / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,ssd,subvol=/@
31 26 0:24 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,subvol=/@home
44 26 259:1 / /boot rw,relatime shared:3 - vfat /dev/nvme0n1p1 rw,fmask=0022
99 26 8:17 / /run/media/brook/my\\040disk ro,nosuid shared:9 - ext4 /dev/sdb1 ro";

    #[test]
    fn parses_mountinfo_including_escaped_paths() -> TestResult {
        let mounts = parse_mountinfo(REAL_LINES);
        check_eq(mounts.len(), 4, "every fixture line should parse")?;
        let home = mounts
            .iter()
            .find(|m| m.mountpoint == Path::new("/home"))
            .ctx("find the /home mount")?;
        check_eq(home.fstype.as_str(), "btrfs", "the /home filesystem type")?;
        check_eq(home.source.as_str(), "/dev/nvme0n1p2", "the /home backing device")?;
        check(
            home.super_options.iter().any(|o| o == "compress=zstd:1"),
            "the superblock options should carry the compression setting",
        )?;
        let removable = mounts.get(3).ctx("the fourth fixture mount")?;
        check_eq(
            removable.mountpoint.as_path(),
            Path::new("/run/media/brook/my disk"),
            "the octal escape should be decoded back to a space",
        )?;
        check(removable.read_only(), "a mount with the ro option is read-only")
    }

    #[test]
    fn mount_compression_reads_the_level() -> TestResult {
        let fs = FsInfo {
            fstype: "btrfs".to_owned(),
            magic: magic::BTRFS,
            mountpoint: PathBuf::from("/home"),
            source: String::new(),
            options: vec!["rw".to_owned(), "compress=zstd:1".to_owned()],
            read_only: false,
        };
        check_eq(
            fs.mount_compression(),
            Some(("zstd".to_owned(), Some(1))),
            "compress=zstd:1 gives both the algorithm and the level",
        )?;

        let forced = FsInfo {
            options: vec!["compress-force=lzo".to_owned()],
            ..fs.clone()
        };
        check_eq(
            forced.mount_compression(),
            Some(("lzo".to_owned(), None)),
            "compress-force counts too, and needs no level",
        )?;

        let plain = FsInfo { options: vec!["rw".to_owned()], ..fs };
        check_eq(
            plain.mount_compression(),
            None,
            "a mount without a compress option has no compression",
        )
    }

    #[test]
    fn tiers_match_the_plan() -> TestResult {
        let fs = |fstype: &str| FsInfo {
            fstype: fstype.to_owned(),
            magic: 0,
            mountpoint: PathBuf::from("/"),
            source: String::new(),
            options: Vec::new(),
            read_only: false,
        };
        check_eq(
            tier_for(&fs("btrfs")),
            Tier::Native(BackendKind::Btrfs),
            "btrfs compresses itself",
        )?;
        check_eq(
            tier_for(&fs("bcachefs")),
            Tier::Native(BackendKind::Bcachefs),
            "bcachefs compresses itself",
        )?;
        check_eq(tier_for(&fs("ext4")), Tier::Pack, "ext4 needs the pack store")?;
        check_eq(tier_for(&fs("xfs")), Tier::Pack, "xfs needs the pack store")?;
        check_eq(tier_for(&fs("f2fs")), Tier::Pack, "f2fs needs the pack store")?;
        check(
            matches!(tier_for(&fs("nfs")), Tier::Unsupported(_)),
            "a network filesystem is unsupported",
        )?;
        check(
            matches!(tier_for(&fs("exfat")), Tier::Unsupported(_)),
            "exfat is unsupported",
        )?;
        // Read-only wins over the type.
        let ro = FsInfo { read_only: true, ..fs("btrfs") };
        check(
            matches!(tier_for(&ro), Tier::Unsupported(_)),
            "a read-only mount is unsupported even on btrfs",
        )
    }

    #[test]
    fn probes_this_machine() -> TestResult {
        let info = probe(Path::new(".")).ctx("probe the current directory")?;
        check(!info.fstype.is_empty(), "the probe should name a filesystem")?;
        // Whatever this runs on, the magic and the name must agree when we
        // know the name for that magic.
        if let Some(name) = fstype_from_magic(info.magic) {
            check_eq(name, info.fstype.as_str(), "the magic and the mountinfo name must agree")?;
        }
        Ok(())
    }
}
