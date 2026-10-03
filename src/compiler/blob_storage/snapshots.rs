//! Portable immutable graph inventories with local usage receipts.
//!
//! The caller holds the package-cache lock during publication and collection.
//! Receipts are replaced last when publishing, and first when collecting.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

use super::format::{
    Digest, Receipt, StoredUnit, UnitResult, decode_snapshot, decode_unit, digest_filename,
    encode_snapshot, snapshot_id,
};
use crate::CargoResult;
use crate::ops::CleanContext;
use crate::util::data_structures::{HashMap, HashSet};

const USAGE_UPDATE_INTERVAL: u64 = 4 * 60 * 60;
const RETENTION: u64 = 30 * 24 * 60 * 60;
const UNITS: &str = "units-v1";
const SNAPSHOTS: &str = "snapshots-v1";
const LOCAL: &str = "local-v1";

pub(super) struct SnapshotStore<'a> {
    root: &'a Path,
}

impl<'a> SnapshotStore<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }

    pub fn load_units(&self, build_dir: &[u8]) -> CargoResult<HashMap<Vec<u8>, StoredUnit>> {
        self.check_directories()?;
        Ok(read_receipt(&self.receipt_path(build_dir))?.units)
    }

    /// Completed units survive a failed build, but slots are never GC roots.
    /// Reload the receipt under the caller's exclusive lock to merge changes.
    pub fn save(
        &self,
        build_dir: &[u8],
        invalidated: &HashSet<Vec<u8>>,
        updates: &HashMap<Vec<u8>, UnitResult>,
        snapshot: Option<Vec<Digest>>,
        now: u64,
    ) -> CargoResult<()> {
        self.check_directories()?;
        let path = self.receipt_path(build_dir);
        let mut receipt = read_receipt(&path)?;
        let mut changed = false;
        for unit_path in invalidated {
            changed |= receipt.units.remove(unit_path).is_some();
        }
        for (unit_path, result) in updates {
            publish_object(&self.root.join(UNITS).join(hex(&result.id)), &result.bytes)?;
            let stored = StoredUnit {
                result: result.id,
                generation: result.generation,
            };
            if receipt.units.get(unit_path) != Some(&stored) {
                receipt.units.insert(unit_path.clone(), stored);
                changed = true;
            }
        }
        if let Some(mut results) = snapshot {
            let id = snapshot_id(&mut results);
            let manifest = self.root.join(SNAPSHOTS).join(hex(&id));
            // Verify existing bytes before trusting the object. On a hit, no
            // serialized graph is allocated and no file is touched for writing.
            let valid = read_regular(&manifest)?.is_some_and(|bytes| {
                // `id` was computed from the canonical graph above, so matching
                // its digest also verifies framing without allocating members.
                blake3::hash(&bytes).as_bytes() == &id
            });
            if !valid {
                atomic_replace(&manifest, &encode_snapshot(&results))?;
            }
            match receipt.usage.get_mut(&id) {
                Some(last_used) if now.saturating_sub(*last_used) >= USAGE_UPDATE_INTERVAL => {
                    *last_used = now;
                    changed = true;
                }
                None => {
                    receipt.usage.insert(id, now);
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            atomic_replace(&path, &receipt.encode())?;
        }
        Ok(())
    }

    pub fn clean(
        &self,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
        now: u64,
    ) -> CargoResult<()> {
        let plan = self.plan_collection(max_size, now)?;
        // Publish every lookup invalidation before removing immutable objects.
        // If one replacement fails, leave all cache data intact.
        if !clean_ctx.dry_run {
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
            ensure!(
                name != "imports" || receipt.units.is_empty(),
                "import receipt contains local unit slots"
            );
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
            let previous_units = receipt.units.len();
            let previous_usage = receipt.usage.len();
            receipt
                .units
                .retain(|_, unit| unit_refs.get(&unit.result).is_some_and(|count| *count > 0));
            receipt
                .usage
                .retain(|id, time| *time >= cutoff && recency.contains_key(id));
            if receipt.units.is_empty() && receipt.usage.is_empty() {
                if path.try_exists()? {
                    plan.forgotten.push(path);
                }
                continue;
            }
            // Newly adopted leases were inserted before counting usage.
            let imports_changed = path.file_name().is_some_and(|name| name == "imports")
                && adopted.iter().any(|id| receipt.usage.contains_key(id));
            if previous_units != receipt.units.len()
                || previous_usage != receipt.usage.len()
                || imports_changed
            {
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
        store
            .save(
                build_dir,
                &HashSet::default(),
                &[(b"unit".to_vec(), result)].into_iter().collect(),
                Some(vec![id]),
                now,
            )
            .unwrap();
        id
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
            .save(
                b"build",
                &HashSet::default(),
                &HashMap::default(),
                Some(vec![id]),
                USAGE_UPDATE_INTERVAL,
            )
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
            .save(
                b"build",
                &HashSet::default(),
                &HashMap::default(),
                Some(vec![id]),
                USAGE_UPDATE_INTERVAL + 1,
            )
            .unwrap();
        let receipt = read_receipt(&path).unwrap();
        assert_eq!(
            receipt.usage[&snapshot_id(&mut vec![id])],
            USAGE_UPDATE_INTERVAL + 1
        );
    }

    #[test]
    fn failed_build_slots_do_not_retain_orphan_objects() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        let result = result(root.path(), b"failed build output");
        store
            .save(
                b"build",
                &HashSet::default(),
                &[(b"unit".to_vec(), result)].into_iter().collect(),
                None,
                1,
            )
            .unwrap();
        let plan = store.plan_collection(None, 1).unwrap();
        assert_eq!(plan.forgotten, vec![store.receipt_path(b"build")]);
        assert_eq!(plan.removals.len(), 2);
        assert_eq!(store.load_units(b"build").unwrap().len(), 1);
    }

    #[test]
    fn corrupt_receipt_stops_collection_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path());
        save(&store, b"build", result(root.path(), b"live"), 1);
        let receipt = store.receipt_path(b"build");
        fs::write(&receipt, b"truncated").unwrap();
        assert!(store.plan_collection(Some(0), RETENTION + 2).is_err());
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
        for (path, bytes) in plan.receipts {
            atomic_replace(&path, &bytes).unwrap();
        }
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
        store
            .save(
                b"second",
                &HashSet::default(),
                &HashMap::default(),
                Some(vec![first_id, second_id]),
                3,
            )
            .unwrap();
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
        let first = [(b"shared".to_vec(), shared), (b"old".to_vec(), old)]
            .into_iter()
            .collect();
        store
            .save(
                b"first",
                &HashSet::default(),
                &first,
                Some(vec![shared_id, old_id]),
                1,
            )
            .unwrap();
        let new = result(root.path(), b"new");
        let new_id = new.id;
        let second = [(b"new".to_vec(), new)].into_iter().collect();
        store
            .save(
                b"second",
                &HashSet::default(),
                &second,
                Some(vec![shared_id, new_id]),
                2,
            )
            .unwrap();

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
        assert_eq!(first_expired.receipts.len(), 1);
        assert_eq!(first_expired.receipts[0].0, store.receipt_path(b"first"));
        let receipt = Receipt::decode(&first_expired.receipts[0].1).unwrap();
        assert!(receipt.usage.is_empty());
        assert_eq!(receipt.units[b"unit".as_slice()].result, id);
        for (path, bytes) in first_expired.receipts {
            atomic_replace(&path, &bytes).unwrap();
        }
        let both_expired = store.plan_collection(None, RETENTION * 2 + 1).unwrap();
        assert!(both_expired.removals.contains(&manifest));
        assert!(
            both_expired
                .removals
                .contains(&root.path().join(hex(blake3::hash(b"shared").as_bytes())))
        );
        assert!(
            both_expired
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
        assert!(
            store
                .save(
                    b"build",
                    &HashSet::default(),
                    &[(b"unit".to_vec(), next)].into_iter().collect(),
                    Some(vec![next_result]),
                    2
                )
                .is_err()
        );
        assert_eq!(fs::read(store.receipt_path(b"build")).unwrap(), before);
    }
}
