use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::util::data_structures::{HashMap, HashSet};
use crate::{CargoResult, compiler::layout::BlobStorageLayout};

pub struct BlobStorage {
    layout: BlobStorageLayout,
    /// Hashes deduplicated since the last flush for last-use tracking.
    used: Mutex<HashSet<String>>,
    /// Out-dir file path to blob hash, recorded at dedup time.
    ///
    /// Lets fresh units resolve their blobs without rehashing.
    paths: Mutex<HashMap<PathBuf, String>>,
}

impl BlobStorage {
    pub fn new(layout: BlobStorageLayout) -> Self {
        Self {
            layout,
            used: Mutex::new(HashSet::default()),
            paths: Mutex::new(HashMap::default()),
        }
    }

    /// Inserts or deduplicates a file already in blob storage.
    ///
    /// This will return an error if we fail during deduplication in such a way
    /// that the original file was not recovered.
    pub fn insert_or_dedup(&self, path: &Path) -> CargoResult<()> {
        let Ok(hash) = Self::hash_file(path) else {
            return Ok(());
        };
        self.insert_hashed(path, hash)
    }

    /// Hashes a file and dedups it, returning the hash.
    ///
    /// Returns `None` if the file could not be hashed.
    pub(crate) fn dedup_file(&self, path: &Path) -> CargoResult<Option<String>> {
        let Ok(hash) = Self::hash_file(path) else {
            return Ok(None);
        };
        self.insert_hashed(path, hash.clone())?;
        Ok(Some(hash))
    }

    /// Hashes a file for blob storage.
    pub(crate) fn hash_file(path: &Path) -> CargoResult<String> {
        let mut file = File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().to_hex().to_string())
    }
    /// Deduplicates a file with a known hash.
    pub(crate) fn insert_hashed(&self, path: &Path, hash: String) -> CargoResult<()> {
        let file_in_blob_storage = self.layout.root().join(&hash);

        let Err(err) = std::fs::hard_link(path, &file_in_blob_storage) else {
            self.note_used(path, hash);
            return Ok(());
        };

        if err.kind() != ErrorKind::AlreadyExists {
            return Ok(());
        }

        let backup_location = {
            let mut backup = path.as_os_str().to_owned();
            backup.push(".backup");
            PathBuf::from(backup)
        };
        let Ok(()) = std::fs::rename(path, &backup_location) else {
            return Ok(());
        };

        if let Err(_) = std::fs::hard_link(&file_in_blob_storage, path) {
            // We failed to hardlink the blob file into our directory, we need to restore the backup
            // If we can't restore, we need to throw an error
            std::fs::rename(backup_location, path)?;
            return Ok(());
        }
        self.note_used(path, hash);

        let _ = std::fs::remove_file(backup_location);

        Ok(())
    }

    /// Returns the blob storage root directory.
    pub fn root(&self) -> &Path {
        self.layout.root()
    }

    /// Removes and returns hashes deduplicated since the last call.
    pub fn take_used_hashes(&self) -> Vec<String> {
        match self.used.lock() {
            Ok(mut used) => std::mem::take(&mut *used).into_iter().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Returns the recorded blob hash for an out-dir file, if any.
    pub fn hash_for_path(&self, path: &Path) -> Option<String> {
        self.paths.lock().ok()?.get(path).cloned()
    }

    /// Marks a previously deduplicated file as used without rehashing.
    ///
    /// Returns false if the path was never recorded, and the caller
    /// should fall back to `insert_or_dedup`.
    pub fn mark_used(&self, path: &Path) -> bool {
        let Some(hash) = self.hash_for_path(path) else {
            return false;
        };
        self.mark_hash_used(hash);
        true
    }

    /// Marks a blob hash as used and records its out-dir path.
    pub(crate) fn mark_hash_used_path(&self, path: &Path, hash: &str) {
        self.mark_hash_used(hash.to_string());
        if let Ok(mut paths) = self.paths.lock() {
            paths.insert(path.to_path_buf(), hash.to_string());
        }
    }

    fn note_used(&self, path: &Path, hash: String) {
        self.mark_hash_used(hash.clone());
        if let Ok(mut paths) = self.paths.lock() {
            paths.insert(path.to_path_buf(), hash);
        }
    }

    fn mark_hash_used(&self, hash: String) {
        if let Ok(mut used) = self.used.lock() {
            used.insert(hash);
        }
    }
}
