//! Bounded format inspection for sampling and chunk-boundary hints.
//!
//! Inspection never extracts, decrypts, or rewrites a payload. Unknown and
//! protected data still receives distributed sampling.

/// A structural hint, separate from whether content actually compresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Unknown,
    RawTexture,
    RawMedia,
    BlockTexture,
    StoredArchive,
    Encoded,
    MixedContainer,
    Executable,
    Malformed,
}

impl Format {
    /// Encoded payloads get fewer distributed samples, never an automatic skip.
    pub fn cheap_sample(self) -> bool {
        matches!(self, Self::Encoded | Self::BlockTexture)
    }
}

/// The recognized family, suitable for explanations and future parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Family {
    #[default]
    Unknown,
    Dds,
    Zip,
    SevenZip,
    Rar,
    Gzip,
    Xz,
    Zstandard,
    Bzip2,
    Lz4,
    Cab,
    Wim,
    Squashfs,
    Psarc,
    CriCpk,
    Unity,
    UnrealIoStore,
    ValveVpk,
    Wad,
    Bethesda,
    Godot,
    BlizzardBlte,
    CriUsm,
    FmodFsb,
    XactWaveBank,
    Pe,
    Elf,
    Riff,
    Wav,
    Avi,
    Webp,
    Png,
    Jpeg,
    Ogg,
    Flac,
    Mp3,
    Bink,
    Mp4,
    Webm,
    Ktx,
    Astc,
    Pvr,
}

impl Family {
    /// Stable name used in diagnostics and measurement records.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown data",
            Self::Dds => "DDS texture",
            Self::Zip => "ZIP/PK3/PK4 archive",
            Self::SevenZip => "7z archive",
            Self::Rar => "RAR archive",
            Self::Gzip => "gzip stream",
            Self::Xz => "xz stream",
            Self::Zstandard => "zstd stream",
            Self::Bzip2 => "bzip2 stream",
            Self::Lz4 => "LZ4 stream",
            Self::Cab => "Microsoft Cabinet archive",
            Self::Wim => "Windows Imaging archive",
            Self::Squashfs => "SquashFS image",
            Self::Psarc => "PlayStation archive",
            Self::CriCpk => "CRI CPK archive",
            Self::Unity => "Unity bundle",
            Self::UnrealIoStore => "Unreal IoStore",
            Self::ValveVpk => "Valve VPK",
            Self::Wad => "WAD archive",
            Self::Bethesda => "Bethesda archive",
            Self::Godot => "Godot package",
            Self::BlizzardBlte => "Blizzard BLTE data",
            Self::CriUsm => "CRI USM video",
            Self::FmodFsb => "FMOD sound bank",
            Self::XactWaveBank => "XACT wave bank",
            Self::Pe => "Windows executable",
            Self::Elf => "ELF executable",
            Self::Riff => "RIFF container",
            Self::Wav => "WAVE audio",
            Self::Avi => "AVI media",
            Self::Webp => "WebP image",
            Self::Png => "PNG image",
            Self::Jpeg => "JPEG image",
            Self::Ogg => "Ogg media",
            Self::Flac => "FLAC audio",
            Self::Mp3 => "MP3 audio",
            Self::Bink => "Bink video",
            Self::Mp4 => "MP4 media",
            Self::Webm => "WebM media",
            Self::Ktx => "KTX texture",
            Self::Astc => "ASTC texture",
            Self::Pvr => "PowerVR texture",
        }
    }
}

/// What the visible header says about payload access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protection {
    #[default]
    None,
    EncryptedMember,
    EncodedPayload,
}

/// Bounded evidence returned by the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Inspection {
    pub format: Format,
    pub family: Family,
    pub protection: Protection,
    /// First payload offset when the header provides one without allocation.
    pub member_offset: Option<u32>,
}

impl Inspection {
    fn new(format: Format, family: Family) -> Self {
        Self {
            format,
            family,
            protection: Protection::None,
            member_offset: None,
        }
    }

    fn encoded(family: Family) -> Self {
        Self {
            format: Format::Encoded,
            family,
            protection: Protection::EncodedPayload,
            member_offset: None,
        }
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn dds(bytes: &[u8]) -> Inspection {
    if bytes.len() < 128 || u32_at(bytes, 4) != Some(124) || u32_at(bytes, 76) != Some(32) {
        return Inspection::new(Format::Malformed, Family::Dds);
    }
    let flags = u32_at(bytes, 80).unwrap_or(0);
    if flags & 4 == 0 {
        return Inspection::new(
            if flags & (0x40 | 0x20000 | 2) != 0 {
                Format::RawTexture
            } else {
                Format::Unknown
            },
            Family::Dds,
        );
    }
    let format = match bytes.get(84..88) {
        Some(
            b"DXT1" | b"DXT2" | b"DXT3" | b"DXT4" | b"DXT5" | b"ATI1" | b"ATI2" | b"BC4U" | b"BC4S"
            | b"BC5U" | b"BC5S",
        ) => Format::BlockTexture,
        Some(b"DX10") if bytes.len() < 148 => Format::Malformed,
        Some(b"DX10") => match u32_at(bytes, 128) {
            Some(70..=84 | 94..=99) => Format::BlockTexture,
            Some(1..=69 | 85..=93) => Format::RawTexture,
            _ => Format::Unknown,
        },
        _ => Format::Unknown,
    };
    Inspection::new(format, Family::Dds)
}

fn zip(bytes: &[u8]) -> Inspection {
    if bytes.len() < 30 {
        return Inspection::new(Format::Malformed, Family::Zip);
    }
    let encrypted = u16_at(bytes, 6).is_some_and(|flags| flags & 1 != 0);
    let method = u16_at(bytes, 8);
    let name = u32::from(u16_at(bytes, 26).unwrap_or(0));
    let extra = u32::from(u16_at(bytes, 28).unwrap_or(0));
    Inspection {
        format: if !encrypted && method == Some(0) {
            Format::StoredArchive
        } else {
            Format::MixedContainer
        },
        family: Family::Zip,
        protection: if encrypted {
            Protection::EncryptedMember
        } else if method == Some(0) {
            Protection::None
        } else {
            Protection::EncodedPayload
        },
        member_offset: 30u32.checked_add(name).and_then(|n| n.checked_add(extra)),
    }
}

fn riff(bytes: &[u8]) -> Inspection {
    if bytes.len() < 12 {
        return Inspection::new(Format::Malformed, Family::Riff);
    }
    match bytes.get(8..12) {
        Some(b"WAVE") => {
            let mut offset = 12usize;
            while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
                let Some(name) = bytes.get(offset..offset + 4) else {
                    break;
                };
                let Some(length) = u32_at(bytes, offset + 4).and_then(|n| usize::try_from(n).ok())
                else {
                    break;
                };
                let payload = offset + 8;
                if name == b"fmt " {
                    let Some(codec) = u16_at(bytes, payload) else {
                        return Inspection::new(Format::Malformed, Family::Wav);
                    };
                    return if matches!(codec, 1 | 3) {
                        Inspection::new(Format::RawMedia, Family::Wav)
                    } else {
                        Inspection::encoded(Family::Wav)
                    };
                }
                let Some(next) = payload
                    .checked_add(length)
                    .and_then(|end| end.checked_add(length % 2))
                else {
                    break;
                };
                if next <= offset || next > bytes.len() {
                    break;
                }
                offset = next;
            }
            Inspection::new(Format::MixedContainer, Family::Wav)
        }
        Some(b"AVI ") => Inspection::encoded(Family::Avi),
        Some(b"WEBP") => Inspection::encoded(Family::Webp),
        _ => Inspection::new(Format::MixedContainer, Family::Riff),
    }
}

/// Inspects a caller-supplied header without reading lengths from the payload.
pub fn inspect(bytes: &[u8]) -> Inspection {
    if bytes.starts_with(b"DDS ") {
        return dds(bytes);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return zip(bytes);
    }
    if bytes.starts_with(b"RIFF") {
        return riff(bytes);
    }
    let mixed = [
        (b"UnityFS\0".as_slice(), Family::Unity),
        (b"UnityRaw\0".as_slice(), Family::Unity),
        (b"-==--==--==--==-".as_slice(), Family::UnrealIoStore),
        (&[0x34, 0x12, 0xaa, 0x55], Family::ValveVpk),
        (b"WAD2".as_slice(), Family::Wad),
        (b"WAD3".as_slice(), Family::Wad),
        (b"IWAD".as_slice(), Family::Wad),
        (b"PWAD".as_slice(), Family::Wad),
        (b"BSA\0".as_slice(), Family::Bethesda),
        (b"BTDX".as_slice(), Family::Bethesda),
        (b"GDPC".as_slice(), Family::Godot),
        (b"MSCF".as_slice(), Family::Cab),
        (b"MSWIM\0\0\0".as_slice(), Family::Wim),
        (b"hsqs".as_slice(), Family::Squashfs),
        (b"sqsh".as_slice(), Family::Squashfs),
        (b"PSAR".as_slice(), Family::Psarc),
        (b"CPK ".as_slice(), Family::CriCpk),
    ];
    if let Some((_, family)) = mixed.iter().find(|(magic, _)| bytes.starts_with(magic)) {
        return Inspection::new(Format::MixedContainer, *family);
    }
    if bytes.starts_with(b"MZ") {
        return Inspection::new(Format::Executable, Family::Pe);
    }
    if bytes.starts_with(b"\x7fELF") {
        return Inspection::new(Format::Executable, Family::Elf);
    }
    let encoded = [
        (b"7z\xbc\xaf\x27\x1c".as_slice(), Family::SevenZip),
        (b"Rar!\x1a\x07".as_slice(), Family::Rar),
        (b"\x1f\x8b".as_slice(), Family::Gzip),
        (b"\xfd7zXZ\0".as_slice(), Family::Xz),
        (b"\x28\xb5\x2f\xfd".as_slice(), Family::Zstandard),
        (b"BZh".as_slice(), Family::Bzip2),
        (b"\x04\x22\x4d\x18".as_slice(), Family::Lz4),
        (b"BLTE".as_slice(), Family::BlizzardBlte),
        (b"CRID".as_slice(), Family::CriUsm),
        (b"FSB5".as_slice(), Family::FmodFsb),
        (b"WBND".as_slice(), Family::XactWaveBank),
        (b"\xabKTX 11\xbb\r\n\x1a\n".as_slice(), Family::Ktx),
        (b"\xabKTX 20\xbb\r\n\x1a\n".as_slice(), Family::Ktx),
        (b"\x13\xab\xa1\x5c".as_slice(), Family::Astc),
        (b"PVR\x03".as_slice(), Family::Pvr),
        (b"\x89PNG\r\n\x1a\n".as_slice(), Family::Png),
        (b"\xff\xd8\xff".as_slice(), Family::Jpeg),
        (b"OggS".as_slice(), Family::Ogg),
        (b"fLaC".as_slice(), Family::Flac),
        (b"ID3".as_slice(), Family::Mp3),
        (b"BIK".as_slice(), Family::Bink),
        (b"KB2f".as_slice(), Family::Bink),
        (b"\x1a\x45\xdf\xa3".as_slice(), Family::Webm),
    ];
    if let Some((_, family)) = encoded.iter().find(|(magic, _)| bytes.starts_with(magic)) {
        return match *family {
            Family::Astc => Inspection {
                format: Format::BlockTexture,
                family: *family,
                protection: Protection::EncodedPayload,
                member_offset: None,
            },
            // These containers can hold either encoded or raw payloads. Keep
            // the full distributed sample instead of guessing from the name.
            Family::Ktx | Family::Pvr | Family::FmodFsb | Family::XactWaveBank => {
                Inspection::new(Format::MixedContainer, *family)
            }
            _ => Inspection::encoded(*family),
        };
    }
    if bytes.get(4..8) == Some(b"ftyp") {
        return Inspection::encoded(Family::Mp4);
    }
    Inspection::new(Format::Unknown, Family::Unknown)
}

/// Compatibility wrapper used by the estimator.
pub fn header(bytes: &[u8]) -> Format {
    inspect(bytes).format
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};

    fn dds_bytes(flags: u32, fourcc: &[u8; 4], dxgi: u32) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0; 148];
        for (offset, value) in [
            (0, *b"DDS "),
            (4, 124u32.to_le_bytes()),
            (76, 32u32.to_le_bytes()),
            (80, flags.to_le_bytes()),
            (84, *fourcc),
            (128, dxgi.to_le_bytes()),
        ] {
            bytes
                .get_mut(offset..offset + 4)
                .ctx("header field")?
                .copy_from_slice(&value);
        }
        Ok(bytes)
    }

    #[test]
    fn distinguishes_raw_and_block_textures_and_rejects_truncation() -> TestResult {
        check_eq(
            header(&dds_bytes(0x40, b"\0\0\0\0", 0)?),
            Format::RawTexture,
            "RGB stays a candidate",
        )?;
        check_eq(
            header(&dds_bytes(4, b"DXT1", 0)?),
            Format::BlockTexture,
            "legacy BC1",
        )?;
        check_eq(
            header(&dds_bytes(4, b"DX10", 98)?),
            Format::BlockTexture,
            "DX10 BC7",
        )?;
        check_eq(
            header(&dds_bytes(4, b"DX10", 87)?),
            Format::RawTexture,
            "DX10 BGRA",
        )?;
        let full = dds_bytes(4, b"DX10", 98)?;
        for length in 4..148 {
            check_eq(
                header(full.get(..length).ctx("truncated header")?),
                Format::Malformed,
                "short DDS",
            )?;
        }
        check(
            !Format::Unknown.cheap_sample(),
            "unknown data receives full sampling",
        )
    }

    #[test]
    fn archive_encryption_remains_a_mixed_sample() -> TestResult {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.resize(30, 0);
        check_eq(header(&zip), Format::StoredArchive, "stored first entry")?;
        *zip.get_mut(6).ctx("flags")? = 1;
        let evidence = inspect(&zip);
        check_eq(
            evidence.protection,
            Protection::EncryptedMember,
            "encryption is recorded",
        )?;
        check(
            !evidence.format.cheap_sample(),
            "mixed archives receive full distributed sampling",
        )
    }

    #[test]
    fn common_game_families_have_stable_evidence() -> TestResult {
        for (bytes, family, format) in [
            (
                b"UnityFS\0".as_slice(),
                Family::Unity,
                Format::MixedContainer,
            ),
            (
                &[0x34, 0x12, 0xaa, 0x55],
                Family::ValveVpk,
                Format::MixedContainer,
            ),
            (b"BTDX".as_slice(), Family::Bethesda, Format::MixedContainer),
            (b"BLTE".as_slice(), Family::BlizzardBlte, Format::Encoded),
            (b"\x7fELF".as_slice(), Family::Elf, Format::Executable),
        ] {
            let evidence = inspect(bytes);
            check_eq(evidence.family, family, family.label())?;
            check_eq(evidence.format, format, family.label())?;
        }
        Ok(())
    }

    #[test]
    fn riff_payloads_and_additional_archives_are_classified() -> TestResult {
        let mut pcm = b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0".to_vec();
        pcm.extend_from_slice(&1u16.to_le_bytes());
        pcm.resize(36, 0);
        let evidence = inspect(&pcm);
        check_eq(evidence.family, Family::Wav, "WAVE family")?;
        check_eq(
            evidence.format,
            Format::RawMedia,
            "PCM remains compressible",
        )?;

        let mut encoded = pcm;
        let codec = encoded.get_mut(20..22).ctx("WAVE codec")?;
        codec.copy_from_slice(&2u16.to_le_bytes());
        check_eq(
            inspect(&encoded).format,
            Format::Encoded,
            "ADPCM is encoded",
        )?;
        for (magic, family) in [
            (b"MSCF".as_slice(), Family::Cab),
            (b"MSWIM\0\0\0".as_slice(), Family::Wim),
            (b"hsqs".as_slice(), Family::Squashfs),
            (b"PSAR".as_slice(), Family::Psarc),
            (b"CPK ".as_slice(), Family::CriCpk),
        ] {
            check_eq(inspect(magic).family, family, family.label())?;
        }
        for (magic, family, format) in [
            (b"CRID".as_slice(), Family::CriUsm, Format::Encoded),
            (b"FSB5".as_slice(), Family::FmodFsb, Format::MixedContainer),
            (
                b"WBND".as_slice(),
                Family::XactWaveBank,
                Format::MixedContainer,
            ),
            (
                b"\xabKTX 20\xbb\r\n\x1a\n".as_slice(),
                Family::Ktx,
                Format::MixedContainer,
            ),
            (
                b"\x13\xab\xa1\x5c".as_slice(),
                Family::Astc,
                Format::BlockTexture,
            ),
        ] {
            let evidence = inspect(magic);
            check_eq(evidence.family, family, family.label())?;
            check_eq(evidence.format, format, family.label())?;
        }
        Ok(())
    }
}
