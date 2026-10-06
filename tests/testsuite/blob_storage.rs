use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::prelude::*;
use cargo_test_support::paths::ReadOnly;
use cargo_test_support::registry::Package;
use cargo_test_support::{Project, paths, prelude::*, project, t};

type Digest = [u8; 32];
const UNIT_OUTPUT_MAGIC: &[u8] = b"cargo-shared-storage-unit-output-v1\0";
const SNAPSHOT_MAGIC: &[u8] = b"cargo-shared-storage-snapshot-v1\0";

fn blob_root() -> PathBuf {
    paths::cargo_home().join("blobs")
}

fn shared_storage() -> PathBuf {
    paths::cargo_home().join("shared-storage")
}

fn object_path(kind: &str, id: &Digest) -> PathBuf {
    shared_storage()
        .join(kind)
        .join(blake3::Hash::from(*id).to_hex().as_str())
}

fn snapshot_project() -> Project {
    Package::new("common", "0.1.0")
        .file("src/lib.rs", "pub fn value() -> u32 { 10 }")
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
            "fn main() { println!(\"{}\", common::value() + variant::value()); }",
        )
        .build()
}

fn build_snapshot(p: &Project, feature: &str) {
    p.cargo("build")
        .args(&["--features", feature, "-Zshared-blob-storage"])
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
}

fn build_fresh_snapshot(p: &Project, feature: &str) {
    let output = p
        .cargo("build --message-format=json")
        .args(&["--features", feature, "-Zshared-blob-storage"])
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    let artifacts: BTreeMap<_, _> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            (value["reason"] == "compiler-artifact").then(|| {
                (
                    value["target"]["name"].as_str().unwrap().to_owned(),
                    value["fresh"].as_bool().unwrap(),
                )
            })
        })
        .collect();
    assert_eq!(
        artifacts,
        ["app", "common", "variant"]
            .into_iter()
            .map(|name| (name.to_owned(), true))
            .collect()
    );
}

fn snapshot_ids() -> BTreeSet<Digest> {
    let dir = shared_storage().join("snapshots");
    if !dir.exists() {
        return BTreeSet::new();
    }
    t!(fs::read_dir(dir))
        .map(|entry| {
            let entry = t!(entry);
            let bytes = t!(fs::read(entry.path()));
            let hash = blake3::hash(&bytes);
            assert_eq!(entry.file_name().to_str().unwrap(), hash.to_hex().as_str());
            *hash.as_bytes()
        })
        .collect()
}

fn take_u64(bytes: &mut &[u8]) -> u64 {
    let (value, rest) = bytes.split_at(8);
    *bytes = rest;
    u64::from_le_bytes(value.try_into().unwrap())
}

fn take_digest(bytes: &mut &[u8]) -> Digest {
    let (value, rest) = bytes.split_at(32);
    *bytes = rest;
    value.try_into().unwrap()
}

fn object_bytes(kind: &str, id: &Digest) -> Vec<u8> {
    let bytes = t!(fs::read(object_path(kind, id)));
    assert_eq!(blake3::hash(&bytes).as_bytes(), id);
    bytes
}

fn snapshot_unit_outputs(id: &Digest) -> BTreeSet<Digest> {
    let data = object_bytes("snapshots", id);
    let mut bytes = data.strip_prefix(SNAPSHOT_MAGIC).unwrap();
    let count = take_u64(&mut bytes);
    let unit_outputs = (0..count).map(|_| take_digest(&mut bytes)).collect();
    assert!(bytes.is_empty());
    unit_outputs
}

fn snapshot_blobs(id: &Digest) -> BTreeSet<PathBuf> {
    let mut blobs = BTreeSet::new();
    for unit_output in snapshot_unit_outputs(id) {
        let data = object_bytes("unit-output", &unit_output);
        let mut bytes = data.strip_prefix(UNIT_OUTPUT_MAGIC).unwrap();
        assert!(matches!(bytes[0], 1..=3));
        bytes = &bytes[1..];
        for _ in 0..take_u64(&mut bytes) {
            let path_len = take_u64(&mut bytes) as usize;
            bytes = &bytes[path_len..];
            let hash = take_digest(&mut bytes);
            let size = take_u64(&mut bytes);
            let path = blob_root().join(blake3::Hash::from(hash).to_hex().as_str());
            let contents = t!(fs::read(&path));
            assert_eq!(contents.len() as u64, size);
            assert_eq!(blake3::hash(&contents).as_bytes(), &hash);
            blobs.insert(path);
        }
        assert!(bytes.is_empty());
    }
    blobs
}

fn workspace_histories() -> Vec<PathBuf> {
    t!(fs::read_dir(shared_storage().join("workspace-history")))
        .map(|entry| t!(entry).path())
        .collect()
}

fn history_file(id: &Digest) -> PathBuf {
    let histories = workspace_histories();
    assert_eq!(histories.len(), 1);
    histories[0].join(blake3::Hash::from(*id).to_hex().as_str())
}

fn usage_time(id: &Digest) -> u64 {
    t!(fs::read_to_string(history_file(id)))
        .trim()
        .parse()
        .unwrap()
}

fn backdate_snapshot(id: &Digest, timestamp: u64) {
    t!(fs::write(history_file(id), format!("{timestamp}\n")));
}

fn cache_files() -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !dir.exists() {
            return;
        }
        for entry in t!(fs::read_dir(dir)) {
            let path = t!(entry).path();
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    t!(path.strip_prefix(root)).to_path_buf(),
                    t!(fs::read(path)),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    collect(&paths::cargo_home(), &blob_root(), &mut files);
    collect(&paths::cargo_home(), &shared_storage(), &mut files);
    files
}

fn clean_blobs(p: &Project, max_size: &str, dry_run: bool) {
    let mut cmd = p.cargo("clean gc");
    cmd.args(&["--max-blob-size", max_size, "-Zgc", "-Zshared-blob-storage"])
        .masquerade_as_nightly_cargo(&["gc", "shared-blob-storage"]);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.run();
}

fn fingerprint_file(p: &Project, name: &str) -> PathBuf {
    let files: Vec<_> = p
        .glob(&format!("target/**/lib-{name}"))
        .map(|path| t!(path))
        .collect();
    assert_eq!(files.len(), 1);
    files.into_iter().next().unwrap()
}

fn unit_output_hash(path: &Path) -> Digest {
    let data = t!(fs::read_to_string(path));
    let (_, hash) = data.split_once("\nunit-output-v1 ").unwrap();
    *t!(blake3::Hash::from_hex(hash)).as_bytes()
}

fn compiled(stderr: &[u8], name: &str) -> bool {
    std::str::from_utf8(stderr)
        .unwrap()
        .lines()
        .any(|line| line.contains("Running") && line.contains(&format!("--crate-name {name} ")))
}

#[cargo_test]
fn disabled_without_unstable_flag() {
    let p = snapshot_project();
    p.cargo("build --features foo").run();
    assert!(!blob_root().exists());
    assert!(!shared_storage().exists());
}

#[cargo_test]
fn stores_dependency_artifact_by_content_hash() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let artifact = t!(p
        .glob("target/debug/build/common/*/out/libcommon-*.rlib")
        .next()
        .unwrap());
    let contents = t!(fs::read(artifact));
    let blob = blob_root().join(blake3::hash(&contents).to_hex().as_str());
    assert_eq!(t!(fs::read(&blob)), contents);
    let id = snapshot_ids().pop_first().unwrap();
    assert!(snapshot_blobs(&id).contains(&blob));
    assert!(
        snapshot_unit_outputs(&id).contains(&unit_output_hash(&fingerprint_file(&p, "common")))
    );
    let local = t!(fs::read(p.bin("app")));
    assert!(
        !blob_root()
            .join(blake3::hash(&local).to_hex().as_str())
            .exists()
    );
}

#[cargo_test]
fn readonly_cargo_home_still_works() {
    let p = snapshot_project();
    p.cargo("generate-lockfile").run();
    p.cargo("fetch --locked").run();
    let _readonly = ReadOnly::new(paths::cargo_home());
    build_snapshot(&p, "foo");
}

#[cargo_test]
fn identical_build_snapshot_refreshes_only_its_workspace_history() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    let blobs = snapshot_blobs(id);
    let before: BTreeMap<_, _> = cache_files()
        .into_iter()
        .filter(|(path, _)| !path.starts_with("shared-storage/workspace-history"))
        .collect();
    let now = t!(SystemTime::now().duration_since(UNIX_EPOCH)).as_secs();
    backdate_snapshot(id, now - 60);
    build_fresh_snapshot(&p, "foo");
    assert_eq!(snapshot_ids(), ids);
    assert!(usage_time(id) >= now);
    assert_eq!(snapshot_blobs(id), blobs);
    let after: BTreeMap<_, _> = cache_files()
        .into_iter()
        .filter(|(path, _)| !path.starts_with("shared-storage/workspace-history"))
        .collect();
    assert_eq!(after, before);
}

#[cargo_test]
fn feature_snapshots_retain_shared_blobs_and_recapture_expired_outputs() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let foo_id = snapshot_ids().pop_first().unwrap();
    let foo_blobs = snapshot_blobs(&foo_id);
    build_snapshot(&p, "bar");
    let bar_id = *snapshot_ids().iter().find(|id| **id != foo_id).unwrap();
    let bar_blobs = snapshot_blobs(&bar_id);
    assert!(foo_blobs.intersection(&bar_blobs).next().is_some());
    assert!(foo_blobs.difference(&bar_blobs).next().is_some());
    backdate_snapshot(&foo_id, 1);
    let bar_used = usage_time(&bar_id);
    build_fresh_snapshot(&p, "foo");
    assert!(usage_time(&foo_id) > 1);
    assert_eq!(usage_time(&bar_id), bar_used);
    backdate_snapshot(&foo_id, 1);
    clean_blobs(&p, "1GiB", false);
    assert_eq!(snapshot_ids(), BTreeSet::from([bar_id]));
    for blob in foo_blobs.difference(&bar_blobs) {
        assert!(!blob.exists());
    }
    assert_eq!(snapshot_blobs(&bar_id), bar_blobs);
    build_fresh_snapshot(&p, "bar");
    p.process(&p.bin("app")).with_stdout_data("12\n").run();
    build_fresh_snapshot(&p, "foo");
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
    assert_eq!(snapshot_blobs(&foo_id), foo_blobs);
}

#[cargo_test]
fn pressure_gc_preserves_build_outputs_and_dry_run_preserves_storage() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    let artifacts: Vec<_> = p
        .glob("target/debug/build/*/*/out/*.rlib")
        .map(|path| {
            let path = t!(path);
            let bytes = t!(fs::read(&path));
            (path, bytes)
        })
        .collect();
    let before = cache_files();
    clean_blobs(&p, "0", true);
    assert_eq!(cache_files(), before);
    clean_blobs(&p, "0", false);
    for blob in &blobs {
        assert!(!blob.exists());
    }
    assert_eq!(
        t!(fs::read_dir(shared_storage().join("cache-entries"))).count(),
        0
    );
    for (path, bytes) in artifacts {
        assert_eq!(t!(fs::read(path)), bytes);
    }
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
    build_fresh_snapshot(&p, "foo");
    assert_eq!(snapshot_ids(), BTreeSet::from([id]));
    assert_eq!(snapshot_blobs(&id), blobs);
}

fn changing_project() -> Project {
    Package::new("changing", "0.1.0")
        .file(
            "src/lib.rs",
            "pub fn value() -> &'static str { env!(\"BLOB_STORAGE_TEST_VALUE\") }",
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
        changing = "0.1.0"
    "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", changing::value()); }",
        )
        .build()
}

#[cargo_test]
fn disabled_rebuild_preserves_blobs_and_reenabled_tracking_captures_new_outputs() {
    let p = changing_project();
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("original\n")
        .run();
    let id = snapshot_ids().pop_first().unwrap();
    let blobs: BTreeMap<_, _> = snapshot_blobs(&id)
        .into_iter()
        .map(|path| {
            let bytes = t!(fs::read(&path));
            (path, bytes)
        })
        .collect();
    p.cargo("run")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .with_stdout_data("replacement\n")
        .run();
    for (path, bytes) in blobs {
        assert_eq!(t!(fs::read(path)), bytes);
    }
    let fingerprint = fingerprint_file(&p, "changing");
    assert!(!t!(fs::read_to_string(&fingerprint)).contains("unit-output"));
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    let ids = snapshot_ids();
    assert_eq!(ids.len(), 2);
    let new_id = ids.iter().find(|other| **other != id).unwrap();
    assert!(snapshot_unit_outputs(new_id).contains(&unit_output_hash(&fingerprint)));
    snapshot_blobs(new_id);
}

#[cargo_test]
fn snapshots_without_workspace_history_do_not_retain_blobs() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    t!(fs::remove_dir_all(
        shared_storage().join("workspace-history")
    ));
    clean_blobs(&p, "1GiB", false);
    assert!(snapshot_ids().is_empty());
    for blob in blobs {
        assert!(!blob.exists());
    }
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn workspace_history_is_independent_of_build_directory() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let first = workspace_histories();
    p.cargo("build --features foo --target-dir other-target -Zshared-blob-storage")
        .args(&["--config", "build.build-dir=\"other-build\""])
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    assert_eq!(workspace_histories(), first);
    let expected = blake3::hash(
        t!(fs::canonicalize(p.root()))
            .as_os_str()
            .as_encoded_bytes(),
    );
    assert_eq!(
        first[0].file_name().unwrap().to_str().unwrap(),
        expected.to_hex().as_str()
    );
    for id in snapshot_ids() {
        assert!(usage_time(&id) > 1);
    }
}

#[cargo_test]
fn different_workspaces_keep_independent_history() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let first_id = snapshot_ids().pop_first().unwrap();
    let first_history = history_file(&first_id);
    let first_blobs = snapshot_blobs(&first_id);
    t!(fs::write(&first_history, "1\n"));
    let second = project()
        .at("second")
        .file(
            "Cargo.toml",
            &t!(fs::read_to_string(p.root().join("Cargo.toml"))),
        )
        .file(
            "src/main.rs",
            &t!(fs::read_to_string(p.root().join("src/main.rs"))),
        )
        .build();
    build_snapshot(&second, "foo");
    assert_eq!(workspace_histories().len(), 2);
    assert_eq!(t!(fs::read_to_string(&first_history)), "1\n");
    let second_history = workspace_histories()
        .into_iter()
        .find(|path| *path != first_history.parent().unwrap())
        .unwrap();
    let second_ids: BTreeSet<_> = t!(fs::read_dir(&second_history))
        .map(|entry| {
            *t!(blake3::Hash::from_hex(
                t!(entry).file_name().to_str().unwrap()
            ))
            .as_bytes()
        })
        .collect();
    let second_blobs: BTreeSet<_> = second_ids.iter().flat_map(snapshot_blobs).collect();
    assert!(first_blobs.intersection(&second_blobs).next().is_some());
    clean_blobs(&p, "1GiB", false);
    assert_eq!(snapshot_ids(), second_ids);
    for id in &second_ids {
        snapshot_blobs(id);
    }
    for entry in t!(fs::read_dir(second_history)) {
        t!(fs::write(t!(entry).path(), "1\n"));
    }
    clean_blobs(&p, "1GiB", false);
    for blob in first_blobs.union(&second_blobs) {
        assert!(!blob.exists());
    }
}

fn incomplete_unit_output_preserves_other_snapshot(damage: &str) {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let foo_id = snapshot_ids().pop_first().unwrap();
    let foo_blobs = snapshot_blobs(&foo_id);
    let foo_outputs = snapshot_unit_outputs(&foo_id);
    build_snapshot(&p, "bar");
    let bar_id = *snapshot_ids().iter().find(|id| **id != foo_id).unwrap();
    let bar_blobs = snapshot_blobs(&bar_id);
    let bar_outputs = snapshot_unit_outputs(&bar_id);
    let broken = foo_outputs.difference(&bar_outputs).next().unwrap();
    let path = object_path("unit-output", broken);
    match damage {
        "missing" => t!(fs::remove_file(&path)),
        "truncated" => {
            let bytes = t!(fs::read(&path));
            t!(fs::write(&path, &bytes[..bytes.len() / 2]));
        }
        "wrong-hash" => {
            let mut bytes = t!(fs::read(&path));
            *bytes.last_mut().unwrap() ^= 1;
            t!(fs::write(&path, bytes));
        }
        _ => unreachable!(),
    }
    clean_blobs(&p, "1GiB", false);
    assert_eq!(snapshot_ids(), BTreeSet::from([bar_id]));
    assert_eq!(snapshot_blobs(&bar_id), bar_blobs);
    for blob in foo_blobs.difference(&bar_blobs) {
        assert!(!blob.exists());
    }
    build_fresh_snapshot(&p, "foo");
    assert_eq!(snapshot_blobs(&foo_id), foo_blobs);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn missing_unit_output_preserves_shared_blobs() {
    incomplete_unit_output_preserves_other_snapshot("missing");
}
#[cargo_test]
fn truncated_unit_output_preserves_shared_blobs() {
    incomplete_unit_output_preserves_other_snapshot("truncated");
}
#[cargo_test]
fn unit_output_hash_mismatch_preserves_shared_blobs() {
    incomplete_unit_output_preserves_other_snapshot("wrong-hash");
}

#[cargo_test]
fn fresh_outputs_repopulate_deleted_storage() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    t!(fs::remove_dir_all(blob_root()));
    t!(fs::remove_dir_all(shared_storage()));
    build_fresh_snapshot(&p, "foo");
    assert_eq!(snapshot_ids(), BTreeSet::from([id]));
    assert_eq!(snapshot_blobs(&id), blobs);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn malformed_optional_hash_recaptures_without_recompiling() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let fingerprint = fingerprint_file(&p, "common");
    let hash = unit_output_hash(&fingerprint);
    let record = t!(fs::read_to_string(&fingerprint));
    let base = record.split_once('\n').unwrap().0;
    t!(fs::write(
        &fingerprint,
        format!("{base}\nunit-output-v1 invalid")
    ));
    t!(fs::remove_file(object_path("unit-output", &hash)));
    build_fresh_snapshot(&p, "foo");
    assert_eq!(unit_output_hash(&fingerprint), hash);
    snapshot_blobs(&id);
}

#[cargo_test]
fn failed_build_does_not_refresh_workspace_history() {
    let p = changing_project();
    p.cargo("build -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    backdate_snapshot(id, 1);
    p.change_file(
        "src/main.rs",
        "compile_error!(\"build must fail\"); fn main() {}",
    );
    p.cargo("build -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_status(101)
        .with_stderr_contains("[ERROR] build must fail")
        .run();
    assert_eq!(snapshot_ids(), ids);
    assert_eq!(usage_time(id), 1);
    let completed = unit_output_hash(&fingerprint_file(&p, "changing"));
    assert!(object_path("unit-output", &completed).is_file());
    clean_blobs(&p, "0", false);
    assert!(!object_path("unit-output", &completed).exists());
    p.change_file(
        "src/main.rs",
        "fn main() { println!(\"{}\", changing::value()); }",
    );
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    assert!(snapshot_unit_outputs(snapshot_ids().first().unwrap()).contains(&completed));
}

fn restores_immutable_dependencies(mode: &str) {
    let p = snapshot_project();
    let first = p
        .cargo(&format!("{mode} -vv --features foo -Zshared-blob-storage"))
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    assert!(compiled(&first.stderr, "common"));
    assert!(compiled(&first.stderr, "variant"));
    p.cargo("clean").run();
    let restored = p
        .cargo(&format!("{mode} -vv --features foo -Zshared-blob-storage"))
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    assert!(
        !compiled(&restored.stderr, "common"),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(
        !compiled(&restored.stderr, "variant"),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(compiled(&restored.stderr, "app"));
    let fresh = p
        .cargo(&format!("{mode} -vv --features foo -Zshared-blob-storage"))
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    for name in ["common", "variant", "app"] {
        assert!(!compiled(&fresh.stderr, name));
    }
    if mode == "build" {
        p.process(&p.bin("app")).with_stdout_data("11\n").run();
    }
}

#[cargo_test]
fn build_cache_restores_after_clean() {
    restores_immutable_dependencies("build");
}
#[cargo_test]
fn check_cache_restores_after_clean() {
    restores_immutable_dependencies("check");
}

#[cargo_test]
fn changed_environment_rejects_cached_outputs() {
    let p = changing_project();
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("original\n")
        .run();
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    assert!(compiled(&output.stderr, "changing"));
}

#[cargo_test]
fn corrupt_cache_blob_falls_back_to_compilation() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let artifact = t!(p
        .glob("target/debug/build/common/*/out/libcommon-*.rlib")
        .next()
        .unwrap());
    let original = t!(fs::read(&artifact));
    let blob = blob_root().join(blake3::hash(&original).to_hex().as_str());
    let mut wrong = original.clone();
    wrong[0] ^= 1;
    let replacement = blob_root().join("replacement");
    t!(fs::write(&replacement, wrong));
    t!(fs::rename(replacement, &blob));
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv --features foo -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("11\n")
        .run();
    assert!(compiled(&output.stderr, "common"));
    assert_eq!(t!(fs::read(blob)), original);
}

#[cargo_test]
fn changed_features_do_not_restore_another_unit() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv --features bar -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("12\n")
        .run();
    assert!(
        !compiled(&output.stderr, "common"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(compiled(&output.stderr, "variant"));
}

#[cargo_test]
fn changed_environment_invalidates_cached_transitive_consumers() {
    Package::new("leaf", "0.1.0")
        .file(
            "src/lib.rs",
            "pub const VALUE: &str = env!(\"BLOB_STORAGE_TEST_VALUE\");",
        )
        .publish();
    Package::new("middle", "0.1.0")
        .edition("2021")
        .dep("leaf", "0.1.0")
        .file(
            "src/lib.rs",
            "pub fn value() -> &'static str { leaf::VALUE }",
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
        middle = "0.1.0"
    "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", middle::value()); }",
        )
        .build();
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("original\n")
        .run();
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    assert!(compiled(&output.stderr, "leaf"));
    assert!(compiled(&output.stderr, "middle"));
}

#[cargo_test]
fn build_scripts_proc_macros_and_their_consumers_are_not_restored() {
    Package::new("scripted", "0.1.0")
        .file(
            "build.rs",
            r#"fn main() { println!("cargo:rustc-env=SCRIPT_VALUE=42"); }"#,
        )
        .file(
            "src/lib.rs",
            r#"pub const VALUE: &str = env!("SCRIPT_VALUE");"#,
        )
        .publish();
    Package::new("macro_dep", "0.1.0")
        .proc_macro(true)
        .file(
            "src/lib.rs",
            r#"
            extern crate proc_macro;
            #[proc_macro]
            pub fn value(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
                "42".parse().unwrap()
            }
        "#,
        )
        .publish();
    Package::new("consumer", "0.1.0")
        .edition("2021")
        .dep("scripted", "0.1.0")
        .dep("macro_dep", "0.1.0")
        .file(
            "src/lib.rs",
            r#"
            pub fn value() -> String {
                format!("{} {}", scripted::VALUE, macro_dep::value!())
            }
        "#,
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
        consumer = "0.1.0"
    "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", consumer::value()); }",
        )
        .build();
    p.cargo("run -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("42 42\n")
        .run();
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_contains("42 42")
        .run();
    for name in ["scripted", "macro_dep", "consumer", "app"] {
        assert!(
            compiled(&output.stderr, name),
            "unexpected cache hit for {name}"
        );
    }
}

#[cargo_test]
fn corrupt_workspace_history_stops_collection_before_deleting_blobs() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    t!(fs::write(history_file(&id), "not a timestamp\n"));
    let before = cache_files();
    p.cargo("clean gc --max-blob-size 0 -Zgc -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["gc", "shared-blob-storage"])
        .with_status(101)
        .with_stderr_contains("[ERROR] invalid workspace history timestamp: [..]")
        .run();
    assert_eq!(cache_files(), before);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn changed_rustflags_do_not_restore_stale_outputs() {
    Package::new("flagged", "0.1.0")
        .file(
            "src/lib.rs",
            "pub fn value() -> bool { cfg!(debug_assertions) }",
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
        flagged = "0.1.0"
    "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", flagged::value()); }",
        )
        .build();
    p.cargo("run -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("true\n")
        .run();
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv -Zshared-blob-storage")
        .env("RUSTFLAGS", "-Cdebug-assertions=no")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("false\n")
        .run();
    assert!(compiled(&output.stderr, "flagged"));
}
