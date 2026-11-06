use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use cargo_util::Sha256;

use crate::{CargoResult, compiler::layout::BlobStorageLayout};

pub struct BlobStorage {
    layout: BlobStorageLayout,
}

impl BlobStorage {
    pub fn new(layout: BlobStorageLayout) -> Self {
        Self { layout }
    }

    /// Inserts or deduplicates a file already in blob storage.
    ///
    /// This will return an error if we fail during deduplication in such a way
    /// that the original file was not recovered.
    pub fn insert_or_dedup(&self, path: &Path) -> CargoResult<()> {
        let Ok(hash) = Self::hash(path) else {
            return Ok(());
        };
        let file_in_blob_storage = self.layout.root().join(hash);

        let Err(err) = std::fs::hard_link(path, &file_in_blob_storage) else {
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

        let _ = std::fs::remove_file(backup_location);

        Ok(())
    }

    fn hash(path: &Path) -> CargoResult<String> {
        let mut hasher = Sha256::new();
        hasher.update_path(path)?;
        Ok(hasher.finish_hex())
    }
}
