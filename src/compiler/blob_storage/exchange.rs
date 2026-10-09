//! Transfer cache entries and their blobs through REAPI.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, ensure};
use bazel_remote_apis::build::bazel::remote::execution::v2::{
    ActionResult, Digest as RemoteDigest, OutputFile,
};

use super::format::{CacheEntry, CachedOutput, Digest};
use super::remote::{RemoteCache, verify_contents};
use super::snapshots::{
    SnapshotStore, blob_path, check_directory, hex, publish_blob, regular_size,
};
use super::{BlobStorage, restore_path};
use crate::CargoResult;
use crate::util::data_structures::{HashMap, HashSet};

const MAX_METADATA_SIZE: i64 = 16 * 1024 * 1024;
const CACHE_ENTRY: &str = "cache-entry";
// Bump when the remote ActionResult layout changes.
const KEY_VERSION: &str = "cargo-remote-cache-v3";

pub(super) fn publish(
    remote: &RemoteCache,
    root: &Path,
    unit_hash: &str,
    entry: &CacheEntry,
) -> CargoResult<()> {
    tracing::debug!(unit_hash, "publishing unit to remote cache");
    let staging = tempfile::Builder::new()
        .prefix(".remote-upload")
        .tempdir_in(root)?;
    let cache_entry = staging.path().join(CACHE_ENTRY);
    // The local cache entry can be replaced by another Cargo invocation.
    fs::write(&cache_entry, entry.encode())?;
    let mut paths = Vec::with_capacity(entry.outputs.len() + 1);
    paths.push(cache_entry);
    let mut names = Vec::with_capacity(entry.outputs.len() + 1);
    names.push(CACHE_ENTRY.to_owned());
    let mut seen = HashSet::default();
    for output in &entry.outputs {
        if seen.insert(output.hash) {
            let hash = hex(&output.hash);
            let path = blob_path(root, &output.hash);
            ensure!(
                regular_size(&path)? == Some(output.size),
                "missing or invalid blob during remote publication"
            );
            paths.push(path);
            names.push(format!("blobs/{hash}"));
        }
    }
    let digests = remote.upload_files(&paths)?;
    ensure!(digests.len() == names.len(), "incomplete remote upload");
    let output_files = names
        .into_iter()
        .zip(digests)
        .map(|(path, digest)| OutputFile {
            path,
            digest: Some(digest),
            ..Default::default()
        })
        .collect();
    remote.update_action(
        &key(unit_hash, entry.fingerprint),
        ActionResult {
            output_files,
            ..Default::default()
        },
    )?;
    tracing::debug!(unit_hash, "published unit to remote cache");
    Ok(())
}

/// Returns the restored entry and the digest of its local encoding.
pub(super) fn fetch(
    remote: &RemoteCache,
    root: &Path,
    unit_hash: &str,
    fingerprint: u64,
) -> CargoResult<Option<(Digest, Arc<CacheEntry>)>> {
    let Some(found) = lookup(remote, root, unit_hash, fingerprint)? else {
        return Ok(None);
    };
    found.finish(remote, root, unit_hash).map(Some)
}

/// A remote cache entry whose blobs have not all been downloaded.
pub(super) struct Found {
    pub entry: Arc<CacheEntry>,
    /// Blobs missing locally, by index into `entry.outputs`.
    missing: Vec<(usize, RemoteDigest)>,
    staging: tempfile::TempDir,
}

impl Found {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }

    /// Download and publish the missing blobs of outputs matching `select`.
    pub fn download(
        &mut self,
        remote: &RemoteCache,
        root: &Path,
        select: impl Fn(&CachedOutput) -> bool,
    ) -> CargoResult<()> {
        let (selected, rest) = std::mem::take(&mut self.missing)
            .into_iter()
            .partition::<Vec<_>, _>(|(index, _)| select(&self.entry.outputs[*index]));
        self.missing = rest;
        let downloads = selected
            .iter()
            .map(|(index, digest)| {
                let hash = &self.entry.outputs[*index].hash;
                (digest.clone(), self.staging.path().join(hex(hash)))
            })
            .collect();
        remote.download_files(downloads)?;
        for (index, _) in selected {
            let output = &self.entry.outputs[index];
            let downloaded = self.staging.path().join(hex(&output.hash));
            ensure!(
                BlobStorage::hash(&downloaded)? == output.hash,
                "remote blob BLAKE3 digest mismatch"
            );
            publish_blob(&downloaded, &blob_path(root, &output.hash))?;
        }
        Ok(())
    }

    /// Download the remaining blobs and publish the local cache entry.
    pub fn finish(
        mut self,
        remote: &RemoteCache,
        root: &Path,
        unit_hash: &str,
    ) -> CargoResult<(Digest, Arc<CacheEntry>)> {
        self.download(remote, root, |_| true)?;
        let digest = SnapshotStore::new(root).publish_cache_entry(unit_hash, &self.entry)?;
        Ok((digest, self.entry))
    }
}

/// Look up an entry and validate its layout without downloading output blobs.
pub(super) fn lookup(
    remote: &RemoteCache,
    root: &Path,
    unit_hash: &str,
    fingerprint: u64,
) -> CargoResult<Option<Found>> {
    tracing::debug!(unit_hash, "looking up unit in remote cache");
    let Some(result) = remote.get_action(&key(unit_hash, fingerprint), &[CACHE_ENTRY])? else {
        tracing::debug!(unit_hash, "remote cache miss");
        return Ok(None);
    };
    tracing::debug!(unit_hash, "remote cache entry found");
    ensure!(
        result.exit_code == 0,
        "remote cache entry was not successful"
    );
    #[expect(
        deprecated,
        reason = "Reject symlinks returned by older REAPI servers."
    )]
    let legacy_symlinks =
        !result.output_file_symlinks.is_empty() || !result.output_directory_symlinks.is_empty();
    ensure!(
        result.output_directories.is_empty()
            && !legacy_symlinks
            && result.output_symlinks.is_empty(),
        "unsupported remote cache output type"
    );
    let mut files = HashMap::default();
    for output in result.output_files {
        ensure!(!output.is_executable, "executable remote cache container");
        ensure!(output.digest.is_some(), "missing remote output digest");
        ensure!(
            !files.contains_key(&output.path),
            "duplicate remote cache output"
        );
        files.insert(output.path.clone(), output);
    }
    check_directory(root)?;
    let staging = tempfile::Builder::new()
        .prefix(".remote-download")
        .tempdir_in(root)?;
    let cache_entry = read_metadata(
        remote,
        files
            .remove(CACHE_ENTRY)
            .context("missing remote cache entry")?,
        &staging.path().join(CACHE_ENTRY),
    )?;
    let entry = CacheEntry::decode_native(&cache_entry)?;
    if entry.fingerprint != fingerprint {
        tracing::debug!(
            unit_hash,
            "rejecting remote cache entry with mismatched fingerprint"
        );
        return Ok(None);
    }
    let mut missing = Vec::with_capacity(entry.outputs.len());
    let mut seen = HashMap::default();
    for (index, output) in entry.outputs.iter().enumerate() {
        restore_path(&output.path)?;
        if let Some(size) = seen.insert(output.hash, output.size) {
            ensure!(size == output.size, "inconsistent remote blob size");
            continue;
        }
        let name = format!("blobs/{}", hex(&output.hash));
        let digest = files
            .remove(name.as_str())
            .and_then(|file| file.digest)
            .context("missing remote output blob")?;
        ensure!(
            u64::try_from(digest.size_bytes).ok() == Some(output.size),
            "remote blob size does not match cache entry"
        );
        let destination = blob_path(root, &output.hash);
        if regular_size(&destination)? == Some(output.size)
            && BlobStorage::hash(&destination)? == output.hash
        {
            tracing::debug!(
                unit_hash,
                digest = %digest.hash,
                bytes = digest.size_bytes,
                "reusing local blob for remote cache entry"
            );
            continue;
        }
        missing.push((index, digest));
    }
    ensure!(files.is_empty(), "unexpected remote cache output");
    Ok(Some(Found {
        entry: Arc::new(entry),
        missing,
        staging,
    }))
}

/// Prefer contents inlined by GetActionResult. Servers may omit them.
fn read_metadata(remote: &RemoteCache, file: OutputFile, path: &Path) -> CargoResult<Vec<u8>> {
    let digest = file
        .digest
        .as_ref()
        .context("missing remote output digest")?;
    ensure!(
        (0..=MAX_METADATA_SIZE).contains(&digest.size_bytes),
        "remote cache metadata exceeds size limit"
    );
    if !file.contents.is_empty() {
        verify_contents(digest, &file.contents)?;
        tracing::debug!(path = %file.path, "using inlined remote cache metadata");
        return Ok(file.contents);
    }
    remote.download_files(vec![(digest.clone(), path.to_path_buf())])?;
    Ok(fs::read(path)?)
}

fn key(unit_hash: &str, fingerprint: u64) -> Vec<u8> {
    format!(
        "{KEY_VERSION}/{}/{unit_hash}/{fingerprint:016x}",
        std::env::consts::OS
    )
    .into_bytes()
}
