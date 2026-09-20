use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Context;
use cargo_util::paths::link_or_copy;
use cargo_util::{Sha256, paths};
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

    pub fn publish_entry(&self, pkg_dir: &str, out_dir: &Path) -> CargoResult<()> {
        let content = self.layout.content_dir();

        let mut files = BTreeMap::new();
        for entry in walkdir::WalkDir::new(out_dir) {
            let entry = entry?;
            let src = entry.path();
            if !src.is_file() {
                continue;
            }
            let rel = src
                .strip_prefix(out_dir)
                .expect("walked path under out dir");
            if rel.as_os_str().is_empty() {
                continue;
            }
            let hash = Self::hash(src)?;
            let dest = content.join(&hash);

            link_or_copy(src, &dest)?;

            files.insert(rel.to_path_buf(), hash);
        }

        let entry = CacheEntry { files };

        let json = serde_json::to_string(&entry)?;
        let entry_path = self.layout.entries_dir().join(pkg_dir);
        cargo_util::paths::create_dir_all(entry_path.parent().unwrap())?;

        cargo_util::paths::write_atomic(&entry_path, &json).context("writing cache entry")?;

        Ok(())
    }

    pub fn restore_from_cache(&self, entry: CacheEntry, out_dir: &Path) -> CargoResult<()> {
        paths::create_dir_all(out_dir)?;

        let content_dir = self.layout.content_dir();
        for (path_in_out_dir, hash) in entry.files {
            if path_in_out_dir.as_os_str().is_empty() {
                continue;
            }
            let dest = out_dir.join(&path_in_out_dir);
            if let Some(parent) = dest.parent() {
                paths::create_dir_all(parent)?;
            }
            let file_in_cache = content_dir.join(hash);
            paths::link_or_copy(&file_in_cache, &dest)?;
        }

        Ok(())
    }

    fn restore_files(&self, files: BTreeMap<PathBuf, String>, dest_root: &Path) -> CargoResult<()> {
        paths::create_dir_all(dest_root)?;
        let content_dir = self.layout.content_dir();
        for (rel, hash) in files {
            if rel.as_os_str().is_empty() || rel.is_absolute() {
                continue;
            }
            if rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                continue;
            }
            let dest = dest_root.join(rel);
            if let Some(parent) = dest.parent() {
                paths::create_dir_all(parent)?;
            }
            paths::link_or_copy(&content_dir.join(hash), &dest)?;
        }
        Ok(())
    }

    pub fn publish_build_script(
        &self,
        pkg_dir: &str,
        run_root: &Path,
        output: &crate::compiler::custom_build::BuildOutput,
    ) -> CargoResult<()> {
        let entry = ScriptEntry {
            files: self.dedup_dir(run_root)?,
            output: output.clone(),
        };
        let json = serde_json::to_string(&entry)?;
        let entry_path = self.layout.entries_dir().join(pkg_dir).join("script.json");
        cargo_util::paths::create_dir_all(entry_path.parent().unwrap())?;
        cargo_util::paths::write_atomic(&entry_path, &json)
            .context("writing script cache entry")?;
        Ok(())
    }

    pub fn restore_build_script(
        &self,
        pkg_dir: &str,
        run_root: &Path,
    ) -> Option<crate::compiler::custom_build::BuildOutput> {
        let path = self.layout.entries_dir().join(pkg_dir).join("script.json");
        let content = std::fs::read_to_string(&path).ok()?;
        let entry: ScriptEntry = serde_json::from_str(&content).ok()?;
        self.restore_files(entry.files, run_root).ok()?;
        Some(entry.output)
    }

    fn dedup_dir(&self, dir: &Path) -> CargoResult<BTreeMap<PathBuf, String>> {
        let content = self.layout.content_dir();
        let mut files = BTreeMap::new();
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry?;
            let src = entry.path();
            if !src.is_file() {
                continue;
            }
            let rel = src.strip_prefix(dir).expect("walked path under cached dir");
            if rel.as_os_str().is_empty() {
                continue;
            }
            let hash = Self::hash(src)?;
            link_or_copy(src, &content.join(&hash))?;
            files.insert(rel.to_path_buf(), hash);
        }
        Ok(files)
    }

    fn hash(path: &Path) -> CargoResult<String> {
        let mut hasher = Sha256::new();
        hasher.update_path(path)?;
        Ok(hasher.finish_hex())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ScriptEntry {
    files: BTreeMap<PathBuf, String>,
    output: crate::compiler::custom_build::BuildOutput,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    // A map of paths relative to the OUT_DIR and their hashes.
    pub files: BTreeMap<PathBuf, String>,
}
