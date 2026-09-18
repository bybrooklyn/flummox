//! Lossless IPC paths. Ordinary UTF-8 remains readable; other paths use bytes.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    match path.to_str() {
        Some(text) => text.serialize(serializer),
        #[cfg(unix)]
        None => path.as_os_str().as_bytes().serialize(serializer),
        #[cfg(windows)]
        None => Wide {
            wide: path.as_os_str().encode_wide().collect(),
        }
        .serialize(serializer),
    }
}

#[cfg(windows)]
#[derive(Serialize, Deserialize)]
struct Wide {
    wide: Vec<u16>,
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Encoded {
        Text(String),
        #[cfg(unix)]
        Bytes(Vec<u8>),
        #[cfg(windows)]
        Wide(Wide),
    }
    Ok(match Encoded::deserialize(deserializer)? {
        Encoded::Text(text) => text.into(),
        #[cfg(unix)]
        Encoded::Bytes(bytes) => std::ffi::OsString::from_vec(bytes).into(),
        #[cfg(windows)]
        Encoded::Wide(encoded) => std::ffi::OsString::from_wide(&encoded.wide).into(),
    })
}

#[cfg(unix)]
pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match path {
            Some(path) => serializer.serialize_some(&EncodedRef(path)),
            None => serializer.serialize_none(),
        }
    }

    struct EncodedRef<'a>(&'a Path);

    impl Serialize for EncodedRef<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            super::serialize(self.0, serializer)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Encoded {
            Text(String),
            #[cfg(unix)]
            Bytes(Vec<u8>),
            #[cfg(windows)]
            Wide(Wide),
        }
        Ok(
            Option::<Encoded>::deserialize(deserializer)?.map(|encoded| match encoded {
                Encoded::Text(text) => text.into(),
                #[cfg(unix)]
                Encoded::Bytes(bytes) => std::ffi::OsString::from_vec(bytes).into(),
                #[cfg(windows)]
                Encoded::Wide(encoded) => std::ffi::OsString::from_wide(&encoded.wide).into(),
            }),
        )
    }
}
