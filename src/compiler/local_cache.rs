//! Local reuse of immutable compiler units, independent of fingerprint propagation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use cargo_util::{ProcessBuilder, paths};

use super::blob_storage::{BlobStorage, UnitOutputHash};
use super::fingerprint::{self, Fingerprint};
use super::{BuildRunner, CompileMode, Executor, FileFlavor, Unit};
use crate::util::CargoResult;

pub(super) struct LocalCache {
    storage: Arc<BlobStorage>,
    unit_hash: String,
    unit_dir: PathBuf,
    fingerprint: Arc<Fingerprint>,
    dependencies: Vec<PathBuf>,
    key: OnceLock<u64>,
    restored: OnceLock<UnitOutputHash>,
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
            for output in build_runner.outputs(&dep.unit)?.iter() {
                // Match the artifacts supplied by extern_args. Metadata is available
                // before a pipelined dependency has finished producing its rlib.
                if (output.flavor == FileFlavor::Rmeta && (metadata_only || no_embed_metadata))
                    || (output.flavor == FileFlavor::Linkable && !metadata_only)
                {
                    dependencies.push(output.path.clone());
                }
            }
        }
        dependencies.sort();
        dependencies.dedup();
        Ok(Some(Arc::new(Self {
            storage,
            unit_hash: build_runner.files().unit_hash(unit),
            unit_dir: build_runner.files().build_unit_dir(unit),
            fingerprint: Arc::clone(&build_runner.fingerprints[unit]),
            dependencies,
            key: OnceLock::new(),
            restored: OnceLock::new(),
        })))
    }

    fn key(&self) -> CargoResult<u64> {
        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.fingerprint.hash_u64().to_le_bytes());
        for dependency in &self.dependencies {
            let mut dependency_hash = blake3::Hasher::new();
            dependency_hash.update_reader(paths::open(dependency)?)?;
            hasher.update(dependency_hash.finalize().as_bytes());
        }
        let key = u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().unwrap());
        let _ = self.key.set(key);
        Ok(key)
    }

    pub(super) fn restore(&self) -> CargoResult<Option<UnitOutputHash>> {
        self.storage
            .restore_cache_entry(&self.unit_hash, self.key()?, &self.unit_dir)
    }

    pub(super) fn accept(&self, unit_output: UnitOutputHash) -> CargoResult<()> {
        anyhow::ensure!(
            self.storage.prepare_unit(Some(unit_output))? == Some(unit_output),
            "restored unit output disappeared from shared storage"
        );
        let _ = self.restored.set(unit_output);
        Ok(())
    }

    pub(super) fn restored(&self) -> Option<UnitOutputHash> {
        self.restored.get().copied()
    }

    pub(super) fn publish(&self, unit_output: UnitOutputHash) -> CargoResult<()> {
        self.storage
            .publish_cache_entry(&self.unit_hash, self.key()?, unit_output, &self.unit_dir)
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
    let source = unit.pkg.package_id().source_id();
    let mut eligible = !unit.is_local()
        && (source.is_registry() || source.is_git())
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
        eligible = dependencies
            .iter()
            .all(|dep| cacheable_unit(build_runner, &dep.unit, exec));
    }
    build_runner
        .local_cache_eligible
        .insert(unit.clone(), eligible);
    eligible
}

/// Validate inputs not included in Cargo's structural fingerprint. In particular,
/// env! and option_env! are discovered only in rustc's dep-info, which is absent
/// when restoring into a clean build directory.
pub(super) fn validate_dep_info(
    dep_info: &Path,
    rustc: &ProcessBuilder,
    cwd: &Path,
    pkg_root: &Path,
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
        let file = crate::util::try_canonicalize(&cwd.join(file))?;
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
        assert!(validate_dep_info(&dep_info, &rustc, root, root).is_ok());
        rustc.env("CARGO_LOCAL_CACHE_TEST", "two");
        assert!(validate_dep_info(&dep_info, &rustc, root, root).is_err());
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
        assert!(validate_dep_info(&dep_info, &rustc, &package, &package).is_err());
        paths::write(&dep_info, b"").unwrap();
        assert!(validate_dep_info(&dep_info, &rustc, &package, &package).is_err());
    }
}
