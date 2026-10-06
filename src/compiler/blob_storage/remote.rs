use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, ensure};
use bazel_remote_apis::build::bazel::remote::execution::v2 as reapi;
use bazel_remote_apis::google::bytestream;
use cargo_util::Sha256;
use futures::{TryFutureExt, stream};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
}

struct Transport {
    runtime: Option<Runtime>,
    channel: Channel,
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Cargo can also be embedded in a process already running Tokio.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

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
        let endpoint = Endpoint::from_shared(url)
            .map_err(|_| anyhow!("invalid cache.remote.url endpoint"))?
            .connect_timeout(timeout);
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
        }))
    }

    pub(super) fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub(super) fn get_action(&self, key: &[u8]) -> CargoResult<Option<reapi::ActionResult>> {
        let action_digest = digest_bytes(key)?;
        self.run(|channel| {
            self.deadline("GetActionResult", async move {
                let mut client = reapi::action_cache_client::ActionCacheClient::new(channel)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE);
                let request = self.unary_request(reapi::GetActionResultRequest {
                    instance_name: self.instance_name.clone(),
                    action_digest: Some(action_digest),
                    digest_function: reapi::digest_function::Value::Sha256 as i32,
                    ..Default::default()
                });
                match client.get_action_result(request).await {
                    Ok(response) => Ok(Some(response.into_inner())),
                    Err(status) if status.code() == Code::NotFound => Ok(None),
                    Err(status) => Err(rpc_error("GetActionResult", status)),
                }
            })
        })
    }

    pub(super) fn update_action(&self, key: &[u8], result: reapi::ActionResult) -> CargoResult<()> {
        ensure!(!self.read_only, "remote cache is read-only");
        let action_digest = digest_bytes(key)?;
        self.run(|channel| {
            self.deadline("UpdateActionResult", async move {
                let mut client = reapi::action_cache_client::ActionCacheClient::new(channel)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE);
                client
                    .update_action_result(self.unary_request(reapi::UpdateActionResultRequest {
                        instance_name: self.instance_name.clone(),
                        action_digest: Some(action_digest),
                        action_result: Some(result),
                        digest_function: reapi::digest_function::Value::Sha256 as i32,
                        ..Default::default()
                    }))
                    .await
                    .map_err(|status| rpc_error("UpdateActionResult", status))?;
                Ok(())
            })
        })
    }

    pub(super) fn upload_files(&self, paths: &[PathBuf]) -> CargoResult<Vec<reapi::Digest>> {
        ensure!(!self.read_only, "remote cache is read-only");
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.run(|channel| async move {
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
            let mut client =
                reapi::content_addressable_storage_client::ContentAddressableStorageClient::new(
                    channel.clone(),
                )
                .max_decoding_message_size(MAX_MESSAGE_SIZE);
            for batch in unique.chunks(FIND_MISSING_BATCH) {
                let response = self
                    .deadline(
                        "FindMissingBlobs",
                        client
                            .find_missing_blobs(self.unary_request(
                                reapi::FindMissingBlobsRequest {
                                    instance_name: self.instance_name.clone(),
                                    blob_digests:
                                        batch.iter().map(|(digest, _)| (*digest).clone()).collect(),
                                    digest_function: reapi::digest_function::Value::Sha256 as i32,
                                },
                            ))
                            .map_err(|status| rpc_error("FindMissingBlobs", status)),
                    )
                    .await?
                    .into_inner();
                tracing::debug!(
                    requested = batch.len(),
                    missing = response.missing_blob_digests.len(),
                    "FindMissingBlobs completed"
                );
                let requested: HashMap<_, _> = batch.iter().copied().collect();
                let mut seen = HashSet::default();
                for digest in &response.missing_blob_digests {
                    validate_digest(digest)?;
                    let path = requested
                        .get(digest)
                        .context("FindMissingBlobs returned an unrequested digest")?;
                    if seen.insert(digest) {
                        tracing::debug!(
                            digest = %digest.hash,
                            bytes = digest.size_bytes,
                            "uploading remote cache blob"
                        );
                        // Sequential uploads bound memory regardless of artifact count or size.
                        self.upload_file(channel.clone(), path, digest)
                            .await
                            .with_context(|| {
                                format!("uploading remote cache blob ({} bytes)", digest.size_bytes)
                            })?;
                        tracing::debug!(
                            digest = %digest.hash,
                            bytes = digest.size_bytes,
                            "uploaded remote cache blob"
                        );
                    }
                }
            }
            Ok(digests)
        })
    }

    pub(super) fn download_file(
        &self,
        digest: &reapi::Digest,
        destination: &Path,
    ) -> CargoResult<()> {
        validate_digest(digest)?;
        tracing::debug!(
            digest = %digest.hash,
            bytes = digest.size_bytes,
            "downloading remote cache blob"
        );
        self.run(|channel| async move {
            let mut client = bytestream::byte_stream_client::ByteStreamClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_SIZE);
            let mut response = self
                .deadline(
                    "ByteStream.Read",
                    client
                        .read(self.request(bytestream::ReadRequest {
                            resource_name: self.resource_name(&format!(
                                "blobs/{}/{}",
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
            let mut file = tokio::fs::File::create(destination)
                .await
                .context("failed to create remote cache staging file")?;
            let mut hasher = Sha256::new();
            let mut size = 0_i64;
            while let Some(message) = self
                .deadline(
                    "ByteStream.Read",
                    response
                        .message()
                        .map_err(|status| rpc_error("ByteStream.Read", status)),
                )
                .await?
            {
                size = checked_size(size, message.data.len())?;
                ensure!(
                    size <= digest.size_bytes,
                    "remote cache blob exceeds its digest size"
                );
                hasher.update(&message.data);
                file.write_all(&message.data)
                    .await
                    .context("failed to write remote cache staging file")?;
            }
            ensure!(size == digest.size_bytes, "remote cache blob is truncated");
            ensure!(
                hasher.finish_hex() == digest.hash,
                "remote cache blob SHA256 mismatch"
            );
            file.flush()
                .await
                .context("failed to flush remote cache staging file")?;
            tracing::debug!(
                digest = %digest.hash,
                bytes = digest.size_bytes,
                "downloaded remote cache blob"
            );
            Ok(())
        })
        .with_context(|| {
            format!(
                "downloading remote cache blob ({} bytes)",
                digest.size_bytes
            )
        })
    }

    async fn upload_file(
        &self,
        channel: Channel,
        path: &Path,
        digest: &reapi::Digest,
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
        let resource_name = self.resource_name(&format!(
            "uploads/{:08x}-{:04x}-{:04x}-{:04x}-{:012x}/blobs/{}/{}",
            id >> 96,
            (id >> 80) & 0xffff,
            (id >> 64) & 0xffff,
            (id >> 48) & 0xffff,
            id & 0xffffffffffff,
            digest.hash,
            digest.size_bytes,
        ));
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let messages = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|message| (message, receiver))
        });
        let producer = async move {
            let mut hasher = Sha256::new();
            let mut offset = 0_i64;
            let mut name = resource_name;
            loop {
                let mut data = vec![0; CHUNK_SIZE];
                let count = file
                    .read(&mut data)
                    .await
                    .context("failed to read remote cache upload")?;
                data.truncate(count);
                let next_offset = checked_size(offset, count)?;
                ensure!(
                    next_offset <= digest.size_bytes,
                    "remote cache upload changed size"
                );
                hasher.update(&data);
                let finish_write = count == 0;
                if finish_write {
                    ensure!(
                        offset == digest.size_bytes,
                        "remote cache upload was truncated"
                    );
                    ensure!(
                        hasher.finish_hex() == digest.hash,
                        "remote cache upload changed contents"
                    );
                }
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
                offset = next_offset;
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
        ensure!(
            response.into_inner().committed_size == digest.size_bytes,
            "remote cache upload committed size mismatch"
        );
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
                anyhow!(
                    "remote cache {operation} timed out after {}s",
                    self.timeout.as_secs()
                )
            })?
    }

    fn run<T, F>(&self, operation: impl FnOnce(Channel) -> F) -> CargoResult<T>
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
                let channel = {
                    let _entered = runtime.enter();
                    endpoint.connect_lazy()
                };
                *slot = Some(Arc::new(Transport {
                    runtime: Some(runtime),
                    channel,
                }));
            }
            Arc::clone(slot.as_ref().unwrap())
        };
        // Worker threads keep HTTP/2 IO alive between synchronous Cargo operations.
        // Unlike Runtime::block_on, this also permits callers inside a Tokio context.
        let _entered = transport.runtime.as_ref().unwrap().enter();
        futures::executor::block_on(operation(transport.channel.clone()))
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
    anyhow!("remote cache {operation} failed ({:?})", status.code())
}

#[cfg(test)]
mod tests {
    use super::*;

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
