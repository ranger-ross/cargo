//! Local reuse of immutable compiler units, independent of fingerprint propagation.

use std::ffi::OsStr;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context as _;
use cargo_util::{ProcessBuilder, paths};

use super::blob_storage::{
    BlobStorage, DependencyArtifact, Output, PendingOutputs, PrefetchSource, Restored,
    TrackedOutput, build_script_marker, output_path_key,
};
use super::fingerprint::{self, Fingerprint};
use super::{
    BuildOutput, BuildRunner, BuildScriptOutputs, CompileMode, Executor, FileFlavor, LibraryPath,
    Unit, UnitHash,
};
use crate::util::CargoResult;

pub(super) struct LocalCache {
    storage: Arc<BlobStorage>,
    unit_hash: String,
    unit_dir: PathBuf,
    fingerprint: Arc<Fingerprint>,
    /// Dependency artifacts hashed into the key, sorted by path.
    dependencies: Vec<PathBuf>,
    /// The same artifacts as cache metadata paths, for prefetching. Empty when
    /// an artifact lies outside its unit's output tree.
    artifacts: Vec<DependencyArtifact>,
    build_script_outputs: Arc<Mutex<BuildScriptOutputs>>,
    /// This package's build scripts. Their output is part of the key.
    script_metadatas: Vec<UnitHash>,
    /// `OUT_DIR`s of this package's build scripts. Their contents are part of the key.
    out_dirs: Vec<PathBuf>,
    /// Finish markers of this package's build-script runs.
    script_dirs: Vec<PathBuf>,
    /// A build script or proc-macro, or a unit they link against. Its full
    /// outputs are needed early.
    urgent: bool,
    key: OnceLock<u64>,
    restored: AtomicBool,
}

impl LocalCache {
    pub(super) fn new(
        build_runner: &mut BuildRunner<'_, '_>,
        unit: &Unit,
        exec: &Arc<dyn Executor>,
        force: bool,
    ) -> CargoResult<Option<Arc<Self>>> {
        let bcx = build_runner.bcx;
        if !super::should_dedup_out_dir(build_runner, unit)
            || force
            || !exec.supports_local_cache()
            || bcx.rustc().wrapper.is_some()
            || bcx.rustc().workspace_wrapper.is_some()
            || bcx.gctx.get_env_os("RUSTC").is_some()
            || bcx.gctx.build_config()?.rustc.is_some()
            || !cacheable_unit(build_runner, unit, exec)
        {
            return Ok(None);
        }
        let Some(storage) = build_runner.files().blob_storage() else {
            return Ok(None);
        };
        let no_embed_metadata = !bcx.target_data.info(unit.kind).should_embed_metadata();
        let mut dependencies = Vec::new();
        for dep in build_runner.unit_deps(unit) {
            let metadata_only =
                build_runner.only_requires_rmeta(unit, &dep.unit) || dep.unit.mode.is_check();
            let dep_dir = build_runner.files().build_unit_dir(&dep.unit);
            for output in build_runner.outputs(&dep.unit)?.iter() {
                // Match the artifacts supplied by extern_args. Metadata is available
                // before a pipelined dependency has finished producing its rlib.
                if (output.flavor == FileFlavor::Rmeta && (metadata_only || no_embed_metadata))
                    || (output.flavor == FileFlavor::Linkable && !metadata_only)
                {
                    let artifact = output
                        .path
                        .strip_prefix(&dep_dir)
                        .ok()
                        .and_then(|relative| output_path_key(relative).ok())
                        .map(|path| DependencyArtifact {
                            unit_dir: dep_dir.clone(),
                            path,
                        });
                    dependencies.push((output.path.clone(), artifact));
                }
            }
        }
        dependencies.sort_by(|a, b| a.0.cmp(&b.0));
        dependencies.dedup_by(|a, b| a.0 == b.0);
        let (dependencies, artifacts): (Vec<_>, Vec<_>) = dependencies.into_iter().unzip();
        let artifacts = artifacts.into_iter().collect::<Option<Vec<_>>>();
        let prefetchable = artifacts.is_some();
        let script_runs = build_runner
            .unit_deps(unit)
            .iter()
            .filter(|dep| dep.unit.mode.is_run_custom_build())
            .map(|dep| dep.unit.clone())
            .collect::<Vec<_>>();
        let out_dirs = script_runs
            .iter()
            .map(|run| build_runner.files().out_dir_new_layout(run))
            .collect();
        let script_dirs = script_runs
            .iter()
            .map(|run| build_script_marker(&build_runner.files().build_unit_dir(run)))
            .collect();
        let cache = Arc::new(Self {
            storage,
            unit_hash: build_runner.files().unit_hash(unit),
            unit_dir: build_runner.files().build_unit_dir(unit),
            fingerprint: Arc::clone(&build_runner.fingerprints[unit]),
            dependencies,
            artifacts: artifacts.unwrap_or_default(),
            build_script_outputs: Arc::clone(&build_runner.build_script_outputs),
            script_metadatas: build_runner
                .find_build_script_metadatas(unit)
                .unwrap_or_default(),
            out_dirs,
            script_dirs,
            urgent: build_runner.is_build_tool_dep(unit)
                || unit.target.proc_macro()
                || unit.target.is_custom_build(),
            key: OnceLock::new(),
            restored: AtomicBool::new(false),
        });
        if !prefetchable {
            tracing::debug!(unit_hash = cache.unit_hash, "unit cannot be prefetched");
        }
        Ok(Some(cache))
    }

    /// Queue this unit for remote prefetching ahead of its job.
    pub(super) fn prefetch(self: &Arc<Self>) {
        if self.artifacts.len() == self.dependencies.len() {
            self.storage
                .prefetch(Arc::clone(self) as Arc<dyn PrefetchSource>);
        }
    }

    /// Whether a remote fetch should keep this unit's job from starting. With
    /// `metadata_first`, the job can start once the rmeta is available.
    pub(super) fn prefetching(&self, metadata_first: bool) -> bool {
        self.storage.prefetching(&self.unit_hash, metadata_first)
    }

    /// The input guard computed from the dependency artifacts on disk.
    fn key(&self) -> CargoResult<u64> {
        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        let hashes = self
            .dependencies
            .iter()
            .map(|dependency| {
                let mut hasher = blake3::Hasher::new();
                hasher.update_reader(paths::open(dependency)?)?;
                Ok(*hasher.finalize().as_bytes())
            })
            .collect::<CargoResult<Vec<_>>>()?;
        let key = self.guard(&hashes)?;
        let _ = self.key.set(key);
        Ok(key)
    }

    /// The input guard. The unit hash and fingerprint do not cover dependency
    /// artifacts, the parsed build-script output, or generated `OUT_DIR` files.
    /// It is computed after build scripts have run. `dependency_hashes` are the
    /// BLAKE3 hashes of `dependencies`, which equal their blob hashes.
    fn guard(&self, dependency_hashes: &[[u8; 32]]) -> CargoResult<u64> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.fingerprint.hash_u64().to_le_bytes());
        for hash in dependency_hashes {
            hasher.update(hash);
        }
        if !self.script_metadatas.is_empty() {
            let outputs = self.build_script_outputs.lock().unwrap();
            for metadata in &self.script_metadatas {
                let output = outputs
                    .get(*metadata)
                    .context("missing build script output")?;
                hash_build_output(&mut hasher, output, &self.out_dirs);
            }
        }
        for out_dir in &self.out_dirs {
            hasher.update(&out_dir_hash(&self.storage, out_dir)?);
        }
        Ok(u64::from_le_bytes(
            hasher.finalize().as_bytes()[..8].try_into().unwrap(),
        ))
    }

    /// See [`validate_dep_info`]. Generated inputs from this package's `OUT_DIR`s
    /// are accepted because their contents are part of the key.
    pub(super) fn validate_dep_info(
        &self,
        dep_info: &Path,
        rustc: &ProcessBuilder,
        cwd: &Path,
        pkg_root: &Path,
    ) -> CargoResult<()> {
        validate_dep_info(dep_info, rustc, cwd, pkg_root, &self.out_dirs)
    }

    /// The unit hash that keys this unit's cache entry.
    pub(super) fn unit_hash(&self) -> &str {
        &self.unit_hash
    }

    /// With `metadata_first`, a restore can return [`Restored::Metadata`]
    /// before rlibs and object files are available.
    pub(super) fn restore(&self, metadata_first: bool) -> CargoResult<Restored> {
        self.storage.restore_cache_entry(
            &self.unit_hash,
            self.key()?,
            &self.unit_dir,
            metadata_first,
        )
    }

    pub(super) fn finish_restore(&self, pending: PendingOutputs) -> CargoResult<()> {
        self.storage.finish_restore(pending, &self.unit_dir)
    }

    pub(super) fn accept(&self) -> CargoResult<()> {
        self.storage
            .accept_cache_entry(&self.unit_hash, &self.unit_dir)?;
        self.restored.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn restored(&self) -> bool {
        self.restored.load(Ordering::Relaxed)
    }

    pub(super) fn publish(&self, outputs: Vec<Output>) -> CargoResult<TrackedOutput> {
        self.storage
            .publish_cache_entry(&self.unit_hash, self.key()?, outputs, &self.unit_dir)
    }
}

impl PrefetchSource for LocalCache {
    fn unit_hash(&self) -> &str {
        &self.unit_hash
    }

    fn unit_dir(&self) -> &Path {
        &self.unit_dir
    }

    fn dependencies(&self) -> &[DependencyArtifact] {
        &self.artifacts
    }

    fn build_scripts(&self) -> &[PathBuf] {
        &self.script_dirs
    }

    fn key_from(&self, dependency_hashes: &[[u8; 32]]) -> CargoResult<u64> {
        self.guard(dependency_hashes)
    }

    fn urgent(&self) -> bool {
        self.urgent
    }
}

/// Caches a build-script run of an immutable package: its `OUT_DIR` and the
/// script's captured output. A hit replaces running the script.
pub(super) struct RunCache {
    storage: Arc<BlobStorage>,
    unit_hash: String,
    unit_dir: PathBuf,
    fingerprint: Arc<Fingerprint>,
    /// The compiled build script.
    script: PathBuf,
    /// The build script as cache metadata, for prefetching.
    artifacts: Vec<DependencyArtifact>,
    /// Environment Cargo sets for the script, except values that do not
    /// affect its output.
    env: Vec<(String, Option<std::ffi::OsString>)>,
    /// Runs of linked dependencies. Their metadata reaches the script as
    /// `DEP_*` variables and their `OUT_DIR`s often hold headers it reads.
    dep_metadatas: Vec<UnitHash>,
    dep_out_dirs: Vec<PathBuf>,
    dep_markers: Vec<PathBuf>,
    build_script_outputs: Arc<Mutex<BuildScriptOutputs>>,
    key: OnceLock<u64>,
}

/// Records the values of `rerun-if-env-changed` variables when a run is
/// published, so a restore can reject a run made under different values.
const RERUN_ENV: &str = "run/rerun-env";
/// The guard of the run that produced the current outputs. A fresh run cannot
/// recompute it because its fingerprint changes after the script runs.
const RUN_KEY: &str = "run/cache-key";

impl RunCache {
    /// `env` is what Cargo sets for the script before adding `DEP_*` variables.
    pub(super) fn new(
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
        script_unit: &Unit,
        script: PathBuf,
        env: &std::collections::BTreeMap<String, Option<std::ffi::OsString>>,
    ) -> CargoResult<Option<Arc<Self>>> {
        let bcx = build_runner.bcx;
        if !immutable(unit)
            || unit.pkg.manifest().metabuild().is_some()
            || bcx.build_config.force_rebuild
            || bcx.rustc().wrapper.is_some()
            || bcx.gctx.get_env_os("RUSTC").is_some()
            || bcx.gctx.build_config()?.rustc.is_some()
        {
            return Ok(None);
        }
        let Some(storage) = build_runner.files().blob_storage() else {
            return Ok(None);
        };
        let script_dir = build_runner.files().build_unit_dir(script_unit);
        let artifacts = script
            .strip_prefix(&script_dir)
            .ok()
            .and_then(|relative| output_path_key(relative).ok())
            .map(|path| DependencyArtifact {
                unit_dir: script_dir,
                path,
            })
            .into_iter()
            .collect();
        let dep_runs = build_runner
            .unit_deps(unit)
            .iter()
            .filter(|dep| dep.unit.mode.is_run_custom_build())
            .map(|dep| dep.unit.clone())
            .collect::<Vec<_>>();
        Ok(Some(Arc::new(Self {
            storage,
            unit_hash: build_runner.files().unit_hash(unit),
            unit_dir: build_runner.files().build_unit_dir(unit),
            fingerprint: Arc::clone(&build_runner.fingerprints[unit]),
            script,
            artifacts,
            // The job count changes parallelism, not results.
            env: env
                .iter()
                .filter(|(name, _)| *name != "NUM_JOBS")
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            dep_metadatas: dep_runs
                .iter()
                .map(|run| build_runner.get_run_build_script_metadata(run))
                .collect(),
            dep_out_dirs: dep_runs
                .iter()
                .map(|run| build_runner.files().out_dir_new_layout(run))
                .collect(),
            dep_markers: dep_runs
                .iter()
                .map(|run| build_script_marker(&build_runner.files().build_unit_dir(run)))
                .collect(),
            build_script_outputs: Arc::clone(&build_runner.build_script_outputs),
            key: OnceLock::new(),
        })))
    }

    /// Queue this run for remote prefetching ahead of its job.
    pub(super) fn prefetch(self: &Arc<Self>) {
        if !self.artifacts.is_empty() {
            self.storage
                .prefetch(Arc::clone(self) as Arc<dyn PrefetchSource>);
        }
    }

    pub(super) fn prefetching(&self) -> bool {
        self.storage.prefetching(&self.unit_hash, false)
    }

    fn key(&self) -> CargoResult<u64> {
        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(paths::open(&self.script)?)?;
        let key = self.guard(hasher.finalize().as_bytes())?;
        let _ = self.key.set(key);
        Ok(key)
    }

    /// The input guard. It is computed before the script runs, so the run's
    /// fingerprint is hashed directly. Its memoized hash would go stale once
    /// the script reports what it depends on.
    fn guard(&self, script_hash: &[u8; 32]) -> CargoResult<u64> {
        let mut hasher = blake3::Hasher::new();
        put(&mut hasher, b"build-script-run");
        hasher.update(&crate::util::hash_u64(&*self.fingerprint).to_le_bytes());
        hasher.update(script_hash);
        hasher.update(&(self.env.len() as u64).to_le_bytes());
        for (name, value) in &self.env {
            put(&mut hasher, name.as_bytes());
            match value {
                Some(value) => put(&mut hasher, value.as_encoded_bytes()),
                None => put(&mut hasher, b"\0unset"),
            }
        }
        {
            let outputs = self.build_script_outputs.lock().unwrap();
            for metadata in &self.dep_metadatas {
                let output = outputs
                    .get(*metadata)
                    .context("missing dependency build script output")?;
                hasher.update(&(output.metadata.len() as u64).to_le_bytes());
                for (key, value) in &output.metadata {
                    put(&mut hasher, key.as_bytes());
                    put(&mut hasher, value.as_bytes());
                }
            }
        }
        for out_dir in &self.dep_out_dirs {
            hasher.update(&out_dir_hash(&self.storage, out_dir)?);
        }
        Ok(u64::from_le_bytes(
            hasher.finalize().as_bytes()[..8].try_into().unwrap(),
        ))
    }

    /// Restore the run's outputs. The caller parses the restored stdout and
    /// calls [`RunCache::accept`] once [`RunCache::env_matches`] holds.
    pub(super) fn restore(&self) -> CargoResult<bool> {
        Ok(matches!(
            self.storage.restore_cache_entry(
                &self.unit_hash,
                self.key()?,
                &self.unit_dir,
                false
            )?,
            Restored::Complete
        ))
    }

    /// Whether `rerun-if-env-changed` variables have the values they had when
    /// the restored run was published. `current` looks up the script's value.
    pub(super) fn env_matches(
        &self,
        output: &BuildOutput,
        current: impl Fn(&str) -> Option<String>,
    ) -> CargoResult<bool> {
        let path = self.unit_dir.join(RERUN_ENV);
        let recorded: Vec<(String, Option<String>)> = if path.exists() {
            serde_json::from_slice(&paths::read_bytes(&path)?)?
        } else {
            Vec::new()
        };
        Ok(output.rerun_if_env_changed.iter().all(|name| {
            let value = recorded
                .iter()
                .find(|(recorded, _)| recorded == name)
                .and_then(|(_, value)| value.clone());
            value == current(name)
        }))
    }

    pub(super) fn accept(&self) -> CargoResult<()> {
        self.storage
            .accept_cache_entry(&self.unit_hash, &self.unit_dir)?;
        paths::write(self.unit_dir.join(RUN_KEY), self.key()?.to_string())
    }

    /// Keep a fresh run's entry in this build's snapshot, republishing it if
    /// shared storage lost it.
    pub(super) fn retain(&self) -> CargoResult<()> {
        let tracked = Some(TrackedOutput::CacheEntry);
        if self
            .storage
            .prepare_unit(tracked, Some(&self.unit_hash), &self.unit_dir)?
            .is_some()
        {
            return Ok(());
        }
        let path = self.unit_dir.join(RUN_KEY);
        if !path.exists() {
            return Ok(());
        }
        let key = paths::read(&path)?.trim().parse()?;
        self.publish_outputs(key)
    }

    /// Publish a successful run. `current` looks up the script's environment.
    pub(super) fn publish(
        &self,
        output: &BuildOutput,
        current: impl Fn(&str) -> Option<String>,
    ) -> CargoResult<()> {
        let env = output
            .rerun_if_env_changed
            .iter()
            .map(|name| (name.clone(), current(name)))
            .collect::<Vec<_>>();
        paths::write(self.unit_dir.join(RERUN_ENV), serde_json::to_vec(&env)?)?;
        let key = self.key()?;
        self.publish_outputs(key)?;
        paths::write(self.unit_dir.join(RUN_KEY), key.to_string())
    }

    fn publish_outputs(&self, key: u64) -> CargoResult<()> {
        let run = self.unit_dir.join("run");
        // The run may rewrite `OUT_DIR` in place next time, so blobs must not
        // be hardlinked into it.
        let outputs = self.storage.capture_files(
            &self.unit_dir,
            &[
                self.unit_dir.join("out"),
                run.join("stdout"),
                run.join("stderr"),
                run.join("root-output"),
                self.unit_dir.join(RERUN_ENV),
            ],
            false,
        )?;
        self.storage
            .publish_cache_entry(&self.unit_hash, key, outputs, &self.unit_dir)?;
        Ok(())
    }
}

impl PrefetchSource for RunCache {
    fn unit_hash(&self) -> &str {
        &self.unit_hash
    }

    fn unit_dir(&self) -> &Path {
        &self.unit_dir
    }

    fn dependencies(&self) -> &[DependencyArtifact] {
        &self.artifacts
    }

    fn build_scripts(&self) -> &[PathBuf] {
        &self.dep_markers
    }

    fn key_from(&self, dependency_hashes: &[[u8; 32]]) -> CargoResult<u64> {
        self.guard(&dependency_hashes[0])
    }

    /// Dependents compile against the generated files.
    fn urgent(&self) -> bool {
        true
    }
}

fn cacheable_unit(
    build_runner: &mut BuildRunner<'_, '_>,
    unit: &Unit,
    exec: &Arc<dyn Executor>,
) -> bool {
    if let Some(eligible) = build_runner.local_cache_eligible.get(unit) {
        return *eligible;
    }
    let mut eligible = immutable(unit)
        && (unit.target.is_lib() || unit.target.is_custom_build())
        && matches!(
            unit.mode,
            CompileMode::Build | CompileMode::Check { test: false }
        )
        && !unit.artifact.is_true()
        && build_runner.bcx.extra_args_for(unit).is_none()
        && !exec.force_rebuild(unit);
    if eligible {
        let dependencies = Vec::from(build_runner.unit_deps(unit));
        eligible = dependencies.iter().all(|dep| {
            let dep = &dep.unit;
            if dep.mode.is_run_custom_build() {
                // The key covers this package's build-script output and `OUT_DIR`.
                immutable(dep) && dep.pkg.package_id() == unit.pkg.package_id()
            } else if dep.target.proc_macro() {
                // The key covers the compiled proc-macro. Like Cargo's own
                // freshness checks, it does not cover untracked inputs that a
                // macro reads while expanding.
                immutable(dep)
            } else {
                cacheable_unit(build_runner, dep, exec)
            }
        });
    }
    build_runner
        .local_cache_eligible
        .insert(unit.clone(), eligible);
    eligible
}

pub(super) fn immutable(unit: &Unit) -> bool {
    let source = unit.pkg.package_id().source_id();
    !unit.is_local() && (source.is_registry() || source.is_git())
}

fn put(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn put_strings(hasher: &mut blake3::Hasher, tag: &[u8], values: &[String]) {
    put(hasher, tag);
    hasher.update(&(values.len() as u64).to_le_bytes());
    for value in values {
        put(hasher, value.as_bytes());
    }
}

/// Hash the build-script output that reaches rustc. Paths inside an `OUT_DIR`
/// are hashed relative to it because build directories differ per workspace.
fn hash_build_output(hasher: &mut blake3::Hasher, output: &BuildOutput, out_dirs: &[PathBuf]) {
    put_strings(hasher, b"cfg", &output.cfgs);
    put_strings(hasher, b"check-cfg", &output.check_cfgs);
    put_strings(hasher, b"link", &output.library_links);
    put(hasher, b"env");
    hasher.update(&(output.env.len() as u64).to_le_bytes());
    for (name, value) in &output.env {
        put(hasher, name.as_bytes());
        put(hasher, value.as_bytes());
    }
    put(hasher, b"link-arg");
    hasher.update(&(output.linker_args.len() as u64).to_le_bytes());
    for (target, arg) in &output.linker_args {
        put(hasher, format!("{target:?}").as_bytes());
        put(hasher, arg.as_bytes());
    }
    put(hasher, b"link-search");
    hasher.update(&(output.library_paths.len() as u64).to_le_bytes());
    for library_path in &output.library_paths {
        let (kind, path) = match library_path {
            LibraryPath::CargoArtifact(path) => (&b"artifact"[..], path),
            LibraryPath::External(path) => (&b"external"[..], path),
        };
        put(hasher, kind);
        match out_dirs
            .iter()
            .enumerate()
            .find_map(|(index, out_dir)| Some((index, path.strip_prefix(out_dir).ok()?)))
        {
            Some((index, relative)) => {
                hasher.update(&(index as u64).to_le_bytes());
                put(hasher, relative.as_os_str().as_encoded_bytes());
            }
            None => {
                hasher.update(&u64::MAX.to_le_bytes());
                put(hasher, path.as_os_str().as_encoded_bytes());
            }
        }
    }
}

/// The hash of a finished build script's `OUT_DIR`, computed once per build.
/// Directories with generated sources can be large, so hashing them twice
/// would delay dependents.
pub(super) fn out_dir_hash(storage: &BlobStorage, out_dir: &Path) -> CargoResult<[u8; 32]> {
    storage.tree_hash(out_dir, || {
        let mut hasher = blake3::Hasher::new();
        hash_tree(&mut hasher, out_dir)?;
        Ok(*hasher.finalize().as_bytes())
    })
}

/// Hash relative paths and contents of every file under `root`.
fn hash_tree(hasher: &mut blake3::Hasher, root: &Path) -> CargoResult<()> {
    let entries = walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let files = entries
        .iter()
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| Ok((entry.path(), entry.metadata()?.len())))
        .collect::<CargoResult<Vec<_>>>()?;
    let mut digests = hash_files(&files)?.into_iter();
    for entry in &entries {
        put(
            hasher,
            entry
                .path()
                .strip_prefix(root)?
                .as_os_str()
                .as_encoded_bytes(),
        );
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            put(hasher, b"symlink");
            put(
                hasher,
                std::fs::read_link(entry.path())?
                    .as_os_str()
                    .as_encoded_bytes(),
            );
        } else if file_type.is_file() {
            put(hasher, b"file");
            hasher.update(&digests.next().expect("one digest per file"));
        } else {
            put(hasher, b"dir");
        }
    }
    Ok(())
}

/// Files are hashed in chunks of this size so one large file can use many
/// threads.
const TREE_CHUNK: u64 = 16 * 1024 * 1024;

/// Hash files across threads. A file's digest covers its chunk digests in
/// order. Generated trees can exceed a gigabyte and are on the critical path
/// of their dependents, often while every core is busy compiling.
fn hash_files(files: &[(&Path, u64)]) -> CargoResult<Vec<[u8; 32]>> {
    let chunks = files
        .iter()
        .enumerate()
        .flat_map(|(file, (_, len))| {
            (0..len.div_ceil(TREE_CHUNK).max(1)).map(move |chunk| (file, chunk * TREE_CHUNK))
        })
        .collect::<Vec<_>>();
    let threads = std::thread::available_parallelism()
        .map_or(1, |threads| threads.get())
        .min(chunks.len())
        .max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut chunk_digests = vec![[0; 32]; chunks.len()];
    std::thread::scope(|scope| -> CargoResult<()> {
        let workers = (0..threads)
            .map(|_| {
                scope.spawn(|| -> CargoResult<Vec<(usize, [u8; 32])>> {
                    let mut hashed = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(file, offset)) = chunks.get(index) else {
                            return Ok(hashed);
                        };
                        let mut reader = paths::open(files[file].0)?;
                        reader.seek(SeekFrom::Start(offset))?;
                        let mut hasher = blake3::Hasher::new();
                        hasher.update_reader(reader.take(TREE_CHUNK))?;
                        hashed.push((index, *hasher.finalize().as_bytes()));
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            for (index, digest) in worker.join().expect("hash worker panicked")? {
                chunk_digests[index] = digest;
            }
        }
        Ok(())
    })?;
    let mut digests = Vec::with_capacity(files.len());
    let mut chunk_digests = chunks.iter().zip(chunk_digests).peekable();
    for file in 0..files.len() {
        let mut hasher = blake3::Hasher::new();
        while let Some((_, digest)) = chunk_digests.next_if(|((owner, _), _)| *owner == file) {
            hasher.update(&digest);
        }
        digests.push(*hasher.finalize().as_bytes());
    }
    Ok(digests)
}

/// Map a recorded `OUT_DIR` input to this build's `OUT_DIR`. The cached dep-info
/// records the publisher's absolute path. A run unit's `<package>/<hash>/out`
/// suffix is the same in every build directory.
fn out_dir_input(path: &Path, out_dirs: &[PathBuf]) -> Option<PathBuf> {
    let components: Vec<_> = path.components().collect();
    for out_dir in out_dirs {
        let suffix: Vec<_> = out_dir.components().rev().take(3).collect();
        if suffix.len() < 3 {
            continue;
        }
        let suffix: Vec<_> = suffix.into_iter().rev().collect();
        let Some(start) = components
            .windows(suffix.len())
            .rposition(|window| window == suffix.as_slice())
        else {
            continue;
        };
        let relative = &components[start + suffix.len()..];
        if !relative.is_empty()
            && relative
                .iter()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Some(out_dir.join(relative.iter().collect::<PathBuf>()));
        }
    }
    None
}

/// Validate inputs not included in Cargo's structural fingerprint. In particular,
/// env! and option_env! are discovered only in rustc's dep-info, which is absent
/// when restoring into a clean build directory. Inputs must come from the
/// immutable package or from one of `out_dirs`, whose contents the key covers.
pub(super) fn validate_dep_info(
    dep_info: &Path,
    rustc: &ProcessBuilder,
    cwd: &Path,
    pkg_root: &Path,
    out_dirs: &[PathBuf],
) -> CargoResult<()> {
    let dep_info = fingerprint::parse_rustc_dep_info(dep_info)?;
    anyhow::ensure!(!dep_info.files.is_empty(), "cached dep-info has no inputs");
    for (name, expected) in &dep_info.env {
        anyhow::ensure!(
            rustc.get_env(name).as_deref() == expected.as_deref().map(OsStr::new),
            "cached compiler environment changed: {name}"
        );
    }
    let pkg_root = crate::util::try_canonicalize(pkg_root)?;
    for file in dep_info.files.keys() {
        let file = cwd.join(file);
        if let Some(generated) = out_dir_input(&file, out_dirs) {
            anyhow::ensure!(
                generated.is_file(),
                "cached compiler input is missing from OUT_DIR: {}",
                generated.display()
            );
            continue;
        }
        let file = crate::util::try_canonicalize(&file)?;
        anyhow::ensure!(
            file.starts_with(&pkg_root),
            "cached compiler input is outside its immutable package: {}",
            file.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_recorded_compiler_environment() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        paths::write(&root.join("lib.rs"), b"").unwrap();
        let dep_info = root.join("artifact.d");
        paths::write(
            &dep_info,
            b"artifact: lib.rs\n# env-dep:CARGO_LOCAL_CACHE_TEST=one\n",
        )
        .unwrap();
        let mut rustc = ProcessBuilder::new("rustc");
        rustc.env("CARGO_LOCAL_CACHE_TEST", "one");
        assert!(validate_dep_info(&dep_info, &rustc, root, root, &[]).is_ok());
        rustc.env("CARGO_LOCAL_CACHE_TEST", "two");
        assert!(validate_dep_info(&dep_info, &rustc, root, root, &[]).is_err());
    }

    #[test]
    fn rejects_inputs_outside_immutable_package() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let package = root.join("package");
        paths::create_dir_all(&package).unwrap();
        paths::write(&root.join("external.rs"), b"").unwrap();
        let dep_info = package.join("artifact.d");
        paths::write(&dep_info, b"artifact: ../external.rs\n").unwrap();
        let rustc = ProcessBuilder::new("rustc");
        assert!(validate_dep_info(&dep_info, &rustc, &package, &package, &[]).is_err());
        paths::write(&dep_info, b"").unwrap();
        assert!(validate_dep_info(&dep_info, &rustc, &package, &package, &[]).is_err());
    }

    #[test]
    fn maps_generated_inputs_to_this_builds_out_dir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let package = root.join("package");
        paths::create_dir_all(&package.join("src")).unwrap();
        paths::write(&package.join("src/lib.rs"), b"").unwrap();
        let out_dir = root.join("this/build/scripted/0123456789abcdef/out");
        paths::create_dir_all(&out_dir).unwrap();
        paths::write(&out_dir.join("generated.rs"), b"").unwrap();
        let dep_info = package.join("artifact.d");
        let rustc = ProcessBuilder::new("rustc");
        let out_dirs = [out_dir.clone()];
        // The publisher's build directory differs, but the run unit's suffix does not.
        let recorded = "/other/build/scripted/0123456789abcdef/out/generated.rs";
        paths::write(&dep_info, format!("artifact: src/lib.rs {recorded}\n")).unwrap();
        assert!(validate_dep_info(&dep_info, &rustc, &package, &package, &out_dirs).is_ok());
        assert!(validate_dep_info(&dep_info, &rustc, &package, &package, &[]).is_err());
        for recorded in [
            "/other/build/scripted/fedcba9876543210/out/generated.rs",
            "/other/build/scripted/0123456789abcdef/out/missing.rs",
            "/other/build/scripted/0123456789abcdef/out/../../escape.rs",
        ] {
            paths::write(&dep_info, format!("artifact: src/lib.rs {recorded}\n")).unwrap();
            assert!(
                validate_dep_info(&dep_info, &rustc, &package, &package, &out_dirs).is_err(),
                "{recorded}"
            );
        }
    }
}
