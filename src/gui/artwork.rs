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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub path: PathBuf,
    modified: Option<SystemTime>,
    bytes: u64,
    edge: u32,
}
impl Source {
    pub fn inspect(path: PathBuf) -> Option<Self> {
        let metadata = std::fs::metadata(&path).ok()?;
        metadata.is_file().then(|| Self {
            path,
            modified: metadata.modified().ok(),
            bytes: metadata.len(),
            edge: 128,
        })
    }
    pub fn decode(&self) -> Result<Handle> {
        ensure!(self.bytes <= 32 * 1024 * 1024, "Artwork exceeds 32 MiB");
        let mut reader = image::ImageReader::open(&self.path)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let pixels = reader.decode()?.thumbnail(self.edge, self.edge).to_rgba8();
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
#[derive(Debug, Serialize, Deserialize)]
struct Override {
    game: String,
    #[serde(with = "crate::path_serde")]
    path: PathBuf,
}
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
pub fn save_override(game: String, path: PathBuf) -> Result<()> {
    Source::inspect(path.clone())
        .context("Artwork file unavailable")?
        .decode()?;
    let mut items = overrides()?;
    items.retain(|item| item.game != game);
    items.push(Override { game, path });
    let root = crate::libraries::data_dir()?;
    crate::libraries::private_dir(&root)?;
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    serde_json::to_writer(&mut file, &items)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(root.join("artwork.json"))?;
    Ok(())
}
pub struct Index {
    steam: HashMap<u32, (u8, Source)>,
    overrides: HashMap<String, Source>,
    covers: HashMap<u32, (u8, Source)>,
}
impl Index {
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
#[derive(Default)]
pub struct Cache {
    entries: VecDeque<(Source, Option<Handle>)>,
    pending: HashSet<Source>,
    waiting: VecDeque<Source>,
}
impl Cache {
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
    pub fn request(&mut self, source: Source) {
        if self.entries.iter().any(|(key, _)| *key == source)
            || self.pending.contains(&source)
            || self.waiting.contains(&source)
        {
            return;
        }
        self.waiting.push_back(source);
    }
    pub fn next(&mut self) -> Option<Source> {
        if self.pending.len() >= 2 {
            return None;
        }
        let source = self.waiting.pop_front()?;
        self.pending.insert(source.clone());
        Some(source)
    }
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
}
