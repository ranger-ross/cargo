//! Portable immutable graph inventories with local usage receipts.
//!
//! The caller holds the package-cache lock during publication and collection.
//! Collection rotates the cache revision before invalidating any stored data.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

use super::format::{
    Digest, Receipt, UnitResult, decode_snapshot, decode_unit, digest_filename, encode_snapshot,
    snapshot_id,
};
use crate::CargoResult;
use crate::ops::CleanContext;
use crate::util::data_structures::{HashMap, HashSet};

const USAGE_UPDATE_INTERVAL: u64 = 4 * 60 * 60;
const RETENTION: u64 = 30 * 24 * 60 * 60;
const UNITS: &str = "units-v1";
const SNAPSHOTS: &str = "snapshots-v1";
const LOCAL: &str = "local-v2";
const REVISION_MAGIC: &[u8] = b"cargo-shared-blob-revision-v1\0";

pub(super) struct SnapshotStore<'a> {
    root: &'a Path,
}

impl<'a> SnapshotStore<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub fn revision(&self) -> CargoResult<Digest> {
        self.check_directories()?;
        match read_revision(&self.root.join(LOCAL).join("revision"))? {
            Some(revision) => Ok(revision),
            None => self.replace_revision(),
        }
    }

    pub fn has_receipt(&self, build_dir: &[u8]) -> CargoResult<bool> {
        self.check_directories()?;
        let path = self.receipt_path(build_dir);
        let Some(bytes) = read_regular(&path)? else {
            return Ok(false);
        };
        Receipt::decode(&bytes)
            .with_context(|| format!("invalid shared blob receipt {}", path.display()))?;
        Ok(true)
    }

    /// Publish before attaching the inventory to a fingerprint.
    pub fn publish_unit(&self, result: &UnitResult) -> CargoResult<()> {
        self.check_directories()?;
        publish_object(&self.root.join(UNITS).join(hex(&result.id)), &result.bytes)
    }

    /// Validate inventories fully, but only stat the immutable output blobs.
    pub fn unit_is_complete(&self, id: &Digest) -> CargoResult<bool> {
        self.check_directories()?;
        let path = self.root.join(UNITS).join(hex(id));
        if regular_size(&path)?.is_none() {
            return Ok(false);
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let Ok(outputs) = decode_unit(&bytes, id) else {
            return Ok(false);
        };
        for output in outputs {
            if regular_size(&self.root.join(hex(&output.hash)))? != Some(output.size) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Reload usage under the caller's exclusive lock after a successful build.
    pub fn save(&self, build_dir: &[u8], mut results: Vec<Digest>, now: u64) -> CargoResult<()> {
        self.check_directories()?;
        let path = self.receipt_path(build_dir);
        let mut receipt = read_receipt(&path)?;
        let id = snapshot_id(&mut results);
        let manifest = self.root.join(SNAPSHOTS).join(hex(&id));
        // The canonical graph digest also verifies framing without allocating
        // members. A hit does not serialize the graph or replace the manifest.
        let valid =
            read_regular(&manifest)?.is_some_and(|bytes| blake3::hash(&bytes).as_bytes() == &id);
        if !valid {
            atomic_replace(&manifest, &encode_snapshot(&results))?;
        }
        match receipt.usage.get_mut(&id) {
            Some(last_used) if now.saturating_sub(*last_used) >= USAGE_UPDATE_INTERVAL => {
                *last_used = now;
            }
            None => {
                receipt.usage.insert(id, now);
            }
            _ => return Ok(()),
        }
        atomic_replace(&path, &receipt.encode())
    }

    pub fn clean(
        &self,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
        now: u64,
    ) -> CargoResult<()> {
        let plan = self.plan_collection(max_size, now)?;
        // Rotate before receipt changes or deletion, including partial failure.
        if !clean_ctx.dry_run {
            if plan.pruned_usage || !plan.forgotten.is_empty() || !plan.removals.is_empty() {
                read_revision(&self.root.join(LOCAL).join("revision"))?;
                self.replace_revision()?;
            }
            for (path, bytes) in plan.receipts {
                atomic_replace(&path, &bytes)?;
            }
        }
        for path in plan.forgotten {
            clean_ctx.rm_rf(&path)?;
        }
        // Remove snapshot manifests before their unit manifests and blobs.
        for path in plan.removals {
            clean_ctx.rm_rf(&path)?;
        }
        Ok(())
    }

    fn receipt_path(&self, build_dir: &[u8]) -> PathBuf {
        self.root
            .join(LOCAL)
            .join(blake3::hash(build_dir).to_hex().as_str())
    }

    fn replace_revision(&self) -> CargoResult<Digest> {
        let revision = rand::random::<Digest>();
        let mut bytes = Vec::with_capacity(REVISION_MAGIC.len() + revision.len());
        bytes.extend_from_slice(REVISION_MAGIC);
        bytes.extend_from_slice(&revision);
        atomic_replace(&self.root.join(LOCAL).join("revision"), &bytes)?;
        Ok(revision)
    }

    fn check_directories(&self) -> CargoResult<()> {
        check_directory(self.root)?;
        for directory in [UNITS, SNAPSHOTS, LOCAL] {
            check_directory(&self.root.join(directory))?;
        }
        Ok(())
    }

    /// Decode and plan everything before performing any mutation. Invalid
    /// manifests are disposable; an unreadable receipt may hide a live root,
    /// and is therefore a hard error rather than an empty receipt.
    fn plan_collection(&self, max_size: Option<u64>, now: u64) -> CargoResult<Collection> {
        self.check_directories()?;
        let mut blobs = HashMap::default();
        let mut legacy = Vec::new();
        for entry in entries(self.root)? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "local-v1" {
                legacy.push(entry.path());
                continue;
            }
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(id) = digest_filename(name) {
                blobs.insert(id, (entry.path(), entry.metadata()?.len()));
            } else if name
                .strip_suffix(".timestamp")
                .and_then(digest_filename)
                .is_some()
                || matches!(
                    name,
                    "index.sqlite"
                        | "index.sqlite-journal"
                        | "index.sqlite-wal"
                        | "index.sqlite-shm"
                )
            {
                legacy.push(entry.path());
            }
        }

        let unit_files = object_files(&self.root.join(UNITS))?;
        let snapshot_files = object_files(&self.root.join(SNAPSHOTS))?;
        let mut units = HashMap::default();
        for (id, path) in &unit_files {
            let bytes = read_regular(path)?
                .with_context(|| format!("unit manifest disappeared: {}", path.display()))?;
            let Ok(outputs) = decode_unit(&bytes, id) else {
                continue;
            };
            if outputs.iter().all(|output| {
                blobs
                    .get(&output.hash)
                    .is_some_and(|(_, size)| *size == output.size)
            }) {
                let mut references: Vec<_> =
                    outputs.into_iter().map(|output| output.hash).collect();
                references.sort_unstable();
                references.dedup();
                units.insert(*id, references);
            }
        }
        let mut snapshots = HashMap::default();
        for (id, path) in &snapshot_files {
            let bytes = read_regular(path)?
                .with_context(|| format!("snapshot manifest disappeared: {}", path.display()))?;
            let Ok(results) = decode_snapshot(&bytes, id) else {
                continue;
            };
            if results.iter().all(|result| units.contains_key(result)) {
                snapshots.insert(*id, results);
            }
        }

        let imports_path = self.root.join(LOCAL).join("imports");
        let mut receipts = Vec::new();
        let mut known = HashSet::default();
        let mut recency = HashMap::<Digest, u64>::default();
        let cutoff = now.saturating_sub(RETENTION);
        for entry in entries(&self.root.join(LOCAL))? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name != "imports" && digest_filename(name).is_none() {
                continue;
            }
            let path = entry.path();
            let receipt = read_receipt(&path)?;
            for (id, last_used) in &receipt.usage {
                // Expired usage still prevents granting a second import lease.
                known.insert(*id);
                if *last_used >= cutoff && snapshots.contains_key(id) {
                    recency
                        .entry(*id)
                        .and_modify(|time| *time = (*time).max(*last_used))
                        .or_insert(*last_used);
                }
            }
            receipts.push((path, receipt));
        }
        let mut adopted = Vec::new();
        for id in snapshots.keys() {
            if !known.contains(id) {
                recency.insert(*id, now);
                adopted.push(*id);
            }
        }
        if !adopted.is_empty() {
            let index =
                if let Some(index) = receipts.iter().position(|(path, _)| *path == imports_path) {
                    index
                } else {
                    receipts.push((imports_path, Receipt::default()));
                    receipts.len() - 1
                };
            for id in &adopted {
                receipts[index].1.usage.insert(*id, now);
            }
        }

        // Count each blob once across all retaining units. Eviction traverses
        // each disappearing unit once, not the entire graph for every victim.
        let mut unit_refs = HashMap::<Digest, usize>::default();
        let mut blob_refs = HashMap::<Digest, usize>::default();
        let mut size = 0_u128;
        for id in recency.keys() {
            for unit in &snapshots[id] {
                let references = unit_refs.entry(*unit).or_default();
                *references += 1;
                if *references == 1 {
                    for blob in &units[unit] {
                        let references = blob_refs.entry(*blob).or_default();
                        *references += 1;
                        if *references == 1 {
                            size += u128::from(blobs[blob].1);
                        }
                    }
                }
            }
        }
        if let Some(limit) = max_size {
            let mut oldest: Vec<_> = recency.iter().map(|(id, time)| (*time, *id)).collect();
            oldest.sort_unstable();
            for (_, id) in oldest {
                if size <= u128::from(limit) {
                    break;
                }
                recency.remove(&id);
                for unit in &snapshots[&id] {
                    let references = unit_refs.get_mut(unit).unwrap();
                    *references -= 1;
                    if *references == 0 {
                        for blob in &units[unit] {
                            let references = blob_refs.get_mut(blob).unwrap();
                            *references -= 1;
                            if *references == 0 {
                                size -= u128::from(blobs[blob].1);
                            }
                        }
                    }
                }
            }
        }

        let mut plan = Collection::default();
        for (path, mut receipt) in receipts {
            let previous_usage = receipt.usage.len();
            receipt
                .usage
                .retain(|id, time| *time >= cutoff && recency.contains_key(id));
            plan.pruned_usage |= previous_usage != receipt.usage.len();
            if receipt.usage.is_empty() {
                if path.try_exists()? {
                    plan.forgotten.push(path);
                }
                continue;
            }
            // Newly adopted leases were inserted before counting usage.
            let imports_changed = path.file_name().is_some_and(|name| name == "imports")
                && adopted.iter().any(|id| receipt.usage.contains_key(id));
            if previous_usage != receipt.usage.len() || imports_changed {
                plan.receipts.push((path, receipt.encode()));
            }
        }
        for (id, path) in snapshot_files {
            if !recency.contains_key(&id) {
                plan.removals.push(path);
            }
        }
        for (id, path) in unit_files {
            if !unit_refs.get(&id).is_some_and(|count| *count > 0) {
                plan.removals.push(path);
            }
        }
        for (id, (path, _)) in blobs {
            if !blob_refs.get(&id).is_some_and(|count| *count > 0) {
                plan.removals.push(path);
            }
        }
        plan.removals.extend(legacy);
        Ok(plan)
    }
}

#[derive(Default)]
struct Collection {
    pruned_usage: bool,
    receipts: Vec<(PathBuf, Vec<u8>)>,
    forgotten: Vec<PathBuf>,
    removals: Vec<PathBuf>,
}

fn hex(id: &Digest) -> String {
    blake3::Hash::from_bytes(*id).to_hex().to_string()
}

fn entries(path: &Path) -> CargoResult<Vec<fs::DirEntry>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(entries.collect::<Result<_, _>>()?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to inventory {}", path.display())),
    }
}

fn object_files(path: &Path) -> CargoResult<HashMap<Digest, PathBuf>> {
    let mut files = HashMap::default();
    for entry in entries(path)? {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(digest_filename) else {
            continue;
        };
        ensure!(
            entry.file_type()?.is_file(),
            "shared blob object is not a regular file: {}",
            entry.path().display()
        );
        files.insert(id, entry.path());
    }
    Ok(files)
}

fn check_directory(path: &Path) -> CargoResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "shared blob metadata directory is not a directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn regular_size(path: &Path) -> CargoResult<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file().then_some(metadata.len())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn read_revision(path: &Path) -> CargoResult<Option<Digest>> {
    let Some(bytes) = read_regular(path)? else {
        return Ok(None);
    };
    let revision = bytes
        .strip_prefix(REVISION_MAGIC)
        .and_then(|bytes| bytes.try_into().ok());
    ensure!(
        revision.is_some(),
        "invalid shared blob revision {}",
        path.display()
    );
    Ok(revision)
}

fn read_regular(path: &Path) -> CargoResult<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "shared blob metadata is not a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    Ok(Some(fs::read(path).with_context(|| {
        format!("failed to read {}", path.display())
    })?))
}

fn read_receipt(path: &Path) -> CargoResult<Receipt> {
    match read_regular(path)? {
        Some(bytes) => Receipt::decode(&bytes)
            .with_context(|| format!("invalid shared blob receipt {}", path.display())),
        None => Ok(Receipt::default()),
    }
}

fn publish_object(path: &Path, bytes: &[u8]) -> CargoResult<()> {
    if read_regular(path)?.is_some_and(|existing| existing == bytes) {
        return Ok(());
    }
    // Replace damaged content rather than letting an existing filename stand
    // in for a successfully published object.
    atomic_replace(path, bytes)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> CargoResult<()> {
    let parent = path.parent().expect("metadata file has a parent");
    check_directory(parent)?;
    // The store root is checked by the public entrypoint before this helper.
    fs::create_dir_all(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "refusing to replace non-regular shared blob metadata: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged
        .persist(path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::format::{Output, encode_output_path};
    use super::*;

    fn result(root: &Path, contents: &[u8]) -> UnitResult {
        let hash = *blake3::hash(contents).as_bytes();
        fs::write(root.join(hex(&hash)), contents).unwrap();
        UnitResult::new(vec![Output {
            path: encode_output_path(Path::new("artifact")).unwrap(),
            hash,
            size: contents.len() as u64,
        }])
    }

    fn save(store: &SnapshotStore<'_>, build_dir: &[u8], result: UnitResult, now: u64) -> Digest {
        let id = result.id;
        store.publish_unit(&result).unwrap();
        store.save(build_dir, vec![id], now).unwrap();
        id
    }

    fn clean(
        store: &SnapshotStore<'_>,
        max_size: Option<u64>,
        now: u64,
        dry_run: bool,
    ) -> CargoResult<()> {
        let gctx = crate::util::GlobalContext::new(
            cargo_util_terminal::Shell::from_write(Box::new(Vec::new())),
            store.root.to_path_buf(),
            store.root.join("cargo-home"),
        );
        let mut clean_ctx = CleanContext::new(&gctx);
        clean_ctx.dry_run = dry_run;
        store.clean(&mut clean_ctx, max_size, now)
    }

    #[test]
    fn revision_persists_and_changes_when_local_metadata_is_recreated() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let first = store.revision().unwrap();
        assert_eq!(SnapshotStore::new(root.path()).revision().unwrap(), first);
        assert!(!store.has_receipt(b"build").unwrap());
        fs::remove_dir_all(root.path().join(LOCAL)).unwrap();
        let second = store.revision().unwrap();
        assert_ne!(first, second);
        assert_eq!(store.revision().unwrap(), second);
        let other = tempfile::tempdir().unwrap();
        assert_ne!(SnapshotStore::new(other.path()).revision().unwrap(), second);
    }

    #[test]
    fn invalid_revision_prevents_deletion() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        store.revision().unwrap();
        let id = save(&store, b"build", result(root.path(), b"live"), 1);
        let path = root.path().join(LOCAL).join("revision");
        let valid = fs::read(&path).unwrap();
        let mut trailing = valid.clone();
        trailing.push(0);
        for bytes in [
            b"unknown".to_vec(),
            valid[..valid.len() - 1].to_vec(),
            trailing,
        ] {
            fs::write(&path, &bytes).unwrap();
            assert!(store.revision().is_err());
            assert!(clean(&store, Some(0), 1, false).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert!(store.has_receipt(b"build").unwrap());
            assert!(store.unit_is_complete(&id).unwrap());
        }
    }

    #[test]
    fn dry_run_and_noop_collection_preserve_revision() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        clean(&store, None, 1, false).unwrap();
        assert!(!root.path().join(LOCAL).exists());
        let revision = store.revision().unwrap();
        let id = save(&store, b"build", result(root.path(), b"live"), 1);
        clean(&store, None, 1, false).unwrap();
        assert_eq!(store.revision().unwrap(), revision);
        let receipt = fs::read(store.receipt_path(b"build")).unwrap();
        clean(&store, Some(0), RETENTION + 2, true).unwrap();
        assert_eq!(store.revision().unwrap(), revision);
        assert_eq!(fs::read(store.receipt_path(b"build")).unwrap(), receipt);
        assert!(store.unit_is_complete(&id).unwrap());
        clean(&store, Some(0), RETENTION + 2, false).unwrap();
        let after = store.revision().unwrap();
        assert_ne!(after, revision);
        assert!(!store.has_receipt(b"build").unwrap());
        assert!(!store.unit_is_complete(&id).unwrap());
        clean(&store, Some(0), RETENTION + 2, false).unwrap();
        assert_eq!(store.revision().unwrap(), after);
    }

    #[test]
    fn pruning_missing_usage_rotates_revision_without_object_deletion() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let revision = store.revision().unwrap();
        let missing = save(&store, b"build", result(root.path(), b"missing"), 1);
        let surviving = save(&store, b"build", result(root.path(), b"surviving"), 2);
        fs::remove_file(root.path().join(hex(blake3::hash(b"missing").as_bytes()))).unwrap();
        fs::remove_file(root.path().join(UNITS).join(hex(&missing))).unwrap();
        fs::remove_file(
            root.path()
                .join(SNAPSHOTS)
                .join(hex(&snapshot_id(&mut vec![missing]))),
        )
        .unwrap();
        let plan = store.plan_collection(None, 2).unwrap();
        assert!(plan.removals.is_empty());
        assert!(plan.forgotten.is_empty());
        let path = root.path().join(UNITS).join(hex(&surviving));
        let before = fs::metadata(&path).unwrap();
        clean(&store, None, 2, false).unwrap();
        assert_ne!(store.revision().unwrap(), revision);
        assert!(store.unit_is_complete(&surviving).unwrap());
        assert_eq!(
            read_receipt(&store.receipt_path(b"build")).unwrap().usage,
            [(snapshot_id(&mut vec![surviving]), 2)]
                .into_iter()
                .collect()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before.modified().unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), before.ino());
        }
    }

    #[test]
    fn legacy_local_metadata_is_removed_without_parsing_or_losing_imports() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let id = save(&store, b"build", result(root.path(), b"imported"), 1);
        fs::rename(root.path().join(LOCAL), root.path().join("local-v1")).unwrap();
        fs::write(root.path().join("local-v1").join("imports"), b"obsolete").unwrap();
        let revision = store.revision().unwrap();
        clean(&store, None, 2, false).unwrap();
        assert_ne!(store.revision().unwrap(), revision);
        assert!(!root.path().join("local-v1").exists());
        assert!(store.unit_is_complete(&id).unwrap());
        assert_eq!(
            read_receipt(&root.path().join(LOCAL).join("imports"))
                .unwrap()
                .usage,
            [(snapshot_id(&mut vec![id]), 2)].into_iter().collect()
        );
    }

    #[test]
    fn inventory_validation_checks_manifest_and_blob_metadata_only() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let result = result(root.path(), b"live");
        assert!(!store.unit_is_complete(&result.id).unwrap());
        store.publish_unit(&result).unwrap();
        assert!(store.unit_is_complete(&result.id).unwrap());
        let blob = root.path().join(hex(blake3::hash(b"live").as_bytes()));
        fs::write(&blob, b"same").unwrap();
        assert!(store.unit_is_complete(&result.id).unwrap());
        fs::write(&blob, b"wrong size").unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        fs::remove_file(&blob).unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        fs::create_dir(&blob).unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        fs::remove_dir(&blob).unwrap();
        fs::write(&blob, b"live").unwrap();
        let manifest = root.path().join(UNITS).join(hex(&result.id));
        fs::write(&manifest, b"invalid").unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        fs::remove_file(&manifest).unwrap();
        fs::create_dir(&manifest).unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        let mut malformed = result.bytes;
        malformed.push(0);
        let malformed_id = *blake3::hash(&malformed).as_bytes();
        fs::write(root.path().join(UNITS).join(hex(&malformed_id)), malformed).unwrap();
        assert!(!store.unit_is_complete(&malformed_id).unwrap());
    }

    #[test]
    fn concurrent_identical_publication_creates_complete_inventory() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("blobs");
        let store = SnapshotStore::new(&root);
        let result = UnitResult::new(Vec::new());
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    store.publish_unit(&result).unwrap();
                });
            }
        });
        assert!(store.unit_is_complete(&result.id).unwrap());
        assert!(!store.has_receipt(b"build").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_validation_and_legacy_collection_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"live").unwrap();
        let store = SnapshotStore::new(root.path());
        let result = result(root.path(), b"live");
        store.publish_unit(&result).unwrap();
        let blob = root.path().join(hex(blake3::hash(b"live").as_bytes()));
        fs::remove_file(&blob).unwrap();
        symlink(&sentinel, &blob).unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        fs::remove_file(&blob).unwrap();
        fs::write(&blob, b"live").unwrap();
        let manifest = root.path().join(UNITS).join(hex(&result.id));
        fs::remove_file(&manifest).unwrap();
        fs::write(outside.path().join("manifest"), &result.bytes).unwrap();
        symlink(outside.path().join("manifest"), &manifest).unwrap();
        assert!(!store.unit_is_complete(&result.id).unwrap());
        fs::remove_file(&manifest).unwrap();
        symlink(outside.path(), root.path().join("local-v1")).unwrap();
        clean(&store, None, 1, false).unwrap();
        assert!(fs::symlink_metadata(root.path().join("local-v1")).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"live");
        assert_eq!(
            fs::read(outside.path().join("manifest")).unwrap(),
            result.bytes
        );
    }

    #[test]
    fn fresh_snapshot_throttles_receipt_replacement() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let id = save(&store, b"build", result(root.path(), b"live"), 1);
        let path = store.receipt_path(b"build");
        let before = fs::read(&path).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        store
            .save(b"build", vec![id], USAGE_UPDATE_INTERVAL)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            metadata.modified().unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&path).unwrap().ino(), metadata.ino());
        }
        store
            .save(b"build", vec![id], USAGE_UPDATE_INTERVAL + 1)
            .unwrap();
        let receipt = read_receipt(&path).unwrap();
        assert_eq!(
            receipt.usage[&snapshot_id(&mut vec![id])],
            USAGE_UPDATE_INTERVAL + 1
        );
    }

    #[test]
    fn failed_build_publication_does_not_retain_orphan_objects() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let result = result(root.path(), b"failed build output");
        store.publish_unit(&result).unwrap();
        assert!(store.unit_is_complete(&result.id).unwrap());
        assert!(!store.has_receipt(b"build").unwrap());
        let plan = store.plan_collection(None, 1).unwrap();
        assert!(plan.forgotten.is_empty());
        assert!(
            plan.removals
                .contains(&root.path().join(UNITS).join(hex(&result.id)))
        );
        assert!(
            plan.removals.contains(
                &root
                    .path()
                    .join(hex(blake3::hash(b"failed build output").as_bytes()))
            )
        );
    }

    #[test]
    fn corrupt_receipt_stops_collection_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let revision = store.revision().unwrap();
        save(&store, b"build", result(root.path(), b"live"), 1);
        let receipt = store.receipt_path(b"build");
        fs::write(&receipt, b"truncated").unwrap();
        assert!(store.has_receipt(b"build").is_err());
        assert!(clean(&store, Some(0), RETENTION + 2, false).is_err());
        assert_eq!(store.revision().unwrap(), revision);
        assert_eq!(fs::read(&receipt).unwrap(), b"truncated");
        assert_eq!(object_files(&root.path().join(SNAPSHOTS)).unwrap().len(), 1);
    }

    #[test]
    fn existing_corrupt_objects_are_repaired_before_receipt_publication() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let result = result(root.path(), b"live");
        let id = result.id;
        let snapshot = snapshot_id(&mut vec![id]);
        fs::create_dir(root.path().join(UNITS)).unwrap();
        fs::create_dir(root.path().join(SNAPSHOTS)).unwrap();
        fs::write(root.path().join(UNITS).join(hex(&id)), b"broken").unwrap();
        fs::write(root.path().join(SNAPSHOTS).join(hex(&snapshot)), b"broken").unwrap();
        save(&store, b"build", result, 1);
        let bytes = fs::read(root.path().join(UNITS).join(hex(&id))).unwrap();
        assert!(decode_unit(&bytes, &id).is_ok());
        assert!(store.plan_collection(None, 1).unwrap().removals.is_empty());
    }

    #[test]
    fn expired_import_lease_is_not_renewed() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        save(&store, b"build", result(root.path(), b"imported"), 1);
        fs::remove_dir_all(root.path().join(LOCAL)).unwrap();
        let plan = store.plan_collection(None, 2).unwrap();
        assert!(plan.removals.is_empty());
        assert!(!root.path().join(LOCAL).exists());
        let revision = store.revision().unwrap();
        clean(&store, None, 2, false).unwrap();
        assert_eq!(store.revision().unwrap(), revision);
        let expired = store.plan_collection(None, RETENTION + 3).unwrap();
        assert_eq!(expired.removals.len(), 3);
        assert_eq!(
            expired.forgotten,
            vec![root.path().join(LOCAL).join("imports")]
        );
    }

    #[test]
    fn pressure_preserves_shared_blobs_until_last_retaining_unit() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let shared = result(root.path(), b"shared");
        let first_id = shared.id;
        save(&store, b"first", shared, 1);
        let second_id = save(&store, b"second", result(root.path(), b"other"), 2);
        store.save(b"second", vec![first_id, second_id], 3).unwrap();
        let plan = store.plan_collection(Some(6), 3).unwrap();
        // The newest graph owns both outputs; no partial graph fits in six
        // bytes, even after dropping both older snapshots.
        assert!(
            plan.removals
                .contains(&root.path().join(hex(blake3::hash(b"shared").as_bytes())))
        );
        assert!(
            plan.removals
                .contains(&root.path().join(hex(blake3::hash(b"other").as_bytes())))
        );
        assert!(plan.forgotten.contains(&store.receipt_path(b"first")));
        assert!(plan.forgotten.contains(&store.receipt_path(b"second")));
    }

    #[test]
    fn pressure_keeps_shared_bytes_in_the_newer_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let shared = result(root.path(), b"shared");
        let old = result(root.path(), b"old");
        let shared_id = shared.id;
        let old_id = old.id;
        store.publish_unit(&shared).unwrap();
        store.publish_unit(&old).unwrap();
        store.save(b"first", vec![shared_id, old_id], 1).unwrap();
        let new = result(root.path(), b"new");
        let new_id = new.id;
        store.publish_unit(&new).unwrap();
        store.save(b"second", vec![shared_id, new_id], 2).unwrap();

        let plan = store.plan_collection(Some(9), 2).unwrap();
        assert!(
            plan.removals
                .contains(&root.path().join(hex(blake3::hash(b"old").as_bytes())))
        );
        assert!(
            !plan
                .removals
                .contains(&root.path().join(hex(blake3::hash(b"shared").as_bytes())))
        );
        assert!(
            !plan
                .removals
                .contains(&root.path().join(hex(blake3::hash(b"new").as_bytes())))
        );
        let kept = snapshot_id(&mut vec![shared_id, new_id]);
        assert!(
            !plan
                .removals
                .contains(&root.path().join(SNAPSHOTS).join(hex(&kept)))
        );
    }

    #[test]
    fn one_snapshot_is_pinned_by_both_build_directory_receipts() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let id = save(&store, b"first", result(root.path(), b"shared"), 1);
        save(&store, b"second", result(root.path(), b"shared"), RETENTION);
        let manifest = root
            .path()
            .join(SNAPSHOTS)
            .join(hex(&snapshot_id(&mut vec![id])));
        let first_expired = store.plan_collection(None, RETENTION + 100).unwrap();
        assert!(first_expired.removals.is_empty());
        assert!(first_expired.receipts.is_empty());
        assert_eq!(first_expired.forgotten, vec![store.receipt_path(b"first")]);
        clean(&store, None, RETENTION + 100, false).unwrap();
        let both_expired = store.plan_collection(None, RETENTION * 2 + 1).unwrap();
        assert!(both_expired.removals.contains(&manifest));
        assert!(
            both_expired
                .removals
                .contains(&root.path().join(hex(blake3::hash(b"shared").as_bytes())))
        );
        assert!(
            !both_expired
                .forgotten
                .contains(&store.receipt_path(b"first"))
        );
        assert!(
            both_expired
                .forgotten
                .contains(&store.receipt_path(b"second"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn publication_failure_preserves_previous_receipt() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let first = save(&store, b"build", result(root.path(), b"old"), 1);
        let before = fs::read(store.receipt_path(b"build")).unwrap();
        let next = result(root.path(), b"new");
        let next_result = next.id;
        let id = snapshot_id(&mut vec![next_result]);
        symlink(
            root.path()
                .join(SNAPSHOTS)
                .join(hex(&snapshot_id(&mut vec![first]))),
            root.path().join(SNAPSHOTS).join(hex(&id)),
        )
        .unwrap();
        store.publish_unit(&next).unwrap();
        assert!(store.save(b"build", vec![next_result], 2).is_err());
        assert_eq!(fs::read(store.receipt_path(b"build")).unwrap(), before);
    }
}
