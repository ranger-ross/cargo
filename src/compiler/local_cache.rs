//! Local reuse of immutable compiler units, independent of fingerprint propagation.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context as _;
use cargo_util::{ProcessBuilder, paths};

use super::blob_storage::{
    BlobStorage, DependencyArtifact, Output, PrefetchSource, TrackedOutput, output_path_key,
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
        let out_dirs = build_runner
            .unit_deps(unit)
            .iter()
            .filter(|dep| dep.unit.mode.is_run_custom_build())
            .map(|dep| build_runner.files().out_dir_new_layout(&dep.unit))
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

    /// Whether a remote fetch for this unit is running ahead of its job.
    pub(super) fn prefetching(&self) -> bool {
        self.storage.prefetching(&self.unit_hash)
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
            hash_tree(&mut hasher, out_dir)?;
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

    pub(super) fn restore(&self) -> CargoResult<bool> {
        self.storage
            .restore_cache_entry(&self.unit_hash, self.key()?, &self.unit_dir)
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

    fn inputs_ready(&self) -> bool {
        if self.script_metadatas.is_empty() {
            return true;
        }
        let outputs = self.build_script_outputs.lock().unwrap();
        self.script_metadatas
            .iter()
            .all(|metadata| outputs.get(*metadata).is_some())
    }

    fn key_from(&self, dependency_hashes: &[[u8; 32]]) -> CargoResult<u64> {
        self.guard(dependency_hashes)
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
        && unit.target.is_lib()
        && !unit.target.proc_macro()
        && !unit.target.is_custom_build()
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

fn immutable(unit: &Unit) -> bool {
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

/// Hash relative paths and contents of every file under `root`.
fn hash_tree(hasher: &mut blake3::Hasher, root: &Path) -> CargoResult<()> {
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
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
            let mut contents = blake3::Hasher::new();
            contents.update_reader(paths::open(entry.path())?)?;
            hasher.update(contents.finalize().as_bytes());
        } else {
            put(hasher, b"dir");
        }
    }
    Ok(())
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
