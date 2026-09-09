use std::path::{Path, PathBuf};

use anyhow::Context;
use cargo_util::Sha256;
use cargo_util::paths::link_or_copy;
use serde::{Deserialize, Serialize};

use crate::{CargoResult, compiler::layout::BuildCacheLayout};

pub struct BuildCache {
    layout: BuildCacheLayout,
}

impl BuildCache {
    pub fn new(layout: BuildCacheLayout) -> Self {
        Self { layout }
    }

    pub fn get(&self, pkg_dir: &str) -> Option<CacheEntry> {
        let path = self.layout.entries_dir().join(pkg_dir);
        let content = std::fs::read_to_string(&path).ok()?;
        let entry = serde_json::from_str(&content).ok()?;
        Some(entry)
    }

    pub fn publish_entry(&self, pkg_dir: &str, rmeta: &Path, rlib: &Path) -> CargoResult<()> {
        let rmeta_hash = Self::hash(rmeta)?;
        let rlib_hash = Self::hash(rlib)?;

        let content = self.layout.content_dir();
        // TODO: move this out so its only ran once at startup to save IO/syscalls
        cargo_util::paths::create_dir_all(&content)?;
        let rmeta_path = content.join(rmeta_hash);
        let rlib_path = content.join(rlib_hash);

        link_or_copy(rmeta, &rmeta_path)?;
        link_or_copy(rlib, &rlib_path)?;

        let entry = CacheEntry {
            rmeta: rmeta_path,
            rlib: rlib_path,
        };

        let json = serde_json::to_string(&entry)?;
        let entry_path = self.layout.entries_dir().join(pkg_dir);
        cargo_util::paths::create_dir_all(entry_path.parent().unwrap())?;

        cargo_util::paths::write_atomic(&entry_path, &json).context("writing cache entry")?;

        Ok(())
    }

    fn hash(path: &Path) -> CargoResult<String> {
        let mut hasher = Sha256::new();
        hasher.update_path(path)?;
        Ok(hasher.finish_hex())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub rlib: PathBuf,
    pub rmeta: PathBuf,
}
