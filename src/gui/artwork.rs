//! Local artwork selection, bounded decoding, and owner-local overrides.
use anyhow::{Context, Result, ensure};
use iced::widget::image::Handle;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Write},
    path::PathBuf,
    time::SystemTime,
};

/// An image file plus the stamp it had when inspected.
///
/// The stamp is part of equality, so a file that changes on disk becomes a
/// different cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub path: PathBuf,
    modified: Option<SystemTime>,
    bytes: u64,
    /// Longest side of the decoded thumbnail, in pixels.
    edge: u32,
}
impl Source {
    /// Stamps a regular file for a row icon. Returns `None` for anything else.
    pub fn inspect(path: PathBuf) -> Option<Self> {
        let metadata = std::fs::metadata(&path).ok()?;
        metadata.is_file().then(|| Self {
            path,
            modified: metadata.modified().ok(),
            bytes: metadata.len(),
            edge: 128,
        })
    }
    /// Decodes a thumbnail no larger than `edge` on either side.
    ///
    /// Blocks on file I/O, so call it off the window thread. Fails when the
    /// file is over 32 MiB, exceeds the decoder limits, or changed since it
    /// was inspected.
    pub fn decode(&self) -> Result<Handle> {
        ensure!(self.bytes <= 32 * 1024 * 1024, "Artwork exceeds 32 MiB");
        let mut reader = image::ImageReader::open(&self.path)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let pixels = reader.decode()?.thumbnail(self.edge, self.edge).to_rgba8();
        // A file replaced during the decode would otherwise be cached under
        // the old stamp.
        ensure!(
            Self::inspect(self.path.clone())
                .is_some_and(|fresh| fresh.modified == self.modified && fresh.bytes == self.bytes),
            "Artwork changed while loading"
        );
        Ok(Handle::from_rgba(
            pixels.width(),
            pixels.height(),
            pixels.into_raw(),
        ))
    }
}
/// One user-chosen image, keyed by the game's id string.
#[derive(Debug, Serialize, Deserialize)]
struct Override {
    game: String,
    #[serde(with = "crate::path_serde")]
    path: PathBuf,
}
/// Reads `artwork.json` from the data directory. A missing file is an empty
/// list; a file over 1 MiB is an error.
fn overrides() -> Result<Vec<Override>> {
    let path = crate::libraries::data_dir()?.join("artwork.json");
    match std::fs::File::open(path) {
        Ok(file) => {
            let mut bytes = vec![];
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "Artwork preferences exceed 1 MiB"
            );
            Ok(serde_json::from_slice(&bytes)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(error) => Err(error.into()),
    }
}
/// Records `path` as the artwork for `game`, replacing any earlier choice.
///
/// The image must decode within the usual limits before anything is written.
/// Blocks on file I/O.
pub fn save_override(game: String, path: PathBuf) -> Result<()> {
    Source::inspect(path.clone())
        .context("Artwork file unavailable")?
        .decode()?;
    let mut items = overrides()?;
    items.retain(|item| item.game != game);
    items.push(Override { game, path });
    write_overrides(&items)
}
/// Removes the saved image for a game, so it goes back to its default
/// artwork. Blocks on file I/O.
#[cfg(target_os = "linux")]
pub fn clear_override(game: &str) -> Result<()> {
    let mut items = overrides()?;
    items.retain(|item| item.game != game);
    write_overrides(&items)
}
/// Writes the list through a temporary file in the same directory, renamed
/// over the old one, so a reader never sees a partial list.
fn write_overrides(items: &[Override]) -> Result<()> {
    let root = crate::libraries::data_dir()?;
    crate::libraries::private_dir(&root)?;
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    serde_json::to_writer(&mut file, items)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(root.join("artwork.json"))?;
    Ok(())
}
/// Which image file each game uses, built once per scan.
pub struct Index {
    /// Row icon per Steam app id, with its priority. Lower wins.
    steam: HashMap<u32, (u8, Source)>,
    /// User-chosen images by game id string. These beat Steam's.
    overrides: HashMap<String, Source>,
    /// Detail-pane cover per Steam app id, with its priority. Lower wins.
    covers: HashMap<u32, (u8, Source)>,
}
impl Index {
    /// Reads the saved overrides and lists `appcache/librarycache` under each
    /// Steam root. Unreadable overrides or caches are skipped. Blocks on file
    /// I/O.
    pub fn new(roots: Vec<PathBuf>) -> Result<Self> {
        let mut index = Self {
            steam: HashMap::new(),
            overrides: HashMap::new(),
            covers: HashMap::new(),
        };
        for item in overrides().unwrap_or_else(|error| {
            tracing::warn!(%error, "Local artwork preferences could not be read");
            vec![]
        }) {
            if let Some(source) = Source::inspect(item.path) {
                index.overrides.insert(item.game, source);
            }
        }
        for root in roots {
            let cache = root.join("appcache/librarycache");
            let Ok(entries) = std::fs::read_dir(cache) else {
                continue;
            };
            let mut paths: Vec<_> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            paths.sort();
            // Two layouts are read: a directory named after the app id that
            // holds the images, and flat files named `<appid>_<role>.<ext>`.
            for path in paths {
                if let Some(app) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.parse::<u32>().ok())
                {
                    if let Ok(entries) = std::fs::read_dir(&path) {
                        let mut images: Vec<_> = entries
                            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                            .collect();
                        images.sort();
                        for image in images {
                            index.insert(app, image);
                        }
                    }
                } else if let Some(app) = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.split('_').next())
                    .and_then(|name| name.parse().ok())
                {
                    index.insert(app, path);
                }
            }
        }
        Ok(index)
    }
    /// Offers one image for `app`. Files whose name is not an icon, header or
    /// 600x900 library image are ignored.
    ///
    /// Row icons prefer icon, then header, then library image. Covers use the
    /// reverse order. Among equal priorities the first one offered is kept.
    fn insert(&mut self, app: u32, path: PathBuf) {
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            return;
        };
        let priority = if name.ends_with("_icon") || name == "icon" {
            0
        } else if name.ends_with("_header") || name == "header" {
            1
        } else if name.ends_with("_library_600x900") || name == "library_600x900" {
            2
        } else {
            return;
        };
        if let Some(source) = Source::inspect(path) {
            if self.steam.get(&app).is_none_or(|(old, _)| *old > priority) {
                self.steam.insert(app, (priority, source.clone()));
            }
            let cover_priority = 2 - priority;
            if self
                .covers
                .get(&app)
                .is_none_or(|(old, _)| *old > cover_priority)
            {
                let mut cover = source;
                cover.edge = 384;
                self.covers.insert(app, (cover_priority, cover));
            }
        }
    }
    /// The 384-pixel image for the detail pane: the override if one exists,
    /// otherwise the best Steam cover for any of the game's ids.
    pub fn cover(&self, game: &crate::model::Game) -> Option<Source> {
        self.overrides
            .get(&game.id.to_string())
            .cloned()
            .map(|mut source| {
                source.edge = 384;
                source
            })
            .or_else(|| {
                game.ids().find_map(|id| {
                    id.steam_appid()
                        .and_then(|app| self.covers.get(&app).map(|(_, source)| source.clone()))
                })
            })
    }
    /// The 128-pixel image for the game's row, chosen the same way as `cover`.
    pub fn source(&self, game: &crate::model::Game) -> Option<Source> {
        self.overrides
            .get(&game.id.to_string())
            .cloned()
            .or_else(|| {
                game.ids().find_map(|id| {
                    id.steam_appid()
                        .and_then(|app| self.steam.get(&app).map(|(_, source)| source.clone()))
                })
            })
    }
}
/// Decoded thumbnails plus the queue of sources still to decode.
///
/// The cache does no I/O. The caller takes work from `next`, decodes it in
/// the background and reports back through `loaded`.
#[derive(Default)]
pub struct Cache {
    /// Oldest first, capped at 256. `None` records a failed decode so it is
    /// not retried.
    entries: VecDeque<(Source, Option<Handle>)>,
    /// Handed out by `next` and not yet reported through `loaded`.
    pending: HashSet<Source>,
    waiting: VecDeque<Source>,
}
impl Cache {
    /// The image for `source`. When this exact stamp has none, falls back to
    /// the newest image decoded from the same path at the same size, so a
    /// changed file keeps its old picture until the new one loads.
    pub fn get(&self, source: &Source) -> Option<&Handle> {
        self.entries
            .iter()
            .rev()
            .find(|(key, _)| key == source)
            .and_then(|(_, image)| image.as_ref())
            .or_else(|| {
                self.entries
                    .iter()
                    .rev()
                    .find(|(key, image)| {
                        key.path == source.path && key.edge == source.edge && image.is_some()
                    })
                    .and_then(|(_, image)| image.as_ref())
            })
    }
    /// Queues `source` for decoding unless it is cached, in flight or queued.
    /// The newest request goes first, since it is the one on screen now.
    pub fn request(&mut self, source: Source) {
        if self.entries.iter().any(|(key, _)| *key == source) || self.pending.contains(&source) {
            return;
        }
        self.waiting.retain(|queued| *queued != source);
        self.waiting.push_front(source);
    }
    /// Takes the next source to decode, or `None` while two are in flight.
    /// The caller must report each one back through `loaded`.
    pub fn next(&mut self) -> Option<Source> {
        if self.pending.len() >= 2 {
            return None;
        }
        let source = self.waiting.pop_front()?;
        self.pending.insert(source.clone());
        Some(source)
    }
    /// Stores a decode result, `None` for a failure, and evicts the oldest
    /// entries beyond 256.
    pub fn loaded(&mut self, source: Source, image: Option<Handle>) {
        self.pending.remove(&source);
        self.entries.retain(|(key, _)| *key != source);
        self.entries.push_back((source, image));
        while self.entries.len() > 256 {
            self.entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check, check_eq};
    #[test]
    fn deterministic_roles_bounded_decode_and_changed_sources() -> TestResult {
        let root = tempfile::tempdir().ctx("artwork fixture")?;
        let icon = root.path().join("42_icon.png");
        let header = root.path().join("42_header.png");
        let unknown = root.path().join("42_unrelated.png");
        let cover = root.path().join("42_library_600x900.png");
        for path in [&header, &icon, &unknown, &cover] {
            image::RgbaImage::new(300, 200)
                .save(path)
                .ctx("write artwork")?;
        }
        let mut index = Index {
            steam: HashMap::new(),
            overrides: HashMap::new(),
            covers: HashMap::new(),
        };
        index.insert(42, header);
        index.insert(42, icon.clone());
        index.insert(42, unknown);
        index.insert(42, cover.clone());
        let (_, cover_source) = index.covers.get(&42).ctx("selected cover")?;
        check_eq(
            &cover_source.path,
            &cover,
            "details prefer covers independently of row icons",
        )?;
        check_eq(
            cover_source.edge,
            384,
            "cover decoding uses its own bounded size",
        )?;
        let (_, source) = index.steam.get(&42).ctx("selected icon")?;
        check_eq(
            &source.path,
            &icon,
            "icon wins independently of insertion order",
        )?;
        let source = source.clone();
        let handle = source.decode().ctx("bounded thumbnail")?;
        let mut cache = Cache::default();
        cache.request(source.clone());
        let pending = cache.next().ctx("first worker")?;
        cache.request(source.clone());
        check(cache.next().is_none(), "in-flight source is deduplicated")?;
        cache.loaded(pending, Some(handle));
        check(
            cache.get(&source).is_some(),
            "decoded image is immediately reusable",
        )?;
        std::fs::write(&icon, b"corrupt artwork").ctx("change artwork")?;
        let changed = Source::inspect(icon).ctx("changed source stamp")?;
        check(changed != source, "file change invalidates cache identity")?;
        check(changed.decode().is_err(), "corrupt image cannot decode")?;
        check(
            cache.get(&changed).is_some(),
            "previous image remains during reload",
        )?;
        for number in 0..300 {
            let mut key = source.clone();
            key.bytes = number;
            cache.loaded(key, None);
        }
        check_eq(cache.entries.len(), 256, "cache remains bounded")
    }

    #[test]
    fn the_newest_request_is_decoded_first() -> TestResult {
        let source = |bytes: u64| Source {
            path: "/fixture/art.png".into(),
            modified: None,
            bytes,
            edge: 52,
        };
        let mut cache = Cache::default();
        for number in 1..=4 {
            cache.request(source(number));
        }
        // A request seen again moves to the front, so a row scrolled back
        // into view is not queued behind everything that came after it.
        cache.request(source(1));
        let first = cache.next().ctx("first decode")?;
        let second = cache.next().ctx("second decode")?;
        check_eq(first.bytes, 1, "the row just shown goes first")?;
        check_eq(second.bytes, 4, "then the next newest")?;
        check(cache.next().is_none(), "two at a time")
    }
}
