mod snapshots;

use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::bail;
use cargo_util::paths::create_dir_all;
use filetime::FileTime;
use parking_lot::Mutex;
use tracing::instrument;

use crate::compiler::fingerprint::Fingerprint;
use crate::ops::CleanContext;
use crate::util::cache_lock::CacheLockMode;
use crate::util::data_structures::{HashMap, HashSet};
use crate::{CargoResult, GlobalContext};
use snapshots::{Digest, Output, SnapshotIndex, StoredUnit, UnitResult};

pub struct BlobStorage {
    root: PathBuf,
    build_dir: PathBuf,
    build_dir_id: Vec<u8>,
    tracking: Mutex<Tracking>,
}

#[derive(Default)]
struct Tracking {
    known: HashMap<Vec<u8>, StoredUnit>,
    used: HashSet<Vec<u8>>,
    invalidated: HashSet<Vec<u8>>,
    updates: HashMap<Vec<u8>, UnitResult>,
}

impl BlobStorage {
    pub fn new(root: PathBuf, build_dir: &Path, gctx: &GlobalContext) -> CargoResult<Self> {
        create_dir_all(&root)?;

        if !is_same_filesystem(&root, build_dir).unwrap_or(false) {
            bail!("blob storage and build-dir are on different file systems")
        }

        let build_dir_id = std::fs::canonicalize(build_dir)?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        let known = SnapshotIndex::open(&root)?.load_units(&build_dir_id)?;
        Ok(Self {
            root,
            build_dir: build_dir.to_path_buf(),
            build_dir_id,
            tracking: Mutex::new(Tracking {
                known,
                ..Tracking::default()
            }),
        })
    }

    /// Records membership without opening the unit's output files.
    ///
    /// Returns whether the completed output set needs to be captured.
    pub fn prepare_unit(&self, unit_dir: &Path, generation: Option<Digest>) -> CargoResult<bool> {
        let key = unit_dir
            .strip_prefix(&self.build_dir)?
            .as_os_str()
            .as_encoded_bytes();
        let mut tracking = self.tracking.lock();
        let capture = !tracking
            .known
            .get(key)
            .is_some_and(|unit| Some(unit.generation) == generation);
        if capture {
            tracking.known.remove(key);
            tracking.invalidated.insert(key.to_vec());
        }
        tracking.used.insert(key.to_vec());
        Ok(capture)
    }

    /// Captures a newly completed unit or adopts previously untracked outputs.
    #[instrument(skip_all)]
    pub fn capture_unit(
        &self,
        unit_dir: &Path,
        out_dir: &Path,
        fingerprint: &Fingerprint,
    ) -> CargoResult<()> {
        let mut outputs = Vec::new();
        for entry in walkdir::WalkDir::new(out_dir) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let size = entry.metadata()?.len();
            let hash = Self::hash(path)?;
            let storage_path = self
                .root
                .join(blake3::Hash::from_bytes(hash).to_hex().as_str());
            if !self.insert(path, &storage_path)? {
                self.dedup(path, &storage_path)?;
            }
            outputs.push(Output {
                path: path
                    .strip_prefix(unit_dir)?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
                hash,
                size,
            });
        }
        let mut result = UnitResult::new(outputs);
        result.generation = fingerprint.capture_blob_generation()?;
        let key = unit_dir
            .strip_prefix(&self.build_dir)?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec();
        let mut tracking = self.tracking.lock();
        tracking.known.insert(
            key.clone(),
            StoredUnit {
                result: result.id,
                generation: result.generation,
            },
        );
        tracking.updates.insert(key, result);
        Ok(())
    }

    /// Saves completed units even when another unit failed.
    ///
    /// Only successful builds publish a graph snapshot.
    pub fn finish(&self, gctx: &GlobalContext, successful: bool) -> CargoResult<()> {
        let tracking = self.tracking.lock();
        if tracking.used.is_empty() {
            return Ok(());
        }
        let results = successful.then(|| {
            tracking
                .used
                .iter()
                .filter_map(|key| tracking.known.get(key).map(|unit| unit.result))
                .collect::<Vec<_>>()
        });
        let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        SnapshotIndex::open(&self.root)?.save(
            &self.build_dir_id,
            &tracking.invalidated,
            &tracking.updates,
            results,
            now(),
        )
    }

    /// Collects expired snapshots under the package-cache mutation lock.
    pub fn clean(
        root: &Path,
        clean_ctx: &mut CleanContext<'_>,
        max_size: Option<u64>,
    ) -> CargoResult<()> {
        if !root.try_exists()? {
            return Ok(());
        }
        SnapshotIndex::open_for_clean(root, clean_ctx.dry_run)?.clean(
            root,
            clean_ctx,
            max_size,
            now(),
        )
    }

    fn insert(&self, artifact_path: &Path, storage_path: &Path) -> CargoResult<bool> {
        if storage_path.try_exists()? {
            return Ok(false);
        }

        // This logic is a bit subtle.
        // Reflinking is not atomic so we create a temp dir to create that file falling back to
        // hardlinking. Then regardless of whether we reflinked or hardlinked, we hard link that
        // file into the blob storage so the insert is always atomic.
        //
        // Importantly, we do not use `std::fs::rename` to move the file in to the blob storage as
        // that would overwrite the existing file if there was another process inserted before us.
        let staging_dir = tempfile::Builder::new()
            .prefix(".blob")
            .tempdir_in(&self.root)?;
        let staged = staging_dir.path().join("artifact");
        if reflink_copy::reflink(artifact_path, &staged).is_err() {
            std::fs::hard_link(artifact_path, &staged)?;
        }
        #[cfg(target_os = "linux")]
        ensure_no_writers(&staged)?;

        match std::fs::hard_link(&staged, storage_path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn dedup(&self, path: &Path, storage_path: &Path) -> CargoResult<()> {
        let metadata = path.metadata()?;
        let staging_dir = tempfile::Builder::new()
            .prefix(".blob")
            .tempdir_in(path.parent().unwrap())?;
        let replacement = staging_dir.path().join("artifact");
        if reflink_copy::reflink(storage_path, &replacement).is_ok() {
            // Reflink creates a different inode so things like mtimes are not preserved
            // automatically, which causes issues with rebuild detection.
            std::fs::set_permissions(&replacement, metadata.permissions())?;
            filetime::set_file_times(
                &replacement,
                FileTime::from_last_access_time(&metadata),
                FileTime::from_last_modification_time(&metadata),
            )?;
        } else {
            std::fs::hard_link(storage_path, &replacement)?;
        }
        #[cfg(target_os = "linux")]
        ensure_no_writers(&replacement)?;
        std::fs::rename(&replacement, path)?;

        Ok(())
    }

    #[instrument]
    fn hash(path: &Path) -> CargoResult<Digest> {
        let mut hasher = blake3::Hasher::new();
        let file = File::open(path)?;
        hasher.update_reader(file)?;
        Ok(*hasher.finalize().as_bytes())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Errors if there we cannot get a read lease to a file (ensuring no writers)
///
/// On Linux we need this to avoid concurrency issues when deduplicating with reflinks.
/// During reflinking, there is a brief window where we hold a write lease to the file.
/// If during this period, another worker fork's (say to spawn a rustc process) that process
/// will inhierit the writable fd. This is problematic as we cannot execute a file that has a
/// writable fd which breaks things like executing build scripts.
#[cfg(target_os = "linux")]
fn ensure_no_writers(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let file = File::open(path)?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn is_same_filesystem(dir1: &Path, dir2: &Path) -> std::io::Result<bool> {
    cfg_select! {
        unix => {
            use std::os::unix::fs::MetadataExt;
            let meta1 = std::fs::metadata(dir1)?;
            let meta2 = std::fs::metadata(dir2)?;
            Ok(meta1.dev() == meta2.dev())
        }
        windows => {
            use std::fs::OpenOptions;
            use std::os::windows::fs::OpenOptionsExt;
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
                GetFileInformationByHandle,
            };

            // FIXME: Ideally we use std if/when https://github.com/rust-lang/rust/issues/63010 is
            // stabilized
            fn volume_serial_number(path: &Path) -> std::io::Result<u32> {
                let file = OpenOptions::new()
                    .access_mode(0)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                    .open(path)?;
                let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
                if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(info.dwVolumeSerialNumber)
            }

            Ok(volume_serial_number(dir1)? == volume_serial_number(dir2)?)
        }
    }
}
