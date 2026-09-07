//! Content-addressed build cache (`$CARGO_HOME/build-cache`).
//!
//! [`BuildCache`] owns all cache logic: hashing artifacts into
//! `content/`, tracking them with manifests in `entries/`, publishing fresh
//! builds, and garbage collection. [`BuildCacheLayout`](super::layout::BuildCacheLayout)
//! only describes the on-disk paths; this struct operates on them.

use super::layout::BuildCacheLayout;
use crate::util::CargoResult;
use crate::util::data_structures::HashSet;
use anyhow::Context as _;
use cargo_util::paths;
use std::path::{Path, PathBuf};

/// Current manifest schema version. Entries written with an older version use
/// a content naming rustc cannot resolve (bare `<hash>[.<ext>]`,
/// `lib<hash>[.<ext>]`, or `lib<stem>-<content-hash>` whose rmeta/rlib stems
/// differ per file), so they must be treated as absent and rebuilt.
pub const CACHE_MANIFEST_VERSION: u32 = 4;

/// Content-addressed store for cacheable build artifacts.
#[derive(Clone, Debug)]
pub struct BuildCache {
    layout: BuildCacheLayout,
}

impl BuildCache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            layout: BuildCacheLayout::new(root),
        }
    }

    /// Cache build unit dir (`build-cache/<pkg>/<hash>`).
    pub fn build_unit(&self, pkg_dir: &str) -> PathBuf {
        self.layout.build_unit(pkg_dir)
    }

    /// Hash file at `path` with sha256 hex.
    pub fn hash_file(path: &Path) -> CargoResult<String> {
        let mut hasher = cargo_util::Sha256::new();
        hasher
            .update_path(path)
            .with_context(|| format!("failed to hash `{}`", path.display()))?;
        Ok(hasher.finish_hex())
    }

    /// Insert `src` into the content store under the caller-chosen `name`.
    /// The name is load-bearing (see `publish_unit_to_cas`): rustc only
    /// accepts `--extern` paths shaped `lib*.<rlib|rmeta|so|...>` (else
    /// `E0463: can't find crate`), and it resolves *transitive* deps by `-L`
    /// directory search, which requires an rmeta/rlib pair of one build to
    /// share the exact filename stem. Nothing is ever copied or linked out
    /// of the cache; rustc reads the blobs in place via
    /// `-L dependency=<content-dir>`.
    /// Uses hardlink when possible, copies on cross-device, dedupes on `AlreadyExists`.
    pub fn insert_into_content(&self, src: &Path, name: &str) -> CargoResult<PathBuf> {
        let dst = self.layout.content_path(name);
        if dst.exists() {
            return Ok(dst);
        }
        // Try hardlink first.
        match std::fs::hard_link(src, &dst) {
            Ok(()) => return Ok(dst),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(dst),
            Err(e) if e.raw_os_error() == Some(17) => return Ok(dst),
            Err(e) if e.raw_os_error() == Some(18) => {
                // EXDEV: cross-device, copy via temp + rename.
            }
            Err(_) => {
                // Fall through to copy; hardlink may fail for other reasons.
            }
        }
        // Copy via temp file and atomic rename to avoid partial writes.
        let tmp = self
            .layout
            .content_dir()
            .join(format!(".tmp-{name}-{}", std::process::id()));
        std::fs::copy(src, &tmp)
            .with_context(|| format!("failed to copy `{}` to content tmp", src.display()))?;
        match std::fs::rename(&tmp, &dst) {
            Ok(()) => Ok(dst),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&tmp);
                Ok(dst)
            }
            Err(e) if e.raw_os_error() == Some(17) => {
                let _ = std::fs::remove_file(&tmp);
                Ok(dst)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e.into())
            }
        }
    }

    /// Insert `src` as a mid-build pipelining blob; returns its cache path.
    pub fn insert_rmeta(&self, src: &Path) -> CargoResult<PathBuf> {
        let name = format!("lib{}.rmeta", Self::hash_file(src)?);
        self.insert_into_content(src, &name)
    }

    /// Atomically write a manifest at `entries/<pkg>/<hash>`.
    pub fn write_manifest_atomic(
        &self,
        pkg_dir: &str,
        manifest: &CacheEntryManifest,
    ) -> CargoResult<()> {
        let dst = self.layout.entry_manifest_path(pkg_dir);
        if let Some(parent) = dst.parent() {
            paths::create_dir_all(parent)?;
        }
        let json =
            serde_json::to_string_pretty(manifest).context("failed to serialize manifest")?;
        // Write to temp in same dir then rename for atomicity.
        let tmp = dst.with_extension(format!("tmp-{}", std::process::id()));
        paths::write(&tmp, json.as_bytes())?;
        match std::fs::rename(&tmp, &dst) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another writer won; if fingerprints match, success, else keep existing.
                let _ = std::fs::remove_file(&tmp);
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Read manifest at `entries/<pkg>/<hash>`, if present.
    /// Manifests from an older schema version are treated as absent so stale
    /// entries are rebuilt and republished instead of serving unloadable paths.
    pub fn read_manifest(&self, pkg_dir: &str) -> CargoResult<Option<CacheEntryManifest>> {
        let path = self.layout.entry_manifest_path(pkg_dir);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = paths::read_bytes(&path)?;
        let m: CacheEntryManifest =
            serde_json::from_slice(&bytes).context("failed to parse cache manifest")?;
        if m.version != CACHE_MANIFEST_VERSION {
            return Ok(None);
        }
        Ok(Some(m))
    }

    /// Whether a manifest's content files all exist.
    pub fn manifest_content_exists(&self, manifest: &CacheEntryManifest) -> bool {
        manifest
            .files
            .values()
            .all(|h| self.layout.content_path(h).exists())
    }

    /// Touch manifest mtime to mark use (best-effort). Throttled to once per day to avoid SSD churn.
    pub fn touch_manifest(&self, pkg_dir: &str) {
        let path = self.layout.entry_manifest_path(pkg_dir);
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(elapsed) = std::time::SystemTime::now().duration_since(mtime) {
                    if elapsed.as_secs() < 24 * 60 * 60 {
                        return;
                    }
                }
            }
        }
        let now = filetime::FileTime::now();
        let _ = filetime::set_file_mtime(&path, now);
    }

    /// Publish a built unit's linkable artifacts into CAS and write its manifest.
    ///
    /// Takes the unit's exact output paths: the rmeta plus its linkable
    /// sibling (`Some` rlib, or the dylib for dylib-only units; `None` for
    /// rmeta-only check builds). Nothing is scanned: dep-info, fingerprints,
    /// binaries and unknown files are never published.
    /// Both artifacts share one stored stem derived from the fingerprint hash
    /// (`lib<stem>-<fingerprint>.<ext>`): rustc resolves transitive deps by
    /// `-L` search and requires an rmeta/rlib pair of one build to share the
    /// exact stem, while content-hash names would give each file a different
    /// stem and break the lookup with `E0463`. The fingerprint (which includes
    /// the compile mode) is unique per distinct build, so names never collide
    /// across configurations; identical builds share the name and dedupe.
    /// The workspace originals are left in place: dependents bake workspace
    /// `--extern` paths at plan time, before this publish runs. The unit's
    /// build-dir directory is removed after the build by
    /// [`BuildCache::sweep_unit_dir`].
    pub fn publish_unit_to_cas(
        &self,
        pkg_dir: &str,
        fingerprint_hash: &str,
        rmeta_src: &Path,
        linkable_src: Option<&Path>,
    ) -> CargoResult<()> {
        let mut files = std::collections::BTreeMap::new();
        let mut publish = |src: &Path| -> CargoResult<()> {
            let file_name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("");
            // `strip_prefix` is a no-op for already-prefixed stems; it only
            // avoids a `liblib...` shape that `-L` discovery would not match.
            let stem = stem.strip_prefix("lib").unwrap_or(stem);
            let stored = format!("lib{stem}-{fingerprint_hash}.{ext}");
            self.insert_into_content(src, &stored)?;
            files.insert(format!("out/{file_name}"), stored);
            Ok(())
        };
        publish(rmeta_src)?;
        if let Some(linkable) = linkable_src {
            publish(linkable)?;
        }
        let manifest = CacheEntryManifest {
            version: CACHE_MANIFEST_VERSION,
            fingerprint_hash: fingerprint_hash.to_string(),
            created: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .to_string(),
            ),
            files,
        };
        // If manifest already exists with same fingerprint_hash, skip.
        if let Ok(Some(existing)) = self.read_manifest(pkg_dir) {
            if existing.fingerprint_hash == manifest.fingerprint_hash
                && self.manifest_content_exists(&existing)
            {
                self.touch_manifest(pkg_dir);
                return Ok(());
            }
        }
        self.write_manifest_atomic(pkg_dir, &manifest)
    }
    /// Remove a unit's workspace build-dir directory once the cache owns it.
    ///
    /// Call only after the build's jobs all finished: dependents bake
    /// workspace `--extern` paths at plan time, so the directory must survive
    /// until no rustc can still reference it. Planner freshness for cacheable
    /// units is manifest-driven (see `prepare_target`), so no workspace state
    /// is needed on later builds. Removal happens only when the manifest
    /// exists and every content blob is present; anything else is kept as the
    /// workspace fallback. Best-effort.
    pub fn sweep_unit_dir(&self, pkg_dir: &str, unit_dir: &Path) {
        let complete = match self.read_manifest(pkg_dir) {
            Ok(Some(manifest)) => self.manifest_content_exists(&manifest),
            _ => false,
        };
        if complete {
            let _ = std::fs::remove_dir_all(unit_dir);
        }
    }

    pub fn gc(&self, max_age: std::time::Duration) -> CargoResult<(usize, usize)> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut removed_manifests = 0usize;
        let entries_root = self.layout.entries_dir();
        if entries_root.exists() {
            for pkg_entry in std::fs::read_dir(&entries_root)?.flatten() {
                let pkg_path = pkg_entry.path();
                if !pkg_path.is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(&pkg_path)?.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if let Ok(meta) = std::fs::metadata(&path) {
                        if let Ok(mtime) = meta.modified() {
                            if mtime < cutoff {
                                let _ = std::fs::remove_file(&path);
                                removed_manifests += 1;
                            }
                        }
                    }
                }
            }
        }
        // Collect referenced hashes.
        let mut referenced = HashSet::default();
        if entries_root.exists() {
            for pkg_entry in std::fs::read_dir(&entries_root)?.flatten() {
                let pkg_path = pkg_entry.path();
                if !pkg_path.is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(&pkg_path)?.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if let Ok(bytes) = std::fs::read(&path) {
                        if let Ok(m) = serde_json::from_slice::<CacheEntryManifest>(&bytes) {
                            referenced.extend(m.files.values().cloned());
                        }
                    }
                }
            }
        }
        let mut removed_content = 0usize;
        let content_root = self.layout.content_dir();
        if content_root.exists() {
            for entry in std::fs::read_dir(&content_root)?.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with(".tmp-") {
                        continue;
                    }
                    if !referenced.contains(name) {
                        let _ = std::fs::remove_file(&p);
                        removed_content += 1;
                    }
                }
            }
        }
        Ok((removed_manifests, removed_content))
    }

    /// Ensure the cache root and CAS directories exist.
    pub fn prepare(&mut self) -> CargoResult<()> {
        self.layout.prepare()
    }

    /// Content file path for a stored name (`lib<stem>-<hash>[.<ext>]`).
    pub fn content_path(&self, hash: &str) -> PathBuf {
        self.layout.content_path(hash)
    }

    /// Directory holding content-addressed blobs:
    /// `<build-cache>/content/lib<stem>-<fingerprint>[.<ext>]`.
    pub fn content_dir(&self) -> PathBuf {
        self.layout.content_dir()
    }

    /// Manifest file for a unit: `entries/<pkg>/<hash>`.
    pub fn entry_manifest_path(&self, pkg_dir: &str) -> PathBuf {
        self.layout.entry_manifest_path(pkg_dir)
    }
}

/// Manifest stored at `entries/<pkg>/<hash>` describing content-addressed files.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntryManifest {
    pub version: u32,
    pub fingerprint_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    /// Map from relative path (`out/foo.rlib`) to stored content file name
    /// (`libfoo-<unitmeta>-<fingerprint>.rlib`). Only output artifacts are
    /// stored; fingerprint files and dep-info live in the workspace build-dir
    /// and are not deduplicated into `content`.
    pub files: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache() -> (tempfile::TempDir, BuildCache, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cache = BuildCache::new(dir.path().join("cache"));
        cache.prepare().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        (dir, cache, src)
    }

    fn write_src(src: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = src.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn publish_keeps_build_dir_originals() {
        let (_dir, cache, src) = test_cache();
        let pkg = "foo/abc123";
        let rmeta = write_src(&src, "libfoo.rmeta", b"rmeta-v1");
        let rlib = write_src(&src, "libfoo.rlib", b"rlib-v1");
        cache
            .publish_unit_to_cas(pkg, "fp1", &rmeta, Some(&rlib))
            .unwrap();
        // Dependents bake workspace paths at plan time, so publish must not
        // touch the originals; the sweep below removes them post-build.
        assert!(rmeta.is_file());
        assert!(rlib.is_file());
    }

    #[test]
    fn sweep_removes_whole_unit_dir() {
        let (_dir, cache, src) = test_cache();
        let pkg = "foo/abc123";
        // Unit dir layout mirrors the workspace build-dir: artifacts plus the
        // files publish never uploads (fingerprint, dep-info, lock). None of
        // it is needed: planner freshness is manifest-driven.
        let unit_dir = src.join("build").join("foo").join("abc123");
        let out = unit_dir.join("out");
        let fp = unit_dir.join("fingerprint");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&fp).unwrap();
        let rmeta = write_src(&out, "libfoo.rmeta", b"rmeta-v1");
        let rlib = write_src(&out, "libfoo.rlib", b"rlib-v1");
        std::fs::write(fp.join("libfoo"), b"fingerprint").unwrap();
        std::fs::write(fp.join("dep-libfoo"), b"dep-info").unwrap();
        std::fs::write(unit_dir.join(".lock"), b"lock").unwrap();
        cache
            .publish_unit_to_cas(pkg, "fp1", &rmeta, Some(&rlib))
            .unwrap();
        cache.sweep_unit_dir(pkg, &unit_dir);
        assert!(!unit_dir.exists(), "swept unit dir must be gone entirely");
        assert_eq!(
            std::fs::read(cache.content_path("libfoo-fp1.rmeta")).unwrap(),
            b"rmeta-v1"
        );
        let manifest = cache.read_manifest(pkg).unwrap().unwrap();
        assert!(cache.manifest_content_exists(&manifest));
    }

    #[test]
    fn sweep_keeps_dir_without_complete_entry() {
        let (_dir, cache, src) = test_cache();
        let unit_dir = src.join("build").join("foo").join("abc123");
        std::fs::create_dir_all(&unit_dir).unwrap();
        let rmeta = write_src(&unit_dir, "libfoo.rmeta", b"rmeta");
        // No manifest: keep as the workspace fallback.
        cache.sweep_unit_dir("foo/abc123", &unit_dir);
        assert!(rmeta.is_file());
        // Manifest with missing content: keep too.
        cache
            .publish_unit_to_cas("foo/abc123", "fp1", &rmeta, None)
            .unwrap();
        std::fs::remove_file(cache.content_path("libfoo-fp1.rmeta")).unwrap();
        cache.sweep_unit_dir("foo/abc123", &unit_dir);
        assert!(unit_dir.is_dir());
    }
}
