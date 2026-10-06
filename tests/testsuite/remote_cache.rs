use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::TcpListener;
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

type BlobKey = (String, i64);
type RpcStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Counts {
    requests: usize,
    reads: usize,
    writes: usize,
    updates: usize,
    early_commits: usize,
}

#[derive(Default)]
struct State {
    actions: BTreeMap<BlobKey, reapi::ActionResult>,
    blobs: BTreeMap<BlobKey, Vec<u8>>,
    counts: Counts,
    failure: Option<tonic::Code>,
    simulate_upload_race: bool,
    upload_delay: Duration,
    read_delay: Duration,
    lookup_delay: Duration,
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
                    .add_service(bs::byte_stream_server::ByteStreamServer::new(server_service))
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

    fn resource(&self, resource: &str, upload: bool) -> Result<BlobKey, Status> {
        let resource = resource
            .strip_prefix(&format!("{INSTANCE}/"))
            .ok_or_else(|| Status::invalid_argument("wrong resource instance"))?;
        let parts: Vec<_> = resource.split('/').collect();
        let parts = if upload {
            if parts.len() != 5 || parts[0] != "uploads" || parts[1].is_empty() {
                return Err(Status::invalid_argument("invalid upload resource"));
            }
            &parts[2..]
        } else {
            &parts[..]
        };
        if parts.len() != 3 || parts[0] != "blobs" {
            return Err(Status::invalid_argument("invalid blob resource"));
        }
        digest_key(&reapi::Digest {
            hash: parts[1].to_owned(),
            size_bytes: parts[2]
                .parse()
                .map_err(|_| Status::invalid_argument("invalid resource size"))?,
        })
    }
}

#[tonic::async_trait]
impl reapi::action_cache_server::ActionCache for CacheService {
    async fn get_action_result(
        &self,
        request: Request<reapi::GetActionResultRequest>,
    ) -> Result<Response<reapi::ActionResult>, Status> {
        self.authorize(&request)?;
        let delay = self.state.lock().lookup_delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let request = request.into_inner();
        self.instance(&request.instance_name, request.digest_function)?;
        let key = digest_key(
            request
                .action_digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("missing action digest"))?,
        )?;
        let state = self.state.lock();
        let result = state
            .actions
            .get(&key)
            .ok_or_else(|| Status::not_found("action not cached"))?;
        for output in &result.output_files {
            if !state
                .blobs
                .contains_key(&digest_key(output.digest.as_ref().unwrap())?)
            {
                return Err(Status::not_found("referenced blob evicted"));
            }
        }
        Ok(Response::new(result.clone()))
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
                    || output.path == "unit-output"
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
        if !paths.contains("cache-entry") || !paths.contains("unit-output") || paths.len() < 3 {
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
        Err(Status::unimplemented("fixture requires ByteStream uploads"))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<reapi::BatchReadBlobsRequest>,
    ) -> Result<Response<reapi::BatchReadBlobsResponse>, Status> {
        self.authorize(&request)?;
        Err(Status::unimplemented(
            "fixture requires ByteStream downloads",
        ))
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
        let request = request.into_inner();
        let key = self.resource(&request.resource_name, false)?;
        let mut state = self.state.lock();
        state.counts.reads += 1;
        let data = state
            .blobs
            .get(&key)
            .ok_or_else(|| Status::not_found("blob missing"))?;
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
        let mut data = Vec::new();
        let mut finished = false;
        while let Some(part) = stream.message().await? {
            if finished || part.write_offset != data.len() as i64 {
                return Err(Status::invalid_argument(
                    "noncontiguous write or data after finish",
                ));
            }
            if resource.is_none() {
                let parsed = self.resource(&part.resource_name, true)?;
                let mut state = self.state.lock();
                if state.simulate_upload_race && state.blobs.contains_key(&parsed) {
                    state.counts.early_commits += 1;
                    return Ok(Response::new(bs::WriteResponse {
                        committed_size: parsed.1,
                    }));
                }
                key = Some(parsed);
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
        if !finished || key.1 != data.len() as i64 || key.0 != hex::encode(Sha256::digest(&data)) {
            return Err(Status::invalid_argument(
                "unfinished upload or digest mismatch",
            ));
        }
        let committed_size = data.len() as i64;
        let mut state = self.state.lock();
        state.counts.writes += 1;
        state.blobs.insert(key, data);
        Ok(Response::new(bs::WriteResponse { committed_size }))
    }

    async fn query_write_status(
        &self,
        request: Request<bs::QueryWriteStatusRequest>,
    ) -> Result<Response<bs::QueryWriteStatusResponse>, Status> {
        self.authorize(&request)?;
        let key = self.resource(&request.into_inner().resource_name, true)?;
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
        .file("payload", &"x".repeat(256 * 1024))
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
    for path in [
        p.build_dir(),
        paths::cargo_home().join("blobs"),
        paths::cargo_home().join("shared-storage"),
    ] {
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
        .with_stdout_data(format!("31457280 {value} {label}\n"))
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
    assert!(uploaded.writes > 0 && uploaded.updates >= 2);
    purge_local_cache(&p);
    let restored = cargo(&p, mode, "foo").run();
    for name in ["common", "variant"] {
        assert_compiled(&restored.stderr, name, false);
    }
    assert_compiled(&restored.stderr, "app", true);
    let after_restore = server.counts();
    assert!(after_restore.reads > uploaded.reads);
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
    let (key, original) = {
        let state = server.service.state.lock();
        let (key, blob) = state
            .blobs
            .iter()
            .max_by_key(|(_, blob)| blob.len())
            .unwrap();
        assert!(blob.len() > 256 * 1024);
        (key.clone(), blob.clone())
    };
    server.configure(&p, true);
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
    server.service.state.lock().read_delay = Duration::from_millis(30);
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
    assert!(String::from_utf8_lossy(&rebuilt.stderr).contains("ByteStream.Read"));
    assert_compiled(&rebuilt.stderr, "common", true);
    assert_compiled(&rebuilt.stderr, "variant", true);
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
    assert!(String::from_utf8_lossy(&built.stderr).contains("ByteStream.Write"));
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
