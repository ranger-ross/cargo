use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail, ensure};
use bazel_remote_apis::build::bazel::remote::execution::v2 as reapi;
use bazel_remote_apis::google::bytestream;
use cargo_util::Sha256;
use futures::{StreamExt, TryFutureExt, TryStreamExt, stream};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::runtime::{Builder, Runtime};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};

use crate::context::CargoRemoteCacheConfig;
use crate::util::data_structures::{HashMap, HashSet};
use crate::{CargoResult, GlobalContext};

const CHUNK_SIZE: usize = 64 * 1024;
const FIND_MISSING_BATCH: usize = 512;
const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
// Batches stay below the default 4 MiB gRPC message limit on both ends.
const BATCH_BLOB_LIMIT: i64 = 1024 * 1024;
const BATCH_TOTAL_LIMIT: i64 = 3 * 1024 * 1024;
const BATCH_BLOB_OVERHEAD: i64 = 128;
const TRANSFER_CONCURRENCY: usize = 4;
/// Connections for blob transfers. Lookups and other small RPCs use their own
/// connection so they do not queue behind large transfers.
const BULK_CONNECTIONS: usize = 2;
/// Attempts for operations that fail with a transient error.
const ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(250);
const ZSTD_LEVEL: i32 = 3;
const API_KEY_HEADER: &str = "x-buildbuddy-api-key";

// Deliberately not Debug: configuration and metadata may contain credentials.
pub(super) struct RemoteCache {
    endpoint: Endpoint,
    tls: bool,
    instance_name: String,
    api_key: Option<MetadataValue<Ascii>>,
    read_only: bool,
    timeout: Duration,
    transport: Mutex<Option<Arc<Transport>>>,
    /// Learned from GetCapabilities before the first transfer. Concurrent
    /// transfers wait for a single request.
    compression: tokio::sync::OnceCell<Compression>,
}

/// zstd support advertised by the server.
#[derive(Clone, Copy, Default)]
struct Compression {
    /// `compressed-blobs` ByteStream resources and BatchReadBlobs responses.
    stream: bool,
    batch_update: bool,
}

struct Transport {
    runtime: Option<Runtime>,
    channels: Channels,
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Cargo can also be embedded in a process already running Tokio.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[derive(Clone)]
struct Channels {
    control: Channel,
    bulk: Arc<[Channel]>,
    next: Arc<AtomicUsize>,
}

impl Channels {
    fn bulk(&self) -> Channel {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        self.bulk[index % self.bulk.len()].clone()
    }
}

/// A failure worth retrying, such as an unavailable server or a timeout.
#[derive(Debug)]
struct Transient(String);

impl fmt::Display for Transient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Transient {}

impl RemoteCache {
    pub(super) fn from_config(gctx: &GlobalContext) -> CargoResult<Option<Self>> {
        if !gctx.network_allowed() {
            return Ok(None);
        }
        // Do not include deserialization errors, which can quote configuration values.
        let config = gctx
            .get::<Option<CargoRemoteCacheConfig>>(["cache", "remote"])
            .map_err(|_| anyhow!("invalid cache.remote configuration"))?;
        let Some(config) = config else {
            return Ok(None);
        };
        let api_key = config
            .api_key_env
            .as_deref()
            .map(|name| {
                let value = gctx.get_env(name).map_err(|_| {
                    anyhow!("cache.remote.api-key-env variable is missing or not UTF-8")
                })?;
                parse_api_key(value)
            })
            .transpose()?;
        let timeout = Duration::from_secs(config.timeout.unwrap_or(30));
        ensure!(
            !timeout.is_zero()
                && timeout.as_secs() <= 99_999_999 * 3600
                && Instant::now().checked_add(timeout).is_some(),
            "cache.remote.timeout must be a positive, representable gRPC timeout"
        );
        let url = endpoint_url(&config.url)?;
        let tls = url.starts_with("https://");
        // Hyper's default 5 MiB connection window caps throughput near 5 MiB per
        // round trip.
        let endpoint = Endpoint::from_shared(url)
            .map_err(|_| anyhow!("invalid cache.remote.url endpoint"))?
            .connect_timeout(timeout)
            .initial_connection_window_size(64 * 1024 * 1024)
            .initial_stream_window_size(16 * 1024 * 1024);
        let instance_name = config.instance_name.unwrap_or_default();
        validate_instance(&instance_name)?;
        Ok(Some(Self {
            endpoint,
            tls,
            instance_name,
            api_key,
            read_only: config.read_only.unwrap_or(false),
            timeout,
            transport: Mutex::new(None),
            compression: tokio::sync::OnceCell::new(),
        }))
    }

    pub(super) fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// `inline_output_files` is a hint. Servers may omit the contents of any path.
    pub(super) fn get_action(
        &self,
        key: &[u8],
        inline_output_files: &[&str],
    ) -> CargoResult<Option<reapi::ActionResult>> {
        let action_digest = digest_bytes(key)?;
        let inline_output_files: Vec<_> = inline_output_files
            .iter()
            .map(|path| (*path).to_owned())
            .collect();
        self.run(|channels| {
            self.retry(move || {
                let channel = channels.control.clone();
                let request = self.unary_request(reapi::GetActionResultRequest {
                    instance_name: self.instance_name.clone(),
                    action_digest: Some(action_digest.clone()),
                    inline_output_files: inline_output_files.clone(),
                    digest_function: reapi::digest_function::Value::Sha256 as i32,
                    ..Default::default()
                });
                self.deadline("GetActionResult", async move {
                    let mut client = reapi::action_cache_client::ActionCacheClient::new(channel)
                        .max_decoding_message_size(MAX_MESSAGE_SIZE);
                    match client.get_action_result(request).await {
                        Ok(response) => Ok(Some(response.into_inner())),
                        Err(status) if status.code() == Code::NotFound => Ok(None),
                        Err(status) => Err(rpc_error("GetActionResult", status)),
                    }
                })
            })
        })
    }

    pub(super) fn update_action(&self, key: &[u8], result: reapi::ActionResult) -> CargoResult<()> {
        ensure!(!self.read_only, "remote cache is read-only");
        let action_digest = digest_bytes(key)?;
        self.run(|channels| {
            self.retry(move || {
                let channel = channels.control.clone();
                let request = self.unary_request(reapi::UpdateActionResultRequest {
                    instance_name: self.instance_name.clone(),
                    action_digest: Some(action_digest.clone()),
                    action_result: Some(result.clone()),
                    digest_function: reapi::digest_function::Value::Sha256 as i32,
                    ..Default::default()
                });
                self.deadline("UpdateActionResult", async move {
                    let mut client = reapi::action_cache_client::ActionCacheClient::new(channel)
                        .max_decoding_message_size(MAX_MESSAGE_SIZE)
                        .max_encoding_message_size(MAX_MESSAGE_SIZE);
                    client
                        .update_action_result(request)
                        .await
                        .map_err(|status| rpc_error("UpdateActionResult", status))?;
                    Ok(())
                })
            })
        })
    }

    pub(super) fn upload_files(&self, paths: &[PathBuf]) -> CargoResult<Vec<reapi::Digest>> {
        ensure!(!self.read_only, "remote cache is read-only");
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.run(|channels| async move {
            let mut digests = Vec::with_capacity(paths.len());
            let mut buffer = vec![0; CHUNK_SIZE];
            for path in paths {
                digests.push(hash_file(path, &mut buffer).await?);
            }
            let mut unique = HashMap::default();
            for (digest, path) in digests.iter().zip(paths) {
                unique.entry(digest).or_insert(path);
            }
            let unique: Vec<_> = unique.into_iter().collect();
            let mut missing = Vec::new();
            let mut seen = HashSet::default();
            for batch in unique.chunks(FIND_MISSING_BATCH) {
                let blob_digests: Vec<_> =
                    batch.iter().map(|(digest, _)| (*digest).clone()).collect();
                let response = self
                    .retry(|| {
                        let channel = channels.control.clone();
                        let request = self.unary_request(reapi::FindMissingBlobsRequest {
                            instance_name: self.instance_name.clone(),
                            blob_digests: blob_digests.clone(),
                            digest_function: reapi::digest_function::Value::Sha256 as i32,
                        });
                        self.deadline(
                            "FindMissingBlobs",
                            async move {
                                reapi::content_addressable_storage_client::ContentAddressableStorageClient::new(channel)
                                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                                    .find_missing_blobs(request)
                                    .await
                                    .map_err(|status| rpc_error("FindMissingBlobs", status))
                            },
                        )
                    })
                    .await?
                    .into_inner();
                tracing::debug!(
                    requested = batch.len(),
                    missing = response.missing_blob_digests.len(),
                    "FindMissingBlobs completed"
                );
                let requested: HashMap<_, _> = batch.iter().copied().collect();
                for digest in response.missing_blob_digests {
                    validate_digest(&digest)?;
                    let path = *requested
                        .get(&digest)
                        .context("FindMissingBlobs returned an unrequested digest")?;
                    if seen.insert(digest.clone()) {
                        missing.push((digest, path.as_path()));
                    }
                }
            }
            if missing.is_empty() {
                return Ok(digests);
            }
            let compression = self.compression(&channels).await;
            // Batch and concurrency limits bound memory regardless of artifact count or size.
            stream::iter(plan_transfers(missing))
                .map(|transfer| {
                    let channels = &channels;
                    async move {
                        match transfer {
                            Transfer::Batch(blobs) => {
                                self.retry(|| self.write_batch(channels.bulk(), &blobs, compression))
                                    .await
                            }
                            Transfer::Stream(digest, path) => {
                                tracing::debug!(
                                    digest = %digest.hash,
                                    bytes = digest.size_bytes,
                                    "uploading remote cache blob"
                                );
                                self.retry(|| {
                                    self.upload_file(channels.bulk(), path, &digest, compression)
                                })
                                .await
                                .with_context(|| {
                                    format!(
                                        "uploading remote cache blob ({} bytes)",
                                        digest.size_bytes
                                    )
                                })?;
                                tracing::debug!(
                                    digest = %digest.hash,
                                    bytes = digest.size_bytes,
                                    "uploaded remote cache blob"
                                );
                                Ok(())
                            }
                        }
                    }
                })
                .buffer_unordered(TRANSFER_CONCURRENCY)
                .try_collect::<Vec<()>>()
                .await?;
            Ok(digests)
        })
    }

    /// Download blobs to their destinations after verifying size and SHA256.
    /// Small blobs share BatchReadBlobs requests. Large blobs stream concurrently.
    pub(super) fn download_files(&self, files: Vec<(reapi::Digest, PathBuf)>) -> CargoResult<()> {
        for (digest, _) in &files {
            validate_digest(digest)?;
        }
        if files.is_empty() {
            return Ok(());
        }
        self.run(|channels| async move {
            let compression = self.compression(&channels).await;
            stream::iter(plan_transfers(files))
                .map(|transfer| {
                    let channels = &channels;
                    async move {
                        match transfer {
                            Transfer::Batch(blobs) => {
                                self.retry(|| self.read_batch(channels.bulk(), &blobs, compression))
                                    .await
                            }
                            Transfer::Stream(digest, path) => {
                                self.retry(|| {
                                    self.read_stream(channels.bulk(), &digest, &path, compression)
                                })
                                .await
                            }
                        }
                    }
                })
                .buffer_unordered(TRANSFER_CONCURRENCY)
                .try_collect::<Vec<()>>()
                .await?;
            Ok(())
        })
    }

    /// Ask the server once whether it accepts zstd. Failures fall back to
    /// uncompressed transfers.
    async fn compression(&self, channels: &Channels) -> Compression {
        *self
            .compression
            .get_or_init(|| async {
                let channel = channels.control.clone();
                let request = self.unary_request(reapi::GetCapabilitiesRequest {
                    instance_name: self.instance_name.clone(),
                });
                let result = self
                    .deadline("GetCapabilities", async move {
                        reapi::capabilities_client::CapabilitiesClient::new(channel)
                            .max_decoding_message_size(MAX_MESSAGE_SIZE)
                            .get_capabilities(request)
                            .await
                            .map_err(|status| rpc_error("GetCapabilities", status))
                    })
                    .await;
                let zstd = reapi::compressor::Value::Zstd as i32;
                let compression = match result {
                    Ok(response) => {
                        let cache = response.into_inner().cache_capabilities.unwrap_or_default();
                        Compression {
                            stream: cache.supported_compressors.contains(&zstd),
                            batch_update: cache.supported_batch_update_compressors.contains(&zstd),
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "remote cache capabilities unavailable");
                        Compression::default()
                    }
                };
                tracing::debug!(
                    stream = compression.stream,
                    batch_update = compression.batch_update,
                    "remote cache zstd support"
                );
                compression
            })
            .await
    }

    async fn read_batch(
        &self,
        channel: Channel,
        blobs: &[(reapi::Digest, PathBuf)],
        compression: Compression,
    ) -> CargoResult<()> {
        let bytes: i64 = blobs.iter().map(|(digest, _)| digest.size_bytes).sum();
        tracing::debug!(blobs = blobs.len(), bytes, "downloading remote cache batch");
        let mut client =
            reapi::content_addressable_storage_client::ContentAddressableStorageClient::new(
                channel,
            )
            .max_decoding_message_size(MAX_MESSAGE_SIZE);
        let acceptable_compressors = if compression.stream {
            vec![reapi::compressor::Value::Zstd as i32]
        } else {
            Vec::new()
        };
        let response = self
            .deadline(
                "BatchReadBlobs",
                client
                    .batch_read_blobs(self.unary_request(reapi::BatchReadBlobsRequest {
                        instance_name: self.instance_name.clone(),
                        digests: blobs.iter().map(|(digest, _)| digest.clone()).collect(),
                        acceptable_compressors,
                        digest_function: reapi::digest_function::Value::Sha256 as i32,
                    }))
                    .map_err(|status| rpc_error("BatchReadBlobs", status)),
            )
            .await?
            .into_inner();
        let mut pending: HashMap<_, _> = blobs
            .iter()
            .map(|(digest, path)| (digest.hash.as_str(), (digest, path)))
            .collect();
        ensure!(
            pending.len() == blobs.len(),
            "duplicate remote cache download"
        );
        let mut wire = 0;
        for blob in response.responses {
            let digest = blob
                .digest
                .context("BatchReadBlobs response without digest")?;
            let (expected, path) = pending
                .remove(digest.hash.as_str())
                .context("BatchReadBlobs returned an unrequested blob")?;
            ensure!(
                digest.size_bytes == expected.size_bytes,
                "BatchReadBlobs returned a different blob size"
            );
            check_blob_status("BatchReadBlobs", blob.status)?;
            wire += blob.data.len();
            let data = if blob.compressor == reapi::compressor::Value::Identity as i32 {
                blob.data
            } else if blob.compressor == reapi::compressor::Value::Zstd as i32 && compression.stream
            {
                let capacity = usize::try_from(expected.size_bytes)?;
                zstd::bulk::decompress(&blob.data, capacity)
                    .context("invalid zstd data from BatchReadBlobs")?
            } else {
                bail!("BatchReadBlobs returned an unrequested compressor");
            };
            verify_contents(expected, &data)?;
            tokio::fs::write(path, &data)
                .await
                .context("failed to write remote cache staging file")?;
        }
        ensure!(
            pending.is_empty(),
            "BatchReadBlobs omitted a requested blob"
        );
        tracing::debug!(
            blobs = blobs.len(),
            bytes,
            wire,
            "downloaded remote cache batch"
        );
        Ok(())
    }

    async fn write_batch(
        &self,
        channel: Channel,
        blobs: &[(reapi::Digest, &Path)],
        compression: Compression,
    ) -> CargoResult<()> {
        let mut requests = Vec::with_capacity(blobs.len());
        let mut bytes = 0;
        let mut wire = 0;
        for (digest, path) in blobs {
            let data = tokio::fs::read(path)
                .await
                .context("failed to read remote cache upload")?;
            verify_contents(digest, &data).context("remote cache upload changed contents")?;
            bytes += digest.size_bytes;
            let compressed = compression
                .batch_update
                .then(|| zstd::bulk::compress(&data, ZSTD_LEVEL))
                .transpose()
                .context("failed to compress remote cache upload")?
                .filter(|compressed| compressed.len() < data.len());
            let (data, compressor) = match compressed {
                Some(compressed) => (compressed, reapi::compressor::Value::Zstd),
                None => (data, reapi::compressor::Value::Identity),
            };
            wire += data.len();
            requests.push(reapi::batch_update_blobs_request::Request {
                digest: Some(digest.clone()),
                data,
                compressor: compressor as i32,
            });
        }
        tracing::debug!(
            blobs = blobs.len(),
            bytes,
            wire,
            "uploading remote cache batch"
        );
        let mut client =
            reapi::content_addressable_storage_client::ContentAddressableStorageClient::new(
                channel,
            )
            .max_decoding_message_size(MAX_MESSAGE_SIZE);
        let response = self
            .deadline(
                "BatchUpdateBlobs",
                client
                    .batch_update_blobs(self.unary_request(reapi::BatchUpdateBlobsRequest {
                        instance_name: self.instance_name.clone(),
                        requests,
                        digest_function: reapi::digest_function::Value::Sha256 as i32,
                    }))
                    .map_err(|status| rpc_error("BatchUpdateBlobs", status)),
            )
            .await?
            .into_inner();
        let mut pending: HashSet<_> = blobs
            .iter()
            .map(|(digest, _)| digest.hash.as_str())
            .collect();
        for blob in response.responses {
            let digest = blob
                .digest
                .context("BatchUpdateBlobs response without digest")?;
            ensure!(
                pending.remove(digest.hash.as_str()),
                "BatchUpdateBlobs returned an unrequested blob"
            );
            check_blob_status("BatchUpdateBlobs", blob.status)?;
        }
        ensure!(
            pending.is_empty(),
            "BatchUpdateBlobs omitted a requested blob"
        );
        tracing::debug!(blobs = blobs.len(), bytes, "uploaded remote cache batch");
        Ok(())
    }

    async fn read_stream(
        &self,
        channel: Channel,
        digest: &reapi::Digest,
        destination: &Path,
        compression: Compression,
    ) -> CargoResult<()> {
        tracing::debug!(
            digest = %digest.hash,
            bytes = digest.size_bytes,
            compressed = compression.stream,
            "downloading remote cache blob"
        );
        async {
            let mut client = bytestream::byte_stream_client::ByteStreamClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_SIZE);
            let kind = if compression.stream {
                "compressed-blobs/zstd"
            } else {
                "blobs"
            };
            let mut response = self
                .deadline(
                    "ByteStream.Read",
                    client
                        .read(self.request(bytestream::ReadRequest {
                            resource_name: self.resource_name(&format!(
                                "{kind}/{}/{}",
                                digest.hash, digest.size_bytes
                            )),
                            read_offset: 0,
                            // Read through EOF to reject a valid prefix of a longer blob.
                            read_limit: 0,
                        }))
                        .map_err(|status| rpc_error("ByteStream.Read", status)),
                )
                .await?
                .into_inner();
            let file = std::fs::File::create(destination)
                .context("failed to create remote cache staging file")?;
            let writer = VerifyingWriter {
                file: std::io::BufWriter::new(file),
                hasher: Sha256::new(),
                size: 0,
                limit: digest.size_bytes,
            };
            let mut sink = if compression.stream {
                Sink::Zstd(Box::new(
                    zstd::stream::write::Decoder::new(writer)
                        .context("failed to start zstd decoder")?,
                ))
            } else {
                Sink::Identity(writer)
            };
            while let Some(message) = self
                .deadline(
                    "ByteStream.Read",
                    response
                        .message()
                        .map_err(|status| rpc_error("ByteStream.Read", status)),
                )
                .await?
            {
                sink.write_all(&message.data)
                    .context("failed to write remote cache staging file")?;
            }
            let mut writer = sink.finish()?;
            ensure!(
                writer.size == digest.size_bytes,
                "remote cache blob is truncated"
            );
            ensure!(
                writer.hasher.finish_hex() == digest.hash,
                "remote cache blob SHA256 mismatch"
            );
            writer
                .file
                .flush()
                .context("failed to flush remote cache staging file")?;
            CargoResult::Ok(())
        }
        .await
        .with_context(|| {
            format!(
                "downloading remote cache blob ({} bytes)",
                digest.size_bytes
            )
        })?;
        tracing::debug!(
            digest = %digest.hash,
            bytes = digest.size_bytes,
            "downloaded remote cache blob"
        );
        Ok(())
    }

    async fn upload_file(
        &self,
        channel: Channel,
        path: &Path,
        digest: &reapi::Digest,
        compression: Compression,
    ) -> CargoResult<()> {
        validate_digest(digest)?;
        let mut file = tokio::fs::File::open(path)
            .await
            .context("failed to open remote cache upload")?;
        // REAPI upload IDs are UUIDs; no additional crate is needed for a random v4 UUID.
        let mut id: [u8; 16] = rand::random();
        id[6] = (id[6] & 0x0f) | 0x40;
        id[8] = (id[8] & 0x3f) | 0x80;
        let id = u128::from_be_bytes(id);
        let kind = if compression.stream {
            "compressed-blobs/zstd"
        } else {
            "blobs"
        };
        let resource_name = self.resource_name(&format!(
            "uploads/{:08x}-{:04x}-{:04x}-{:04x}-{:012x}/{kind}/{}/{}",
            id >> 96,
            (id >> 80) & 0xffff,
            (id >> 64) & 0xffff,
            (id >> 48) & 0xffff,
            id & 0xffffffffffff,
            digest.hash,
            digest.size_bytes,
        ));
        let mut encoder = compression
            .stream
            .then(|| zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL))
            .transpose()
            .context("failed to start zstd encoder")?;
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let messages = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|message| (message, receiver))
        });
        // Compressed uploads report offsets in compressed bytes after the first message.
        let sent_bytes = Arc::new(AtomicUsize::new(0));
        let producer_sent = Arc::clone(&sent_bytes);
        let producer = async move {
            let mut hasher = Sha256::new();
            let mut size = 0_i64;
            let mut offset = 0_i64;
            let mut name = resource_name;
            loop {
                let mut chunk = vec![0; CHUNK_SIZE];
                let count = file
                    .read(&mut chunk)
                    .await
                    .context("failed to read remote cache upload")?;
                chunk.truncate(count);
                size = checked_size(size, count)?;
                ensure!(
                    size <= digest.size_bytes,
                    "remote cache upload changed size"
                );
                hasher.update(&chunk);
                let finish_write = count == 0;
                if finish_write {
                    ensure!(
                        size == digest.size_bytes,
                        "remote cache upload was truncated"
                    );
                    ensure!(
                        hasher.finish_hex() == digest.hash,
                        "remote cache upload changed contents"
                    );
                }
                let data = match encoder.as_mut() {
                    None => chunk,
                    Some(encoder) if finish_write => std::mem::replace(
                        encoder,
                        zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL)?,
                    )
                    .finish()
                    .context("failed to compress remote cache upload")?,
                    Some(encoder) => {
                        encoder
                            .write_all(&chunk)
                            .context("failed to compress remote cache upload")?;
                        std::mem::take(encoder.get_mut())
                    }
                };
                // The compressor buffers input. Skip empty messages before the last.
                if data.is_empty() && !finish_write {
                    continue;
                }
                let len = data.len();
                let sent = self
                    .deadline("ByteStream.Write", async {
                        Ok(sender
                            .send(bytestream::WriteRequest {
                                resource_name: std::mem::take(&mut name),
                                write_offset: offset,
                                finish_write,
                                data,
                            })
                            .await
                            .is_ok())
                    })
                    .await?;
                if !sent {
                    // The server can finish early if another writer stored the blob.
                    return CargoResult::Ok(());
                }
                offset = checked_size(offset, len)?;
                producer_sent.fetch_add(len, Ordering::Relaxed);
                if finish_write {
                    return CargoResult::Ok(());
                }
            }
        };
        let mut client = bytestream::byte_stream_client::ByteStreamClient::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_SIZE);
        // Keep the file reader in this future: read errors propagate and cancellation
        // drops both halves, without detached tasks continuing after a deadline.
        let upload = client
            .write(self.request(messages))
            .map_err(|status| rpc_error("ByteStream.Write", status));
        let response =
            match futures::future::select(std::pin::pin!(upload), std::pin::pin!(producer)).await {
                futures::future::Either::Left((response, _)) => response?,
                futures::future::Either::Right((produced, upload)) => {
                    produced?;
                    self.deadline("ByteStream.Write (commit)", upload).await?
                }
            };
        let committed = response.into_inner().committed_size;
        if compression.stream {
            // Compressed uploads report -1 when another writer completed the blob.
            let sent = i64::try_from(sent_bytes.load(Ordering::Relaxed))?;
            ensure!(
                committed == -1 || committed == sent || committed == digest.size_bytes,
                "remote cache upload committed size mismatch"
            );
        } else {
            ensure!(
                committed == digest.size_bytes,
                "remote cache upload committed size mismatch"
            );
        }
        Ok(())
    }

    fn resource_name(&self, suffix: &str) -> String {
        if self.instance_name.is_empty() {
            suffix.to_owned()
        } else {
            format!("{}/{suffix}", self.instance_name)
        }
    }

    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        if let Some(key) = &self.api_key {
            request.metadata_mut().insert(API_KEY_HEADER, key.clone());
        }
        request
    }

    fn unary_request<T>(&self, message: T) -> Request<T> {
        let mut request = self.request(message);
        request.set_timeout(self.timeout);
        request
    }

    async fn deadline<T>(
        &self,
        operation: &str,
        future: impl Future<Output = CargoResult<T>>,
    ) -> CargoResult<T> {
        tokio::time::timeout(self.timeout, future)
            .await
            .map_err(|_| {
                anyhow::Error::new(Transient(format!(
                    "remote cache {operation} timed out after {}s",
                    self.timeout.as_secs()
                )))
            })?
    }

    /// Retry transient failures with backoff. Integrity failures are not retried.
    async fn retry<T, F, Fut>(&self, mut attempt: F) -> CargoResult<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = CargoResult<T>>,
    {
        let mut delay = RETRY_DELAY;
        for _ in 1..ATTEMPTS {
            match attempt().await {
                Err(error) if error.downcast_ref::<Transient>().is_some() => {
                    tracing::debug!(%error, "retrying remote cache operation");
                    tokio::time::sleep(delay).await;
                    delay *= 4;
                }
                result => return result,
            }
        }
        attempt().await
    }

    fn run<T, F>(&self, operation: impl FnOnce(Channels) -> F) -> CargoResult<T>
    where
        F: Future<Output = CargoResult<T>>,
    {
        let transport = {
            let mut slot = self.transport.lock();
            if slot.is_none() {
                let mut endpoint = self.endpoint.clone();
                if self.tls {
                    endpoint = endpoint
                        .tls_config(ClientTlsConfig::new().with_native_roots())
                        .map_err(|_| anyhow!("failed to configure remote cache TLS"))?;
                }
                let runtime = Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("cargo-remote-cache")
                    .enable_all()
                    .build()
                    .context("failed to start remote cache runtime")?;
                // Each lazily connected channel owns its own HTTP/2 connection.
                let channels = {
                    let _entered = runtime.enter();
                    Channels {
                        control: endpoint.connect_lazy(),
                        bulk: (0..BULK_CONNECTIONS)
                            .map(|_| endpoint.connect_lazy())
                            .collect(),
                        next: Arc::default(),
                    }
                };
                *slot = Some(Arc::new(Transport {
                    runtime: Some(runtime),
                    channels,
                }));
            }
            Arc::clone(slot.as_ref().unwrap())
        };
        // Worker threads keep HTTP/2 IO alive between synchronous Cargo operations.
        // Unlike Runtime::block_on, this also permits callers inside a Tokio context.
        let _entered = transport.runtime.as_ref().unwrap().enter();
        futures::executor::block_on(operation(transport.channels.clone()))
    }
}

/// Writes downloaded bytes while checking their size and SHA256.
struct VerifyingWriter {
    file: std::io::BufWriter<std::fs::File>,
    hasher: Sha256,
    size: i64,
    limit: i64,
}

impl std::io::Write for VerifyingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let size = i64::try_from(data.len())
            .ok()
            .and_then(|len| self.size.checked_add(len))
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "remote cache blob exceeds its digest size",
                )
            })?;
        self.file.write_all(data)?;
        self.hasher.update(data);
        self.size = size;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

enum Sink {
    Identity(VerifyingWriter),
    Zstd(Box<zstd::stream::write::Decoder<'static, VerifyingWriter>>),
}

impl Sink {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Identity(writer) => writer.write_all(data),
            Sink::Zstd(decoder) => decoder.write_all(data),
        }
    }

    fn finish(self) -> CargoResult<VerifyingWriter> {
        match self {
            Sink::Identity(writer) => Ok(writer),
            Sink::Zstd(mut decoder) => {
                decoder
                    .flush()
                    .context("invalid zstd data from ByteStream.Read")?;
                Ok(decoder.into_inner())
            }
        }
    }
}

fn endpoint_url(value: &str) -> CargoResult<String> {
    let normalized = if let Some(rest) = value.strip_prefix("grpc://") {
        format!("http://{rest}")
    } else if let Some(rest) = value.strip_prefix("grpcs://") {
        format!("https://{rest}")
    } else {
        value.to_owned()
    };
    ensure!(
        normalized.starts_with("http://") || normalized.starts_with("https://"),
        "cache.remote.url must use grpc(s):// or http(s)://"
    );
    // Reject userinfo even when empty, and reject URL parser normalization of whitespace.
    let authority = normalized.split("://").nth(1).unwrap_or("");
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    ensure!(
        !authority.contains('@') && !normalized.bytes().any(|byte| byte.is_ascii_whitespace()),
        "cache.remote.url must not contain credentials or whitespace"
    );
    let url = url::Url::parse(&normalized).map_err(|_| anyhow!("invalid cache.remote.url"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "cache.remote.url must be a grpc(s) or http(s) origin without credentials, path, query, or fragment"
    );
    Ok(url.into())
}

fn validate_instance(instance: &str) -> CargoResult<()> {
    ensure!(
        instance.is_empty()
            || instance.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                    })
            }),
        "cache.remote.instance-name must contain nonempty URL-safe path components"
    );
    Ok(())
}

fn parse_api_key(value: &str) -> CargoResult<MetadataValue<Ascii>> {
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)),
        "remote cache API key must be nonempty ASCII gRPC metadata"
    );
    let mut key = MetadataValue::try_from(value)
        .map_err(|_| anyhow!("remote cache API key is not valid ASCII gRPC metadata"))?;
    key.set_sensitive(true);
    Ok(key)
}

fn validate_digest(digest: &reapi::Digest) -> CargoResult<()> {
    ensure!(
        digest.size_bytes >= 0
            && digest.hash.len() == 64
            && digest
                .hash
                .bytes()
                .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) }),
        "invalid remote cache SHA256 digest"
    );
    Ok(())
}

/// Verify inlined or batched contents against their REAPI digest.
pub(super) fn verify_contents(digest: &reapi::Digest, contents: &[u8]) -> CargoResult<()> {
    validate_digest(digest)?;
    ensure!(
        i64::try_from(contents.len()).ok() == Some(digest.size_bytes),
        "remote cache contents size mismatch"
    );
    ensure!(
        Sha256::new().update(contents).finish_hex() == digest.hash,
        "remote cache contents SHA256 mismatch"
    );
    Ok(())
}

enum Transfer<T> {
    Batch(Vec<(reapi::Digest, T)>),
    Stream(reapi::Digest, T),
}

/// Pack small blobs into batches below the gRPC message limit. Large blobs stream alone.
fn plan_transfers<T>(blobs: Vec<(reapi::Digest, T)>) -> Vec<Transfer<T>> {
    let mut transfers = Vec::new();
    let mut batch = Vec::new();
    let mut batch_size = 0;
    for (digest, item) in blobs {
        if digest.size_bytes > BATCH_BLOB_LIMIT {
            transfers.push(Transfer::Stream(digest, item));
            continue;
        }
        let size = digest.size_bytes + BATCH_BLOB_OVERHEAD;
        if !batch.is_empty() && batch_size + size > BATCH_TOTAL_LIMIT {
            transfers.push(Transfer::Batch(std::mem::take(&mut batch)));
            batch_size = 0;
        }
        batch_size += size;
        batch.push((digest, item));
    }
    if !batch.is_empty() {
        transfers.push(Transfer::Batch(batch));
    }
    transfers
}

fn check_blob_status(
    operation: &str,
    status: Option<bazel_remote_apis::google::rpc::Status>,
) -> CargoResult<()> {
    let code = Code::from(status.map_or(0, |status| status.code));
    // Like `rpc_error`, omit the server-provided message.
    ensure!(
        code == Code::Ok,
        "remote cache {operation} failed for a blob ({code:?})"
    );
    Ok(())
}

fn digest_bytes(bytes: &[u8]) -> CargoResult<reapi::Digest> {
    Ok(reapi::Digest {
        hash: Sha256::new().update(bytes).finish_hex(),
        size_bytes: i64::try_from(bytes.len()).context("remote cache action key is too large")?,
    })
}

async fn hash_file(path: &Path, buffer: &mut [u8]) -> CargoResult<reapi::Digest> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("failed to open remote cache blob for hashing")?;
    ensure!(
        file.metadata().await?.is_file(),
        "remote cache blob is not a regular file"
    );
    let mut hasher = Sha256::new();
    let mut size = 0_i64;
    loop {
        let count = file
            .read(buffer)
            .await
            .context("failed to read remote cache blob for hashing")?;
        if count == 0 {
            break;
        }
        size = checked_size(size, count)?;
        hasher.update(&buffer[..count]);
    }
    Ok(reapi::Digest {
        hash: hasher.finish_hex(),
        size_bytes: size,
    })
}

fn checked_size(size: i64, count: usize) -> CargoResult<i64> {
    size.checked_add(i64::try_from(count).context("remote cache chunk is too large")?)
        .context("remote cache blob is too large")
}

fn rpc_error(operation: &str, status: Status) -> anyhow::Error {
    // A server may echo request headers in its status message or metadata.
    let message = format!("remote cache {operation} failed ({:?})", status.code());
    match status.code() {
        Code::Unavailable
        | Code::DeadlineExceeded
        | Code::Cancelled
        | Code::ResourceExhausted
        | Code::Aborted
        | Code::Internal
        | Code::Unknown => Transient(message).into(),
        _ => anyhow!(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_plans_respect_batch_limits() {
        let digest = |size| reapi::Digest {
            hash: "0".repeat(64),
            size_bytes: size,
        };
        let sizes = [
            0,
            1,
            BATCH_BLOB_LIMIT,
            BATCH_BLOB_LIMIT + 1,
            BATCH_BLOB_LIMIT,
            BATCH_BLOB_LIMIT,
            BATCH_BLOB_LIMIT - BATCH_BLOB_OVERHEAD,
            10 * BATCH_BLOB_LIMIT,
        ];
        let blobs = sizes
            .iter()
            .enumerate()
            .map(|(i, size)| (digest(*size), i))
            .collect();
        let mut planned = Vec::new();
        for transfer in plan_transfers(blobs) {
            match transfer {
                Transfer::Batch(batch) => {
                    assert!(!batch.is_empty());
                    let total: i64 = batch
                        .iter()
                        .map(|(digest, _)| digest.size_bytes + BATCH_BLOB_OVERHEAD)
                        .sum();
                    assert!(total <= BATCH_TOTAL_LIMIT, "{total}");
                    for (digest, i) in batch {
                        assert!(digest.size_bytes <= BATCH_BLOB_LIMIT);
                        planned.push(i);
                    }
                }
                Transfer::Stream(digest, i) => {
                    assert!(digest.size_bytes > BATCH_BLOB_LIMIT);
                    planned.push(i);
                }
            }
        }
        planned.sort_unstable();
        assert_eq!(planned, (0..sizes.len()).collect::<Vec<_>>());
    }

    #[test]
    fn endpoints_accept_origins_and_reject_secret_bearing_urls() {
        assert_eq!(
            endpoint_url("grpc://localhost:1985").unwrap(),
            "http://localhost:1985/"
        );
        assert_eq!(
            endpoint_url("grpcs://cache.example").unwrap(),
            "https://cache.example/"
        );
        assert_eq!(
            endpoint_url("https://cache.example/").unwrap(),
            "https://cache.example/"
        );
        for value in [
            "grpc://user:secret@cache.example",
            "http://@cache.example",
            "https://cache.example/?key=secret",
            "https://cache.example/#secret",
            "https://cache.example/secret",
            "https://cache.example\n",
            "file:///secret",
        ] {
            let error = endpoint_url(value).unwrap_err();
            assert!(!format!("{error:#}").contains("secret"));
        }
    }

    #[test]
    fn digests_require_nonnegative_sizes_and_canonical_sha256() {
        let valid = digest_bytes(b"blob").unwrap();
        validate_digest(&valid).unwrap();
        for digest in [
            reapi::Digest {
                hash: valid.hash.clone(),
                size_bytes: -1,
            },
            reapi::Digest {
                hash: valid.hash.to_uppercase(),
                size_bytes: 4,
            },
            reapi::Digest {
                hash: "../blob".into(),
                size_bytes: 4,
            },
            reapi::Digest {
                hash: "a".repeat(63),
                size_bytes: 4,
            },
        ] {
            assert!(validate_digest(&digest).is_err());
        }
    }

    #[test]
    fn credentials_are_sensitive_and_errors_do_not_echo_values() {
        assert!(parse_api_key("valid-key").unwrap().is_sensitive());
        for value in [
            "",
            "secret\nkey",
            "secret\rkey",
            "secret\0key",
            "secret\u{80}",
        ] {
            let error = parse_api_key(value).unwrap_err();
            assert!(!format!("{error:#}").contains("secret"));
        }
        let error = rpc_error("Read", Status::unauthenticated("secret-key"));
        assert!(!format!("{error:#}").contains("secret-key"));
    }

    #[test]
    fn instance_names_do_not_escape_resource_prefixes() {
        for instance in ["", "cargo", "organization/cargo-cache"] {
            validate_instance(instance).unwrap();
        }
        for instance in [
            "/cargo",
            "cargo/",
            "cargo//x",
            "../cargo",
            "cargo?secret",
            "cargo\n",
        ] {
            assert!(validate_instance(instance).is_err());
        }
    }
}
