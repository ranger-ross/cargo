//! Transfer cache entries and their complete output graph through REAPI.

use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};
use bazel_remote_apis::build::bazel::remote::execution::v2::{ActionResult, Digest, OutputFile};

use super::format::{CacheEntry, Output, UnitOutput, decode_native_unit};
use super::remote::RemoteCache;
use super::snapshots::{SnapshotStore, check_directory, hex, regular_size};
use super::{BlobStorage, restore_path};
use crate::CargoResult;
use crate::util::data_structures::{HashMap, HashSet};

const MAX_METADATA_SIZE: i64 = 16 * 1024 * 1024;

pub(super) fn publish(
    remote: &RemoteCache,
    root: &Path,
    unit_hash: &str,
    entry: &CacheEntry,
    outputs: &[Output],
) -> CargoResult<()> {
    tracing::debug!(unit_hash, "publishing unit to remote cache");
    let store = SnapshotStore::new(root);
    let staging = tempfile::Builder::new()
        .prefix(".remote-upload")
        .tempdir_in(root)?;
    let cache_entry = staging.path().join("cache-entry");
    // The local cache entry can be replaced by another Cargo invocation.
    fs::write(&cache_entry, entry.encode())?;
    let mut paths = Vec::with_capacity(outputs.len() + 2);
    paths.push(cache_entry);
    paths.push(store.unit_path(&entry.unit_output));
    let mut names = Vec::with_capacity(outputs.len() + 2);
    names.push("cache-entry".to_owned());
    names.push("unit-output".to_owned());
    let mut seen = HashSet::default();
    for output in outputs {
        if seen.insert(output.hash) {
            let hash = hex(&output.hash);
            let path = root.join(&hash);
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

pub(super) fn fetch(
    remote: &RemoteCache,
    root: &Path,
    unit_hash: &str,
    fingerprint: u64,
) -> CargoResult<Option<CacheEntry>> {
    tracing::debug!(unit_hash, "looking up unit in remote cache");
    let Some(result) = remote.get_action(&key(unit_hash, fingerprint))? else {
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
    for output in &result.output_files {
        ensure!(!output.is_executable, "executable remote cache container");
        let digest = output
            .digest
            .as_ref()
            .context("missing remote output digest")?;
        ensure!(
            files.insert(output.path.as_str(), digest).is_none(),
            "duplicate remote cache output"
        );
    }
    check_directory(root)?;
    let staging = tempfile::Builder::new()
        .prefix(".remote-download")
        .tempdir_in(root)?;
    let cache_entry = download_metadata(
        remote,
        files
            .remove("cache-entry")
            .context("missing remote cache entry")?,
        &staging.path().join("cache-entry"),
    )?;
    let entry = CacheEntry::decode(&cache_entry)?;
    if entry.fingerprint != fingerprint {
        tracing::debug!(
            unit_hash,
            "rejecting remote cache entry with mismatched fingerprint"
        );
        return Ok(None);
    }
    let bytes = download_metadata(
        remote,
        files
            .remove("unit-output")
            .context("missing remote unit output")?,
        &staging.path().join("unit-output"),
    )?;
    let outputs = decode_native_unit(&bytes, &entry.unit_output)?;
    ensure!(
        outputs.len() == entry.outputs.len(),
        "remote output metadata count mismatch"
    );
    let mut blobs = Vec::with_capacity(outputs.len());
    let mut seen = HashMap::default();
    for output in &outputs {
        restore_path(&output.path)?;
        if let Some(size) = seen.insert(output.hash, output.size) {
            ensure!(size == output.size, "inconsistent remote blob size");
            continue;
        }
        let name = format!("blobs/{}", hex(&output.hash));
        let digest = files
            .remove(name.as_str())
            .context("missing remote output blob")?;
        ensure!(
            u64::try_from(digest.size_bytes).ok() == Some(output.size),
            "remote blob size does not match unit output"
        );
        blobs.push((output, digest));
    }
    ensure!(files.is_empty(), "unexpected remote cache output");
    for (output, digest) in blobs {
        let name = hex(&output.hash);
        let destination = root.join(&name);
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
        let downloaded = staging.path().join(name);
        remote.download_file(digest, &downloaded)?;
        ensure!(
            BlobStorage::hash(&downloaded)? == output.hash,
            "remote blob BLAKE3 digest mismatch"
        );
        fs::rename(downloaded, destination)?;
    }
    let store = SnapshotStore::new(root);
    store.publish_unit(&UnitOutput {
        id: entry.unit_output,
        bytes,
    })?;
    store.publish_cache_entry(unit_hash, &entry)?;
    Ok(Some(entry))
}

fn download_metadata(remote: &RemoteCache, digest: &Digest, path: &Path) -> CargoResult<Vec<u8>> {
    ensure!(
        (0..=MAX_METADATA_SIZE).contains(&digest.size_bytes),
        "remote cache metadata exceeds size limit"
    );
    remote.download_file(digest, path)?;
    Ok(fs::read(path)?)
}

fn key(unit_hash: &str, fingerprint: u64) -> Vec<u8> {
    format!(
        "cargo-remote-cache-v1/{}/{unit_hash}/{fingerprint:016x}",
        std::env::consts::OS
    )
    .into_bytes()
}
