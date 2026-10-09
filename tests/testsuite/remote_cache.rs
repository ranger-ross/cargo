use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bazel_remote_apis::build::bazel::remote::execution::v2 as reapi;
use bazel_remote_apis::google::bytestream as bs;
use futures::{Stream, StreamExt, stream};
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};
use tonic::{Request, Response, Status};

use crate::prelude::*;
use cargo_test_support::registry::Package;
use cargo_test_support::{Execs, Project, paths, prelude::*, project, t};

const INSTANCE: &str = "cargo/tests";
const API_KEY: &str = "remote-cache-test-secret";
const API_KEY_ENV: &str = "CARGO_REMOTE_CACHE_TEST_API_KEY";
/// Below the default remote timeout, so a held read that times out still succeeds.
const HOLD_LIMIT: Duration = Duration::from_secs(10);

type BlobKey = (String, i64);
type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Counts {
    requests: usize,
    reads: usize,
    writes: usize,
    batch_reads: usize,
    batch_writes: usize,
    updates: usize,
    early_commits: usize,
    compressed_reads: usize,
    compressed_writes: usize,
}

#[derive(Default)]
struct State {
    actions: BTreeMap<BlobKey, reapi::ActionResult>,
    blobs: BTreeMap<BlobKey, Vec<u8>>,
    counts: Counts,
    failure: Option<tonic::Code>,
    simulate_upload_race: bool,
    ignore_inline: bool,
    corrupt_inline: bool,
    read_keys: Vec<BlobKey>,
    upload_delay: Duration,
    read_delay: Duration,
    /// Hold each ByteStream read until this returns true or `HOLD_LIMIT` passes.
    hold_reads_until: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// For each held read, whether the condition held before the limit.
    held_reads: Vec<bool>,
    /// Fail ByteStream reads with `Unavailable`.
    fail_reads: bool,
    lookup_delay: Duration,
    /// Advertise and accept zstd transfers.
    compression: bool,
    /// Fail this many upcoming requests with `Unavailable`.
    transient_failures: usize,
    /// Lookups currently inside `lookup_delay`, and the most seen at once.
    active_lookups: usize,
    peak_lookups: usize,
}

#[derive(Clone)]
struct CacheService {
    state: Arc<Mutex<State>>,
    api_key: Option<&'static str>,
}

struct CacheServer {
    service: CacheService,
    url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CacheServer {
    fn new(api_key: Option<&'static str>) -> Self {
        let listener = t!(TcpListener::bind("127.0.0.1:0"));
        let url = format!("grpc://{}", t!(listener.local_addr()));
        t!(listener.set_nonblocking(true));
        let service = CacheService {
            state: Arc::new(Mutex::new(State::default())),
            api_key,
        };
        let server_service = service.clone();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let thread = thread::spawn(move || {
            let runtime = t!(tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build());
            runtime.block_on(async move {
                let listener = t!(tokio::net::TcpListener::from_std(listener));
                let incoming = stream::unfold(listener, |listener| async move {
                    let connection = listener.accept().await.map(|(socket, _)| socket);
                    Some((connection, listener))
                });
                t!(tonic::transport::Server::builder()
                    .add_service(reapi::action_cache_server::ActionCacheServer::new(
                        server_service.clone(),
                    ))
                    .add_service(
                        reapi::content_addressable_storage_server::ContentAddressableStorageServer::new(
                            server_service.clone(),
                        ),
                    )
                    .add_service(bs::byte_stream_server::ByteStreamServer::new(server_service.clone()))
                    .add_service(reapi::capabilities_server::CapabilitiesServer::new(
                        server_service,
                    ))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stopped.await;
                    })
                    .await);
            });
        });
        Self {
            service,
            url,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    fn configure(&self, p: &Project, read_only: bool) {
        p.change_file(
            ".cargo/config.toml",
            &format!(
                r#"
                    [cache.remote]
                    url = "{}"
                    instance-name = "{INSTANCE}"
                    api-key-env = "{API_KEY_ENV}"
                    read-only = {read_only}
                    timeout = 5
                "#,
                self.url
            ),
        );
    }

    fn counts(&self) -> Counts {
        self.service.state.lock().counts
    }
}

impl Drop for CacheServer {
    fn drop(&mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn digest_key(digest: &reapi::Digest) -> Result<BlobKey, Status> {
    if digest.size_bytes < 0
        || digest.hash.len() != 64
        || !digest
            .hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Status::invalid_argument("invalid SHA256 digest"));
    }
    Ok((digest.hash.clone(), digest.size_bytes))
}

impl CacheService {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let mut state = self.state.lock();
        state.counts.requests += 1;
        if let Some(code) = state.failure {
            return Err(Status::new(code, "cache intentionally unavailable"));
        }
        if state.transient_failures > 0 {
            state.transient_failures -= 1;
            return Err(Status::unavailable("cache briefly unavailable"));
        }
        if let Some(expected) = self.api_key {
            if request
                .metadata()
                .get("x-buildbuddy-api-key")
                .and_then(|value| value.to_str().ok())
                != Some(expected)
            {
                return Err(Status::unauthenticated("invalid API key"));
            }
        }
        Ok(())
    }

    fn instance(&self, instance: &str, function: i32) -> Result<(), Status> {
        if instance != INSTANCE || function != reapi::digest_function::Value::Sha256 as i32 {
            return Err(Status::invalid_argument(
                "wrong instance or digest function",
            ));
        }
        Ok(())
    }

    /// Parses `blobs/` and `compressed-blobs/zstd/` resources. Returns whether
    /// the transfer is compressed.
    fn resource(&self, resource: &str, upload: bool) -> Result<(BlobKey, bool), Status> {
        let resource = resource
            .strip_prefix(&format!("{INSTANCE}/"))
            .ok_or_else(|| Status::invalid_argument("wrong resource instance"))?;
        let mut parts: Vec<_> = resource.split('/').collect();
        if upload {
            if parts.len() < 2 || parts[0] != "uploads" || parts[1].is_empty() {
                return Err(Status::invalid_argument("invalid upload resource"));
            }
            parts.drain(..2);
        }
        let (compressed, digest) = match parts[..] {
            ["blobs", hash, size] => (false, (hash, size)),
            ["compressed-blobs", "zstd", hash, size] => {
                if !self.state.lock().compression {
                    return Err(Status::invalid_argument("zstd is not advertised"));
                }
                (true, (hash, size))
            }
            _ => return Err(Status::invalid_argument("invalid blob resource")),
        };
        let key = digest_key(&reapi::Digest {
            hash: digest.0.to_owned(),
            size_bytes: digest
                .1
                .parse()
                .map_err(|_| Status::invalid_argument("invalid resource size"))?,
        })?;
        Ok((key, compressed))
    }
}

#[tonic::async_trait]
impl reapi::capabilities_server::Capabilities for CacheService {
    async fn get_capabilities(
        &self,
        request: Request<reapi::GetCapabilitiesRequest>,
    ) -> Result<Response<reapi::ServerCapabilities>, Status> {
        self.authorize(&request)?;
        let compressors = if self.state.lock().compression {
            vec![reapi::compressor::Value::Zstd as i32]
        } else {
            Vec::new()
        };
        Ok(Response::new(reapi::ServerCapabilities {
            cache_capabilities: Some(reapi::CacheCapabilities {
                digest_functions: vec![reapi::digest_function::Value::Sha256 as i32],
                supported_compressors: compressors.clone(),
                supported_batch_update_compressors: compressors,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

#[tonic::async_trait]
impl reapi::action_cache_server::ActionCache for CacheService {
    async fn get_action_result(
        &self,
        request: Request<reapi::GetActionResultRequest>,
    ) -> Result<Response<reapi::ActionResult>, Status> {
        self.authorize(&request)?;
        let delay = {
            let mut state = self.state.lock();
            state.active_lookups += 1;
            state.peak_lookups = state.peak_lookups.max(state.active_lookups);
            state.lookup_delay
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.state.lock().active_lookups -= 1;
        let request = request.into_inner();
        self.instance(&request.instance_name, request.digest_function)?;
        let key = digest_key(
            request
                .action_digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing action digest"))?,
        )?;
        let state = self.state.lock();
        let mut result = state
            .actions
            .get(&key)
            .ok_or_else(|| Status::not_found("action not cached"))?
            .clone();
        for output in &mut result.output_files {
            let Some(blob) = state
                .blobs
                .get(&digest_key(output.digest.as_ref().unwrap())?)
            else {
                return Err(Status::not_found("referenced blob evicted"));
            };
            if !state.ignore_inline && request.inline_output_files.contains(&output.path) {
                output.contents = blob.clone();
                if state.corrupt_inline {
                    output.contents[0] ^= 1;
                }
            }
        }
        Ok(Response::new(result))
    }

    async fn update_action_result(
        &self,
        request: Request<reapi::UpdateActionResultRequest>,
    ) -> Result<Response<reapi::ActionResult>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        self.instance(&request.instance_name, request.digest_function)?;
        let key = digest_key(
            request
                .action_digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing action digest"))?,
        )?;
        let result = request
            .action_result
            .ok_or_else(|| Status::invalid_argument("missing action result"))?;
        if result.exit_code != 0
            || !result.output_directories.is_empty()
            || !result.output_symlinks.is_empty()
        {
            return Err(Status::invalid_argument("unexpected action output"));
        }
        let mut state = self.state.lock();
        let mut paths = BTreeSet::new();
        for output in &result.output_files {
            if output.is_executable
                || !paths.insert(output.path.as_str())
                || !(output.path == "cache-entry"
                    || output.path.strip_prefix("blobs/").is_some_and(|hash| {
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }))
            {
                return Err(Status::invalid_argument("unexpected output file"));
            }
            let digest = output
                .digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing output digest"))?;
            if !state.blobs.contains_key(&digest_key(digest)?) {
                return Err(Status::failed_precondition("upload CAS before action"));
            }
        }
        if !paths.contains("cache-entry") || paths.len() < 2 {
            return Err(Status::invalid_argument("incomplete action graph"));
        }
        state.counts.updates += 1;
        state.actions.insert(key, result.clone());
        Ok(Response::new(result))
    }
}

#[tonic::async_trait]
impl reapi::content_addressable_storage_server::ContentAddressableStorage for CacheService {
    async fn find_missing_blobs(
        &self,
        request: Request<reapi::FindMissingBlobsRequest>,
    ) -> Result<Response<reapi::FindMissingBlobsResponse>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        self.instance(&request.instance_name, request.digest_function)?;
        let state = self.state.lock();
        let mut missing_blob_digests = Vec::new();
        for digest in request.blob_digests {
            if state.simulate_upload_race || !state.blobs.contains_key(&digest_key(&digest)?) {
                missing_blob_digests.push(digest);
            }
        }
        Ok(Response::new(reapi::FindMissingBlobsResponse {
            missing_blob_digests,
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<reapi::BatchUpdateBlobsRequest>,
    ) -> Result<Response<reapi::BatchUpdateBlobsResponse>, Status> {
        self.authorize(&request)?;
        let delay = self.state.lock().upload_delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let request = request.into_inner();
        self.instance(&request.instance_name, request.digest_function)?;
        let mut state = self.state.lock();
        state.counts.batch_writes += 1;
        let mut responses = Vec::new();
        for blob in request.requests {
            let digest = blob
                .digest
                .ok_or_else(|| Status::invalid_argument("missing digest"))?;
            let key = digest_key(&digest)?;
            let data =
                if blob.compressor == reapi::compressor::Value::Zstd as i32 && state.compression {
                    state.counts.compressed_writes += 1;
                    zstd::stream::decode_all(blob.data.as_slice()).ok()
                } else if blob.compressor == 0 {
                    Some(blob.data)
                } else {
                    None
                };
            let code = match data {
                Some(data)
                    if key.1 == data.len() as i64
                        && key.0 == hex::encode(Sha256::digest(&data)) =>
                {
                    state.counts.writes += 1;
                    state.blobs.insert(key, data);
                    tonic::Code::Ok
                }
                _ => tonic::Code::InvalidArgument,
            };
            responses.push(reapi::batch_update_blobs_response::Response {
                digest: Some(digest),
                status: Some(bazel_remote_apis::google::rpc::Status {
                    code: code as i32,
                    ..Default::default()
                }),
            });
        }
        Ok(Response::new(reapi::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<reapi::BatchReadBlobsRequest>,
    ) -> Result<Response<reapi::BatchReadBlobsResponse>, Status> {
        self.authorize(&request)?;
        let delay = self.state.lock().read_delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let request = request.into_inner();
        self.instance(&request.instance_name, request.digest_function)?;
        let mut state = self.state.lock();
        state.counts.batch_reads += 1;
        let compress = state.compression
            && request
                .acceptable_compressors
                .contains(&(reapi::compressor::Value::Zstd as i32));
        let mut responses = Vec::new();
        for digest in request.digests {
            let key = digest_key(&digest)?;
            state.counts.reads += 1;
            state.read_keys.push(key.clone());
            let (data, code) = match state.blobs.get(&key) {
                Some(data) => (data.clone(), tonic::Code::Ok),
                None => (Vec::new(), tonic::Code::NotFound),
            };
            let (data, compressor) = if compress && code == tonic::Code::Ok {
                state.counts.compressed_reads += 1;
                (
                    t!(zstd::bulk::compress(&data, 1)),
                    reapi::compressor::Value::Zstd as i32,
                )
            } else {
                (data, 0)
            };
            responses.push(reapi::batch_read_blobs_response::Response {
                digest: Some(digest),
                data,
                compressor,
                status: Some(bazel_remote_apis::google::rpc::Status {
                    code: code as i32,
                    ..Default::default()
                }),
            });
        }
        Ok(Response::new(reapi::BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = RpcStream<reapi::GetTreeResponse>;

    async fn get_tree(
        &self,
        request: Request<reapi::GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented("fixture stores files, not trees"))
    }

    async fn split_blob(
        &self,
        request: Request<reapi::SplitBlobRequest>,
    ) -> Result<Response<reapi::SplitBlobResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented("chunk mappings are not advertised"))
    }

    type GetChunkMappingStream = RpcStream<reapi::GetChunkMappingResponse>;

    async fn get_chunk_mapping(
        &self,
        request: Request<reapi::GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented("chunk mappings are not advertised"))
    }

    async fn splice_blob(
        &self,
        request: Request<reapi::SpliceBlobRequest>,
    ) -> Result<Response<reapi::SpliceBlobResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented("chunk mappings are not advertised"))
    }

    async fn register_chunk_mapping(
        &self,
        request: Request<tonic::Streaming<reapi::RegisterChunkMappingRequest>>,
    ) -> Result<Response<reapi::RegisterChunkMappingResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented("chunk mappings are not advertised"))
    }
}

#[tonic::async_trait]
impl bs::byte_stream_server::ByteStream for CacheService {
    type ReadStream = RpcStream<bs::ReadResponse>;

    async fn read(
        &self,
        request: Request<bs::ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        self.authorize(&request)?;
        let (hold, fail) = {
            let state = self.state.lock();
            (state.hold_reads_until.clone(), state.fail_reads)
        };
        if fail {
            return Err(Status::unavailable("reads intentionally unavailable"));
        }
        if let Some(ready) = hold {
            let deadline = tokio::time::Instant::now() + HOLD_LIMIT;
            let released = loop {
                if ready() {
                    break true;
                }
                if tokio::time::Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            self.state.lock().held_reads.push(released);
        }
        let request = request.into_inner();
        let (key, compressed) = self.resource(&request.resource_name, false)?;
        let mut state = self.state.lock();
        state.counts.reads += 1;
        state.read_keys.push(key.clone());
        let data = state
            .blobs
            .get(&key)
            .ok_or_else(|| Status::not_found("blob missing"))?
            .clone();
        let data = if compressed {
            if request.read_offset != 0 || request.read_limit != 0 {
                return Err(Status::invalid_argument("compressed reads are whole blobs"));
            }
            state.counts.compressed_reads += 1;
            t!(zstd::bulk::compress(&data, 1))
        } else {
            data
        };
        let offset = usize::try_from(request.read_offset)
            .map_err(|_| Status::out_of_range("negative offset"))?;
        if offset > data.len() || request.read_limit < 0 {
            return Err(Status::out_of_range("invalid read range"));
        }
        let limit = if request.read_limit == 0 {
            data.len() - offset
        } else {
            usize::try_from(request.read_limit)
                .unwrap()
                .min(data.len() - offset)
        };
        // Deliberately use small, uneven chunks: a client must consume the whole stream.
        let chunks: Vec<_> = data[offset..offset + limit]
            .chunks(4093)
            .map(|chunk| {
                Ok(bs::ReadResponse {
                    data: chunk.to_vec(),
                })
            })
            .collect();
        let delay = state.read_delay;
        Ok(Response::new(Box::pin(stream::iter(chunks).then(
            move |chunk| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                chunk
            },
        ))))
    }

    async fn write(
        &self,
        request: Request<tonic::Streaming<bs::WriteRequest>>,
    ) -> Result<Response<bs::WriteResponse>, Status> {
        self.authorize(&request)?;
        let delay = self.state.lock().upload_delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let mut stream = request.into_inner();
        let mut resource = None;
        let mut key = None;
        let mut compressed = false;
        let mut data = Vec::new();
        let mut finished = false;
        while let Some(part) = stream.message().await? {
            if finished || part.write_offset != data.len() as i64 {
                return Err(Status::invalid_argument(
                    "noncontiguous write or data after finish",
                ));
            }
            if resource.is_none() {
                let (parsed, is_compressed) = self.resource(&part.resource_name, true)?;
                let mut state = self.state.lock();
                if state.simulate_upload_race && state.blobs.contains_key(&parsed) {
                    state.counts.early_commits += 1;
                    return Ok(Response::new(bs::WriteResponse {
                        committed_size: if is_compressed { -1 } else { parsed.1 },
                    }));
                }
                key = Some(parsed);
                compressed = is_compressed;
                resource = Some(part.resource_name);
            } else if !part.resource_name.is_empty()
                && resource.as_deref() != Some(part.resource_name.as_str())
            {
                return Err(Status::invalid_argument("resource changed during upload"));
            }
            data.extend_from_slice(&part.data);
            finished = part.finish_write;
        }
        let key = key.ok_or_else(|| Status::invalid_argument("empty upload stream"))?;
        let committed_size = data.len() as i64;
        if compressed {
            data = zstd::stream::decode_all(data.as_slice())
                .map_err(|_| Status::invalid_argument("invalid zstd upload"))?;
        }
        if !finished || key.1 != data.len() as i64 || key.0 != hex::encode(Sha256::digest(&data)) {
            return Err(Status::invalid_argument(
                "unfinished upload or digest mismatch",
            ));
        }
        let mut state = self.state.lock();
        state.counts.writes += 1;
        if compressed {
            state.counts.compressed_writes += 1;
        }
        state.blobs.insert(key, data);
        Ok(Response::new(bs::WriteResponse { committed_size }))
    }

    async fn query_write_status(
        &self,
        request: Request<bs::QueryWriteStatusRequest>,
    ) -> Result<Response<bs::QueryWriteStatusResponse>, Status> {
        self.authorize(&request)?;
        let (key, _) = self.resource(&request.into_inner().resource_name, true)?;
        let state = self.state.lock();
        let data = state
            .blobs
            .get(&key)
            .ok_or_else(|| Status::not_found("upload missing"))?;
        Ok(Response::new(bs::QueryWriteStatusResponse {
            committed_size: data.len() as i64,
            complete: true,
        }))
    }
}

fn cache_project() -> Project {
    Package::new("common", "0.1.0")
        // Large enough that the rlib streams while smaller outputs are batched.
        .file("payload", &"x".repeat(1536 * 1024))
        .file(
            "src/lib.rs",
            "pub fn data() -> &'static [u8] { include_bytes!(\"../payload\") }",
        )
        .publish();
    Package::new("variant", "0.1.0")
        .feature("foo", &[])
        .feature("bar", &[])
        .file(
            "src/lib.rs",
            r#"
                #[cfg(feature = "foo")]
                pub fn value() -> u32 { 1 }
                #[cfg(feature = "bar")]
                pub fn value() -> u32 { 2 }
                pub fn label() -> &'static str { env!("REMOTE_CACHE_TEST_VALUE") }
            "#,
        )
        .publish();
    project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2021"
                [dependencies]
                common = "0.1.0"
                variant = "0.1.0"
                [features]
                foo = ["variant/foo"]
                bar = ["variant/bar"]
            "#,
        )
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    let sum: u64 = common::data().iter().map(|b| u64::from(*b)).sum();
                    println!("{} {} {}", sum, variant::value(), variant::label());
                }
            "#,
        )
        .build()
}

fn cargo(p: &Project, mode: &str, feature: &str) -> Execs {
    let mut command = p.cargo(&format!("{mode} -vv -Zshared-blob-storage"));
    command
        .args(&["--features", feature])
        .env(API_KEY_ENV, API_KEY)
        .env("REMOTE_CACHE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"]);
    command
}

fn purge_local_cache(p: &Project) {
    for path in [p.build_dir(), paths::cargo_home().join("shared-storage")] {
        if path.exists() {
            t!(fs::remove_dir_all(path));
        }
    }
}

fn compiled(stderr: &[u8], name: &str) -> bool {
    std::str::from_utf8(stderr)
        .unwrap()
        .lines()
        .any(|line| line.contains("Running") && line.contains(&format!("--crate-name {name} ")))
}

fn assert_compiled(stderr: &[u8], name: &str, expected: bool) {
    assert_eq!(
        compiled(stderr, name),
        expected,
        "{}",
        String::from_utf8_lossy(stderr)
    );
}

fn assert_warning(stderr: &[u8]) {
    let stderr = std::str::from_utf8(stderr).unwrap();
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("warning:") && line.contains("remote cache")),
        "{stderr}"
    );
    assert!(!stderr.contains(API_KEY), "API key leaked into diagnostics");
}

fn assert_app(p: &Project, value: u32, label: &str) {
    p.process(&p.bin("app"))
        .with_stdout_data(format!("188743680 {value} {label}\n"))
        .run();
}

fn restores_from_remote(mode: &str) {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    let first = cargo(&p, mode, "foo").run();
    for name in ["common", "variant", "app"] {
        assert_compiled(&first.stderr, name, true);
    }
    let uploaded = server.counts();
    assert!(uploaded.writes > 0 && uploaded.batch_writes > 0 && uploaded.updates >= 2);
    purge_local_cache(&p);
    let restored = cargo(&p, mode, "foo").run();
    for name in ["common", "variant"] {
        assert_compiled(&restored.stderr, name, false);
    }
    assert_compiled(&restored.stderr, "app", true);
    let after_restore = server.counts();
    assert!(after_restore.reads > uploaded.reads);
    // Each restored unit batches its small outputs, once for metadata and
    // once for deferred outputs.
    let batches = after_restore.batch_reads - uploaded.batch_reads;
    assert!((1..=4).contains(&batches), "{batches}");
    assert_eq!(after_restore.writes, uploaded.writes);
    assert_eq!(after_restore.updates, uploaded.updates);
    if mode == "build" {
        assert_app(&p, 1, "original");
    }
    let before_fresh = server.counts();
    let fresh = cargo(&p, mode, "foo").run();
    for name in ["common", "variant", "app"] {
        assert_compiled(&fresh.stderr, name, false);
    }
    assert_eq!(
        server.counts(),
        before_fresh,
        "fresh builds must not contact the cache"
    );
}

#[cargo_test]
fn build_restores_from_remote_after_removing_all_local_artifacts() {
    restores_from_remote("build");
}

#[cargo_test]
fn check_restores_from_remote_after_removing_all_local_artifacts() {
    restores_from_remote("check");
}

#[cargo_test]
fn local_cache_hits_do_not_contact_remote_cache() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    let seeded = server.counts();

    // Removing only build outputs must leave the shared local cache usable.
    t!(fs::remove_dir_all(p.build_dir()));
    let restored = cargo(&p, "build", "foo").run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    assert_compiled(&restored.stderr, "app", true);
    assert_app(&p, 1, "original");
    assert_eq!(server.counts(), seeded);

    // Local hits must not seed an empty or newly configured remote cache either.
    let empty_server = CacheServer::new(Some(API_KEY));
    empty_server.configure(&p, false);
    t!(fs::remove_dir_all(p.build_dir()));
    let restored = cargo(&p, "build", "foo").run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    assert_app(&p, 1, "original");
    assert_eq!(empty_server.counts(), Counts::default());
}

#[cargo_test]
fn rejected_local_hit_publishes_newly_compiled_output() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    let seeded = server.counts();

    t!(fs::remove_dir_all(p.build_dir()));
    let rebuilt = cargo(&p, "build", "foo")
        .env("REMOTE_CACHE_TEST_VALUE", "replacement")
        .run();
    assert_compiled(&rebuilt.stderr, "common", false);
    assert_compiled(&rebuilt.stderr, "variant", true);
    assert_app(&p, 1, "replacement");
    assert_eq!(server.counts().updates, seeded.updates + 1);

    purge_local_cache(&p);
    let restored = cargo(&p, "build", "foo")
        .env("REMOTE_CACHE_TEST_VALUE", "replacement")
        .run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    assert_app(&p, 1, "replacement");
    assert_eq!(server.counts().updates, seeded.updates + 1);
}

#[cargo_test]
fn read_only_restores_hits_without_publishing_misses() {
    let server = CacheServer::new(None);
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    let seeded = server.counts();
    assert!(seeded.updates >= 2);
    server.configure(&p, true);
    purge_local_cache(&p);
    let hit = cargo(&p, "build", "foo").run();
    assert_compiled(&hit.stderr, "common", false);
    assert_compiled(&hit.stderr, "variant", false);
    assert_app(&p, 1, "original");
    purge_local_cache(&p);
    let miss = cargo(&p, "build", "bar").run();
    assert_compiled(&miss.stderr, "common", false);
    assert_compiled(&miss.stderr, "variant", true);
    assert_app(&p, 2, "original");
    let after = server.counts();
    assert!(after.reads > seeded.reads);
    assert_eq!(after.writes, seeded.writes);
    assert_eq!(after.updates, seeded.updates);
    // A second cold build must still compile the read-only miss.
    purge_local_cache(&p);
    let again = cargo(&p, "build", "bar").run();
    assert_compiled(&again.stderr, "variant", true);
    assert_eq!(server.counts().updates, seeded.updates);
}

#[cargo_test]
fn offline_and_frozen_never_contact_remote_cache() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    // Prepare the registry download and lockfile without enabling remote caching.
    cargo(&p, "build", "foo").run();
    server.configure(&p, false);
    for flag in ["--offline", "--frozen"] {
        purge_local_cache(&p);
        let output = cargo(&p, "build", "foo").arg(flag).run();
        assert_compiled(&output.stderr, "common", true);
        assert_compiled(&output.stderr, "variant", true);
        assert_app(&p, 1, "original");
        assert_eq!(server.counts(), Counts::default());
    }
}

#[cargo_test]
fn corrupt_or_truncated_remote_artifacts_fall_back_to_compilation() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    // Damage a streamed blob and a batched blob, both owned by `common`.
    let (large, small) = {
        let state = server.service.state.lock();
        let (large, blob) = state
            .blobs
            .iter()
            .max_by_key(|(_, blob)| blob.len())
            .unwrap();
        assert!(blob.len() > 1024 * 1024);
        let keys = |action: &reapi::ActionResult| -> Vec<BlobKey> {
            action
                .output_files
                .iter()
                .filter(|file| file.path.starts_with("blobs/"))
                .map(|file| digest_key(file.digest.as_ref().unwrap()).unwrap())
                .collect()
        };
        let common = state
            .actions
            .values()
            .map(keys)
            .find(|keys| keys.contains(large))
            .unwrap();
        let small = common
            .into_iter()
            .filter(|key| key.1 > 0)
            .min_by_key(|key| key.1)
            .unwrap();
        (large.clone(), small)
    };
    server.configure(&p, true);
    for compression in [false, true] {
        server.service.state.lock().compression = compression;
        for key in [&large, &small] {
            let original = server.service.state.lock().blobs[key].clone();
            for truncate in [false, true] {
                let mut damaged = original.clone();
                if truncate {
                    damaged.pop();
                } else {
                    damaged[0] ^= 1;
                }
                server
                    .service
                    .state
                    .lock()
                    .blobs
                    .insert(key.clone(), damaged);
                purge_local_cache(&p);
                let before = server.counts();
                let output = cargo(&p, "build", "foo").run();
                assert!(server.counts().reads > before.reads);
                assert_compiled(&output.stderr, "common", true);
                assert_warning(&output.stderr);
                assert_app(&p, 1, "original");
            }
            server
                .service
                .state
                .lock()
                .blobs
                .insert(key.clone(), original);
        }
    }
}

#[cargo_test]
fn compressed_transfers_round_trip() {
    let server = CacheServer::new(Some(API_KEY));
    server.service.state.lock().compression = true;
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    let uploaded = server.counts();
    assert!(uploaded.compressed_writes > 0 && uploaded.updates >= 2);
    purge_local_cache(&p);
    let restored = cargo(&p, "build", "foo").run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    assert_app(&p, 1, "original");
    assert!(server.counts().compressed_reads > uploaded.compressed_reads);
}

#[cargo_test]
fn transient_failures_are_retried() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    server.configure(&p, true);
    purge_local_cache(&p);
    server.service.state.lock().transient_failures = 2;
    let restored = cargo(&p, "build", "foo").run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert!(!stderr.contains("remote cache disabled"), "{stderr}");
    assert_eq!(server.service.state.lock().transient_failures, 0);
}

#[cargo_test]
fn inlined_metadata_replaces_bytestream_reads() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    let metadata: Vec<BlobKey> = {
        let state = server.service.state.lock();
        state
            .actions
            .values()
            .flat_map(|action| &action.output_files)
            .filter(|file| file.path == "cache-entry")
            .map(|file| digest_key(file.digest.as_ref().unwrap()).unwrap())
            .collect()
    };
    assert!(!metadata.is_empty());
    for ignore_inline in [true, false] {
        {
            let mut state = server.service.state.lock();
            state.ignore_inline = ignore_inline;
            state.read_keys.clear();
        }
        purge_local_cache(&p);
        let output = cargo(&p, "build", "foo").run();
        assert_compiled(&output.stderr, "common", false);
        assert_compiled(&output.stderr, "variant", false);
        assert_app(&p, 1, "original");
        // Metadata goes through ByteStream only when the server ignores the hint.
        let state = server.service.state.lock();
        for key in &metadata {
            assert_eq!(state.read_keys.contains(key), ignore_inline);
        }
    }
}

#[cargo_test]
fn corrupt_inlined_metadata_falls_back_to_compilation() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    server.configure(&p, true);
    server.service.state.lock().corrupt_inline = true;
    purge_local_cache(&p);
    let output = cargo(&p, "build", "foo").run();
    assert_compiled(&output.stderr, "common", true);
    assert_warning(&output.stderr);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn unavailable_or_unauthenticated_cache_does_not_fail_builds() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    for unavailable in [true, false] {
        server.service.state.lock().failure = unavailable.then_some(tonic::Code::Unavailable);
        purge_local_cache(&p);
        let before = server.counts();
        let output = cargo(&p, "build", "foo")
            .env(API_KEY_ENV, "wrong-key")
            .run();
        assert!(server.counts().requests > before.requests);
        assert_compiled(&output.stderr, "common", true);
        assert_compiled(&output.stderr, "variant", true);
        assert_warning(&output.stderr);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("wrong-key"));
        assert_app(&p, 1, "original");
    }
}

#[cargo_test]
fn remote_cache_isolates_features_environment_and_compiler_flags() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    server.configure(&p, true);
    for (feature, label, flags, expected_value) in [
        ("bar", "original", "", 2),
        ("foo", "replacement", "", 1),
        ("foo", "original", "--cfg remote_cache_changed_input", 1),
    ] {
        purge_local_cache(&p);
        let output = cargo(&p, "build", feature)
            .env("REMOTE_CACHE_TEST_VALUE", label)
            .env("RUSTFLAGS", flags)
            .run();
        assert_compiled(&output.stderr, "variant", true);
        if !flags.is_empty() {
            assert_compiled(&output.stderr, "common", true);
        }
        assert_app(&p, expected_value, label);
    }
    // Rejecting an incompatible hit must not destroy the original remote entry.
    purge_local_cache(&p);
    let original = cargo(&p, "build", "foo").run();
    assert_compiled(&original.stderr, "common", false);
    assert_compiled(&original.stderr, "variant", false);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn concurrent_upload_completion_preserves_remote_cache_entries() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    {
        let mut state = server.service.state.lock();
        state.actions.clear();
        state.simulate_upload_race = true;
    }
    purge_local_cache(&p);
    let rebuilt = cargo(&p, "build", "foo").run();
    assert_compiled(&rebuilt.stderr, "common", true);
    assert!(server.counts().early_commits > 0);
    assert!(
        !String::from_utf8_lossy(&rebuilt.stderr)
            .lines()
            .any(|line| line.contains("warning:") && line.contains("remote cache")),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    purge_local_cache(&p);
    server.configure(&p, true);
    let restored = cargo(&p, "build", "foo").run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn unexpected_remote_output_paths_are_rejected() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    {
        let mut state = server.service.state.lock();
        for result in state.actions.values_mut() {
            let mut unexpected = result.output_files[0].clone();
            unexpected.path = "../escaped-output".to_owned();
            result.output_files.push(unexpected);
        }
    }
    purge_local_cache(&p);
    server.configure(&p, true);
    let rebuilt = cargo(&p, "build", "foo").run();
    assert_compiled(&rebuilt.stderr, "common", true);
    assert_compiled(&rebuilt.stderr, "variant", true);
    assert_warning(&rebuilt.stderr);
    assert!(!paths::cargo_home().join("escaped-output").exists());
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn progressing_transfers_can_outlast_timeout() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    server.service.state.lock().upload_delay = Duration::from_millis(300);
    cargo(&p, "build", "foo")
        .args(&["--config", "cache.remote.timeout=1"])
        .run();
    assert_eq!(
        server.counts().updates,
        2,
        "both units must finish uploading"
    );
    purge_local_cache(&p);
    server.configure(&p, true);
    server.service.state.lock().read_delay = Duration::from_millis(5);
    let restored = cargo(&p, "build", "foo")
        .args(&["--config", "cache.remote.timeout=1"])
        .run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "variant", false);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn stalled_download_times_out_with_operation_context() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    cargo(&p, "build", "foo").run();
    purge_local_cache(&p);
    server.configure(&p, true);
    server.service.state.lock().read_delay = Duration::from_secs(2);
    let rebuilt = cargo(&p, "build", "foo")
        .args(&["--config", "cache.remote.timeout=1"])
        .run();
    assert_warning(&rebuilt.stderr);
    let stderr = String::from_utf8_lossy(&rebuilt.stderr);
    assert!(stderr.contains("ByteStream.Read") || stderr.contains("BatchReadBlobs"));
    assert_compiled(&rebuilt.stderr, "common", true);
    assert_compiled(&rebuilt.stderr, "variant", true);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn uploads_do_not_block_dependent_compilation() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    server.service.state.lock().upload_delay = Duration::from_secs(2);
    let output = cargo(&p, "build", "foo").run();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `app` compiles while its dependencies are still uploading, and the build
    // waits for those uploads before it finishes.
    let compiling = stderr.find("Compiling app").unwrap();
    let uploading = stderr.find("Uploading").expect(&stderr);
    assert!(compiling < uploading, "{stderr}");
    assert_eq!(server.counts().updates, 2);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn stalled_upload_times_out_with_operation_context() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    server.service.state.lock().upload_delay = Duration::from_secs(2);
    let built = cargo(&p, "build", "foo")
        .args(&["--config", "cache.remote.timeout=1"])
        .run();
    assert_warning(&built.stderr);
    let stderr = String::from_utf8_lossy(&built.stderr);
    assert!(stderr.contains("ByteStream.Write") || stderr.contains("BatchUpdateBlobs"));
    assert_eq!(server.counts().updates, 0);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn stalled_lookup_times_out_with_operation_context() {
    let server = CacheServer::new(Some(API_KEY));
    let p = cache_project();
    server.configure(&p, false);
    server.service.state.lock().lookup_delay = Duration::from_secs(2);
    let built = cargo(&p, "build", "foo")
        .args(&["--config", "cache.remote.timeout=1"])
        .run();
    assert_warning(&built.stderr);
    assert!(String::from_utf8_lossy(&built.stderr).contains("GetActionResult"));
    assert_compiled(&built.stderr, "common", true);
    assert_compiled(&built.stderr, "variant", true);
    assert_app(&p, 1, "original");
}

#[cargo_test]
fn dependencies_are_prefetched_outside_the_job_queue() {
    for index in 0..4 {
        Package::new(&format!("leaf{index}"), "0.1.0")
            .file("src/lib.rs", &format!("pub fn v() -> u32 {{ {index} }}"))
            .publish();
    }
    Package::new("mid", "0.1.0")
        .dep("leaf0", "0.1.0")
        .file("src/lib.rs", "pub fn v() -> u32 { leaf0::v() + 10 }")
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2021"
                [dependencies]
                leaf1 = "0.1.0"
                leaf2 = "0.1.0"
                leaf3 = "0.1.0"
                mid = "0.1.0"
            "#,
        )
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    println!("{}", leaf1::v() + leaf2::v() + leaf3::v() + mid::v());
                }
            "#,
        )
        .build();
    let server = CacheServer::new(Some(API_KEY));
    server.configure(&p, false);
    let build = || {
        let mut command = p.cargo("build -j1 -vv -Zshared-blob-storage");
        command
            .env(API_KEY_ENV, API_KEY)
            .masquerade_as_nightly_cargo(&["shared-blob-storage"]);
        command.run()
    };
    build();
    purge_local_cache(&p);
    {
        let mut state = server.service.state.lock();
        state.lookup_delay = Duration::from_millis(500);
        state.peak_lookups = 0;
    }
    let restored = build();
    for name in ["leaf0", "leaf1", "leaf2", "leaf3", "mid"] {
        assert_compiled(&restored.stderr, name, false);
    }
    assert_compiled(&restored.stderr, "app", true);
    // With one job slot, restores inside jobs would look up one unit at a time.
    let peak = server.service.state.lock().peak_lookups;
    assert!(peak > 1, "peak concurrent lookups: {peak}");
    p.process(&p.bin("app")).with_stdout_data("16\n").run();
}

/// A library whose only dependency streams its rlib but batches its rmeta.
fn pipelined_project() -> Project {
    Package::new("common", "0.1.0")
        .file("payload", &"x".repeat(1536 * 1024))
        .file(
            "src/lib.rs",
            "pub fn data() -> &'static [u8] { include_bytes!(\"../payload\") }",
        )
        .publish();
    project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2021"
                [dependencies]
                common = "0.1.0"
            "#,
        )
        .file(
            "src/lib.rs",
            "pub fn len() -> usize { common::data().len() }",
        )
        .build()
}

fn build_lib(p: &Project) -> Execs {
    let mut command = p.cargo("build -vv -Zshared-blob-storage");
    command
        .env(API_KEY_ENV, API_KEY)
        .masquerade_as_nightly_cargo(&["shared-blob-storage"]);
    command
}

fn contains_file(dir: &Path, matches: &dyn Fn(&str) -> bool) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            contains_file(&path, matches)
        } else {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(matches)
        }
    })
}

#[cargo_test]
fn dependents_compile_against_metadata_before_rlibs_arrive() {
    let server = CacheServer::new(Some(API_KEY));
    let p = pipelined_project();
    server.configure(&p, false);
    build_lib(&p).run();
    purge_local_cache(&p);
    let target = p.build_dir();
    server.service.state.lock().hold_reads_until = Some(Arc::new(move || {
        contains_file(&target, &|name| {
            name.starts_with("libapp-") && name.ends_with(".rmeta")
        })
    }));
    let restored = build_lib(&p).run();
    assert_compiled(&restored.stderr, "common", false);
    assert_compiled(&restored.stderr, "app", true);
    // Streaming `common`'s rlib waited until `app` compiled against its rmeta.
    let held = server.service.state.lock().held_reads.clone();
    assert!(
        !held.is_empty() && held.iter().all(|released| *released),
        "{held:?}"
    );
}

#[cargo_test]
fn failed_download_after_metadata_use_fails_the_build() {
    let server = CacheServer::new(Some(API_KEY));
    let p = pipelined_project();
    server.configure(&p, false);
    build_lib(&p).run();
    purge_local_cache(&p);
    server.service.state.lock().fail_reads = true;
    build_lib(&p)
        .with_status(101)
        .with_stderr_contains(
            "[ERROR] failed to restore `common v0.1.0` from the build cache after dependents started using its metadata",
        )
        .run();
    // The next build fetches the unit again.
    server.service.state.lock().fail_reads = false;
    let rebuilt = build_lib(&p).run();
    assert_compiled(&rebuilt.stderr, "common", false);
}

#[cargo_test]
fn build_script_runs_restore_from_remote() {
    Package::new("generated", "0.1.0")
        .file(
            "build.rs",
            r#"
            fn main() {
                let out = std::env::var("OUT_DIR").unwrap();
                std::fs::write(format!("{out}/value.rs"), "pub const VALUE: u32 = 7;").unwrap();
                println!("cargo::rustc-link-search=native={out}");
            }
        "#,
        )
        .file(
            "src/lib.rs",
            r#"include!(concat!(env!("OUT_DIR"), "/value.rs"));"#,
        )
        .publish();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2021"
                [dependencies]
                generated = "0.1.0"
            "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", generated::VALUE); }",
        )
        .build();
    let server = CacheServer::new(Some(API_KEY));
    server.configure(&p, false);
    let build = || {
        let mut command = p.cargo("build -vv -Zshared-blob-storage");
        command
            .env(API_KEY_ENV, API_KEY)
            .masquerade_as_nightly_cargo(&["shared-blob-storage"]);
        command.run()
    };
    build();
    purge_local_cache(&p);
    let restored = build();
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert_compiled(&restored.stderr, "build_script_build", false);
    assert_compiled(&restored.stderr, "generated", false);
    assert!(
        !stderr
            .lines()
            .any(|line| line.contains("Running") && line.contains("build_script_build`")),
        "{stderr}"
    );
    p.process(&p.bin("app")).with_stdout_data("7\n").run();
}
