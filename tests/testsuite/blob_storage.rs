use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::prelude::*;
use cargo_test_support::paths::ReadOnly;
use cargo_test_support::registry::Package;
use cargo_test_support::{Project, paths, prelude::*, project, str, t};

#[cargo_test]
fn disabled_without_unstable_flag() {
    Package::new("bar", "0.1.0").publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2015"

                [dependencies]
                bar = "0.1.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("build").run();

    assert!(!paths::home().join(".cargo/blobs").exists());
}

#[cargo_test]
fn stores_dependency_artifact_by_content_hash() {
    Package::new("bar", "0.1.0").publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2015"

                [dependencies]
                bar = "0.1.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("build")
        .arg("-Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();

    let artifact = p
        .glob("target/debug/build/bar/*/out/libbar-*.rlib")
        .next()
        .unwrap()
        .unwrap();
    let contents = t!(fs::read(artifact));
    let hash = blake3::hash(&contents).to_hex();
    let blob = paths::home().join(".cargo/blobs").join(hash.as_str());
    assert_eq!(t!(fs::read(blob)), contents);
}

#[cargo_test]
fn readonly_cargo_home_still_works() {
    Package::new("bar", "0.1.0").publish();

    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2015"

                [dependencies]
                bar = "0.1.0"
            "#,
        )
        .file("src/lib.rs", "")
        .build();

    p.cargo("generate-lockfile").run();
    p.cargo("fetch --locked").run();

    let _readonly = ReadOnly::new(paths::cargo_home());
    p.cargo("build")
        .arg("-Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
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
    assert_fresh_artifacts(&output.stdout);
}

fn assert_fresh_artifacts(stdout: &[u8]) {
    let artifacts: BTreeMap<_, _> = std::str::from_utf8(stdout)
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

fn fingerprint_file(p: &Project, name: &str) -> PathBuf {
    let files: Vec<_> = p
        .glob(&format!("target/**/lib-{name}"))
        .map(|path| t!(path))
        .collect();
    assert_eq!(files.len(), 1);
    files.into_iter().next().unwrap()
}

fn fingerprint_pointer(path: &Path) -> (Digest, Digest) {
    let data = t!(fs::read_to_string(path));
    let (base, pointer) = data.split_once('\n').unwrap();
    assert_eq!(base.len(), 16);
    let fields: Vec<_> = pointer.split(' ').collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], "blob-v1");
    (
        *t!(blake3::Hash::from_hex(fields[1])).as_bytes(),
        *t!(blake3::Hash::from_hex(fields[2])).as_bytes(),
    )
}

fn cache_revision(root: &Path) -> Digest {
    let data = t!(fs::read(root.join("local-v2/revision")));
    data.strip_prefix(REVISION_MAGIC)
        .unwrap()
        .try_into()
        .unwrap()
}

type Digest = [u8; 32];

const SNAPSHOT_MAGIC: &[u8] = b"cargo-shared-blob-snapshot-v1\0";
const UNIT_MAGIC: &[u8] = b"cargo-shared-blob-unit-result-v2\0";
const STATE_MAGIC: &[u8] = b"cargo-shared-blob-state-v2\0";
const REVISION_MAGIC: &[u8] = b"cargo-shared-blob-revision-v1\0";

fn blob_root() -> PathBuf {
    paths::cargo_home().join("blobs")
}

fn object_path(root: &Path, kind: &str, id: &Digest) -> PathBuf {
    root.join(kind)
        .join(blake3::Hash::from(*id).to_hex().as_str())
}

fn object_bytes(root: &Path, kind: &str, id: &Digest) -> Vec<u8> {
    let bytes = t!(fs::read(object_path(root, kind, id)));
    assert_eq!(blake3::hash(&bytes).as_bytes(), id);
    bytes
}

fn snapshot_ids_at(root: &Path) -> BTreeSet<Digest> {
    let dir = root.join("snapshots-v1");
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

fn snapshot_ids() -> BTreeSet<Digest> {
    snapshot_ids_at(&blob_root())
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

fn snapshot_units(root: &Path, id: &Digest) -> BTreeSet<Digest> {
    let data = object_bytes(root, "snapshots-v1", id);
    let mut bytes = data.strip_prefix(SNAPSHOT_MAGIC).unwrap();
    let count = take_u64(&mut bytes);
    let units = (0..count).map(|_| take_digest(&mut bytes)).collect();
    assert!(bytes.is_empty());
    units
}

fn snapshot_blobs(id: &Digest) -> BTreeSet<PathBuf> {
    snapshot_blobs_at(&blob_root(), id)
}

fn snapshot_blobs_at(root: &Path, id: &Digest) -> BTreeSet<PathBuf> {
    let mut blobs = BTreeSet::new();
    for unit in snapshot_units(root, id) {
        let data = object_bytes(root, "units-v1", &unit);
        let mut bytes = data.strip_prefix(UNIT_MAGIC).unwrap();
        assert!(matches!(bytes[0], 1..=3));
        bytes = &bytes[1..];
        for _ in 0..take_u64(&mut bytes) {
            let path_len = take_u64(&mut bytes) as usize;
            bytes = &bytes[path_len..];
            let hash = take_digest(&mut bytes);
            let size = take_u64(&mut bytes);
            let path = object_path(root, "", &hash);
            let contents = t!(fs::read(&path));
            assert_eq!(contents.len() as u64, size);
            assert_eq!(blake3::hash(&contents).as_bytes(), &hash);
            blobs.insert(path);
        }
        assert!(bytes.is_empty());
    }
    blobs
}

// Return byte offsets only: tests age local receipts without rewriting immutable
// manifests or maintaining their own implementation of the storage backend.
fn usage_offsets(data: &[u8]) -> Vec<(Digest, usize)> {
    let mut bytes = data.strip_prefix(STATE_MAGIC).unwrap();
    let mut offsets = Vec::new();
    for _ in 0..take_u64(&mut bytes) {
        let id = take_digest(&mut bytes);
        offsets.push((id, data.len() - bytes.len()));
        take_u64(&mut bytes);
    }
    assert!(bytes.is_empty());
    offsets
}

fn receipts(root: &Path) -> Vec<PathBuf> {
    t!(fs::read_dir(root.join("local-v2")))
        .map(|entry| t!(entry).path())
        .filter(|path| path.file_name().unwrap() != "revision")
        .collect()
}

fn receipt_usage(path: &Path, id: &Digest) -> u64 {
    let data = t!(fs::read(path));
    let offset = usage_offsets(&data)
        .into_iter()
        .find(|(snapshot, _)| snapshot == id)
        .unwrap()
        .1;
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn usage_time(id: &Digest) -> u64 {
    let receipts = receipts(&blob_root());
    assert_eq!(receipts.len(), 1);
    receipt_usage(&receipts[0], id)
}

fn backdate_receipt(path: &Path, id: &Digest, timestamp: u64) {
    let mut data = t!(fs::read(path));
    let offset = usage_offsets(&data)
        .into_iter()
        .find(|(snapshot, _)| snapshot == id)
        .unwrap()
        .1;
    data[offset..offset + 8].copy_from_slice(&timestamp.to_le_bytes());
    t!(fs::write(path, data));
}

fn backdate_snapshot(id: &Digest, timestamp: u64) {
    let receipts = receipts(&blob_root());
    assert_eq!(receipts.len(), 1);
    backdate_receipt(&receipts[0], id, timestamp);
}

fn cache_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
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
    collect(root, root, &mut files);
    files
}

fn clean_blobs(p: &Project, max_size: &str, dry_run: bool) {
    clean_blobs_in(p, &paths::cargo_home(), max_size, dry_run);
}

fn clean_blobs_in(p: &Project, cargo_home: &Path, max_size: &str, dry_run: bool) {
    let mut cmd = p.cargo("clean gc");
    cmd.env("CARGO_HOME", cargo_home)
        .args(&["--max-blob-size", max_size, "-Zgc", "-Zshared-blob-storage"])
        .masquerade_as_nightly_cargo(&["gc", "shared-blob-storage"]);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.run();
}

#[cargo_test]
fn identical_graph_reuses_snapshot_and_throttles_usage() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let ids = snapshot_ids();
    assert_eq!(ids.len(), 1);
    let id = ids.first().unwrap();
    let blobs = snapshot_blobs(id);
    assert!(!blobs.is_empty());
    let now = t!(SystemTime::now().duration_since(UNIX_EPOCH)).as_secs();

    // A fresh invocation inside the four-hour window must not write a new
    // timestamp, even when it falls in a different wall-clock second.
    let recent = now - 60 * 60;
    backdate_snapshot(id, recent);
    let before = cache_files(&blob_root());
    for _ in 0..3 {
        build_snapshot(&p, "foo");
        assert_eq!(snapshot_ids(), ids);
        assert_eq!(usage_time(id), recent);
        assert_eq!(cache_files(&blob_root()), before);
    }

    backdate_snapshot(id, now - 5 * 60 * 60);
    build_snapshot(&p, "foo");
    assert_eq!(snapshot_ids(), ids);
    assert!(usage_time(id) >= now);
    assert_eq!(snapshot_blobs(id), blobs);
    for blob in blobs {
        let contents = t!(fs::read(&blob));
        assert_eq!(
            blob.file_name().unwrap().to_str().unwrap(),
            blake3::hash(&contents).to_hex().as_str()
        );
    }
}

#[cargo_test]
fn feature_graphs_retain_shared_blobs_when_one_expires() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let foo_id = snapshot_ids().pop_first().unwrap();
    let foo_blobs = snapshot_blobs(&foo_id);
    build_snapshot(&p, "bar");
    let both_ids = snapshot_ids();
    assert_eq!(both_ids.len(), 2);
    let bar_id = both_ids.iter().find(|id| **id != foo_id).unwrap();
    let bar_blobs = snapshot_blobs(bar_id);
    let shared: Vec<_> = foo_blobs.intersection(&bar_blobs).collect();
    let foo_only: Vec<_> = foo_blobs.difference(&bar_blobs).collect();
    assert!(!shared.is_empty(), "common dependency must be shared");
    assert!(!foo_only.is_empty(), "foo must have distinct artifacts");
    assert!(bar_blobs.difference(&foo_blobs).next().is_some());

    // Alternating features must reuse each graph, not replace the other one's
    // retention root or accumulate an invocation-specific snapshot.
    backdate_snapshot(&foo_id, 1);
    let bar_used = usage_time(bar_id);
    build_snapshot(&p, "foo");
    assert!(usage_time(&foo_id) > 1);
    assert_eq!(usage_time(bar_id), bar_used);
    build_snapshot(&p, "bar");
    assert_eq!(snapshot_ids(), both_ids);
    for blob in foo_blobs.union(&bar_blobs) {
        assert!(blob.is_file(), "missing retained blob: {blob:?}");
    }

    backdate_snapshot(&foo_id, 1);
    clean_blobs(&p, "1GiB", false);
    for blob in foo_only {
        assert!(!blob.exists(), "expired blob was retained: {blob:?}");
    }
    for blob in &bar_blobs {
        let contents = t!(fs::read(blob));
        assert_eq!(
            blob.file_name().unwrap().to_str().unwrap(),
            blake3::hash(&contents).to_hex().as_str()
        );
    }
    p.cargo("run --features bar -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("12\n")
        .run();
}

#[cargo_test]
fn pressure_gc_preserves_build_outputs_and_dry_run_preserves_cache() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs: Vec<_> = snapshot_blobs(&id)
        .into_iter()
        .map(|path| {
            let contents = t!(fs::read(&path));
            (path, contents)
        })
        .collect();
    assert!(!blobs.is_empty());
    let artifacts: Vec<_> = p
        .glob("target/debug/build/*/*/out/*.rlib")
        .map(|path| {
            let path = t!(path);
            let contents = t!(fs::read(&path));
            (path, contents)
        })
        .collect();
    assert!(!artifacts.is_empty());
    let executable = t!(fs::read(p.bin("app")));
    let before = cache_files(&blob_root());

    clean_blobs(&p, "0", true);
    assert_eq!(cache_files(&blob_root()), before);
    for (path, contents) in &blobs {
        assert_eq!(t!(fs::read(path)), *contents);
    }

    clean_blobs(&p, "0", false);
    for (path, _) in &blobs {
        assert!(!path.exists(), "pressure GC left a CAS name: {path:?}");
    }
    for (path, contents) in artifacts {
        assert_eq!(t!(fs::read(path)), contents);
    }
    assert_eq!(t!(fs::read(p.bin("app"))), executable);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
    // Fresh build outputs must repopulate every evicted inventory and blob.
    p.cargo("run --features foo -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("11\n")
        .run();
    assert_eq!(snapshot_ids(), BTreeSet::from([id]));
    assert_eq!(
        snapshot_blobs(&id),
        blobs.into_iter().map(|(path, _)| path).collect()
    );
}

#[cargo_test]
fn fresh_build_recaptures_outputs_replaced_without_blob_storage() {
    Package::new("changing", "0.1.0")
        .file(
            "src/lib.rs",
            "pub fn value() -> &'static str { env!(\"BLOB_STORAGE_TEST_VALUE\") }",
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
                changing = "0.1.0"
            "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", changing::value()); }",
        )
        .build();
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("original\n")
        .run();
    let original_ids = snapshot_ids();
    assert_eq!(original_ids.len(), 1);
    let artifact = p
        .glob("target/debug/build/changing/*/out/libchanging-*.rlib")
        .next()
        .unwrap()
        .unwrap();
    let original = t!(fs::read(&artifact));

    // Rebuild the same output path with tracking disabled.
    p.cargo("run")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .with_stdout_data("replacement\n")
        .run();
    let fingerprint = fingerprint_file(&p, "changing");
    let plain = t!(fs::read_to_string(&fingerprint));
    assert_eq!(plain.len(), 16, "disabled rebuild retained a blob pointer");
    let replacement = t!(fs::read(&artifact));
    assert_ne!(replacement, original);
    let replacement_blob = paths::cargo_home()
        .join("blobs")
        .join(blake3::hash(&replacement).to_hex().as_str());
    assert!(!replacement_blob.exists());

    // Re-enabling tracking must capture the replacement, not renew the old graph.
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s
[RUNNING] `target/debug/app[EXE]`

"#]])
        .run();
    assert_eq!(t!(fs::read(&replacement_blob)), replacement);
    let ids = snapshot_ids();
    let new_ids: Vec<_> = ids.difference(&original_ids).collect();
    assert_eq!(new_ids.len(), 1);
    assert!(snapshot_blobs(new_ids[0]).contains(&replacement_blob));

    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    assert_eq!(snapshot_ids(), ids);
}

#[cargo_test]
fn archive_restore_without_local_receipts_gets_one_expiring_lease() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    let blobs = snapshot_blobs(id);
    let source = blob_root();
    let restored_home = p.root().join("restored-cargo-home");
    let restored = restored_home.join("blobs");
    // Model an archive extraction: new files containing only immutable bytes,
    // with no hardlinks, preserved mtimes, or workspace-local receipts.
    for (path, contents) in cache_files(&source) {
        if path.starts_with("local-v2") {
            continue;
        }
        let destination = restored.join(path);
        t!(fs::create_dir_all(destination.parent().unwrap()));
        t!(fs::write(destination, contents));
    }
    t!(fs::remove_dir_all(&source));
    assert!(!restored.join("local-v2").exists());
    assert_eq!(snapshot_ids_at(&restored), ids);
    let immutable = cache_files(&restored);

    clean_blobs_in(&p, &restored_home, "1GiB", true);
    assert_eq!(cache_files(&restored), immutable);
    assert!(!restored.join("local-v2").exists());

    clean_blobs_in(&p, &restored_home, "1GiB", false);
    let imports = restored.join("local-v2/imports");
    assert!(receipt_usage(&imports, id) > 1);
    for blob in &blobs {
        let path = restored.join(blob.file_name().unwrap());
        let bytes = t!(fs::read(&path));
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            blake3::hash(&bytes).to_hex().as_str()
        );
    }
    let recent = t!(SystemTime::now().duration_since(UNIX_EPOCH)).as_secs() - 3600;
    backdate_receipt(&imports, id, recent);
    clean_blobs_in(&p, &restored_home, "1GiB", false);
    assert_eq!(receipt_usage(&imports, id), recent);
    assert_eq!(snapshot_ids_at(&restored), ids);

    backdate_receipt(&imports, id, 1);
    clean_blobs_in(&p, &restored_home, "1GiB", false);
    assert!(snapshot_ids_at(&restored).is_empty());
    for blob in &blobs {
        assert!(!restored.join(blob.file_name().unwrap()).exists());
    }
    // An expired import must not be rediscovered and granted a second lease.
    clean_blobs_in(&p, &restored_home, "1GiB", false);
    assert!(snapshot_ids_at(&restored).is_empty());
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn multiple_build_directories_retain_shared_blobs_until_last_usage_expires() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    let blobs = snapshot_blobs(id);
    let first_receipt = receipts(&blob_root()).pop().unwrap();
    backdate_receipt(&first_receipt, id, 1);

    p.cargo("build")
        .args(&[
            "--features",
            "foo",
            "--target-dir",
            "other-target",
            "--config",
            "build.build-dir=\"other-build\"",
            "-Zshared-blob-storage",
        ])
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    let local = receipts(&blob_root());
    assert_eq!(local.len(), 2);
    let second_receipt = local.iter().find(|path| **path != first_receipt).unwrap();
    assert_eq!(receipt_usage(&first_receipt, id), 1);
    let second_usage = usage_offsets(&t!(fs::read(second_receipt)));
    assert_eq!(second_usage.len(), 1);
    let second_id = second_usage[0].0;
    assert!(receipt_usage(second_receipt, &second_id) > 1);
    let second_blobs = snapshot_blobs(&second_id);
    assert!(
        blobs.intersection(&second_blobs).next().is_some(),
        "independent build directories must share dependency blobs"
    );

    clean_blobs(&p, "1GiB", false);
    assert_eq!(snapshot_ids(), BTreeSet::from([second_id]));
    assert_eq!(snapshot_blobs(&second_id), second_blobs);
    for blob in blobs.difference(&second_blobs) {
        assert!(!blob.exists(), "expired build directory retained {blob:?}");
    }
    backdate_receipt(second_receipt, &second_id, 1);
    clean_blobs(&p, "1GiB", false);
    assert!(snapshot_ids().is_empty());
    for blob in blobs.union(&second_blobs) {
        assert!(
            !blob.exists(),
            "last usage expired but blob remains: {blob:?}"
        );
    }
}

fn incomplete_graph_preserves_other_snapshot(damage: &str) {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let foo_id = snapshot_ids().pop_first().unwrap();
    let foo_blobs = snapshot_blobs(&foo_id);
    let foo_units = snapshot_units(&blob_root(), &foo_id);
    build_snapshot(&p, "bar");
    let bar_id = *snapshot_ids().iter().find(|id| **id != foo_id).unwrap();
    let bar_blobs = snapshot_blobs(&bar_id);
    let bar_units = snapshot_units(&blob_root(), &bar_id);
    assert!(foo_blobs.intersection(&bar_blobs).next().is_some());
    let broken_unit = foo_units.difference(&bar_units).next().unwrap();
    let path = object_path(&blob_root(), "units-v1", broken_unit);
    match damage {
        "missing" => t!(fs::remove_file(&path)),
        "truncated" => {
            let bytes = t!(fs::read(&path));
            t!(fs::write(&path, &bytes[..bytes.len() / 2]));
        }
        "wrong-hash" => {
            let mut bytes = t!(fs::read(&path));
            // Keep framing and every blob reference intact. Only digest
            // verification can distinguish this from a complete live graph.
            let encoding = bytes[UNIT_MAGIC.len()];
            let mut outputs = &bytes[UNIT_MAGIC.len() + 1..];
            assert!(take_u64(&mut outputs) > 0);
            let path_len = take_u64(&mut outputs) as usize;
            let last_character =
                bytes.len() - outputs.len() + path_len - if encoding == 2 { 2 } else { 1 };
            bytes[last_character] ^= 1;
            t!(fs::write(&path, bytes));
        }
        _ => unreachable!(),
    }
    clean_blobs(&p, "1GiB", false);
    assert_eq!(snapshot_ids(), BTreeSet::from([bar_id]));
    assert_eq!(snapshot_blobs(&bar_id), bar_blobs);
    for blob in foo_blobs.difference(&bar_blobs) {
        assert!(!blob.exists(), "incomplete graph retained blob: {blob:?}");
    }
    p.cargo("run --features bar -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("12\n")
        .run();
    build_fresh_snapshot(&p, "foo");
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
    assert_eq!(snapshot_ids(), BTreeSet::from([foo_id, bar_id]));
    assert_eq!(snapshot_blobs(&foo_id), foo_blobs);
    assert_eq!(snapshot_blobs(&bar_id), bar_blobs);
}

#[cargo_test]
fn missing_manifest_does_not_collect_blobs_shared_with_complete_graph() {
    incomplete_graph_preserves_other_snapshot("missing");
}

#[cargo_test]
fn truncated_manifest_does_not_collect_blobs_shared_with_complete_graph() {
    incomplete_graph_preserves_other_snapshot("truncated");
}

#[cargo_test]
fn manifest_hash_mismatch_does_not_collect_blobs_shared_with_complete_graph() {
    incomplete_graph_preserves_other_snapshot("wrong-hash");
}

#[cargo_test]
fn corrupt_restored_blob_is_repaired_without_replacing_good_build_output() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let artifact = p
        .glob("target/debug/build/common/*/out/libcommon-*.rlib")
        .next()
        .unwrap()
        .unwrap();
    let original = t!(fs::read(&artifact));
    let blob = blob_root().join(blake3::hash(&original).to_hex().as_str());
    assert_eq!(t!(fs::read(&blob)), original);
    let mut wrong = original.clone();
    wrong[0] ^= 1;
    // Never modify a potentially hardlinked compiler output in place.
    let replacement = blob_root().join("corrupt-replacement");
    t!(fs::write(&replacement, &wrong));
    t!(fs::rename(replacement, &blob));
    assert_eq!(t!(fs::read(&artifact)), original);
    assert_eq!(t!(fs::read(&blob)), wrong);
    // Missing inventory forces capture; metadata-only checks do not hash blobs.
    let id = snapshot_ids().pop_first().unwrap();
    for unit in snapshot_units(&blob_root(), &id) {
        t!(fs::remove_file(object_path(
            &blob_root(),
            "units-v1",
            &unit
        )));
    }
    t!(fs::remove_dir_all(blob_root().join("local-v2")));

    build_snapshot(&p, "foo");
    assert_eq!(t!(fs::read(&artifact)), original);
    assert_eq!(t!(fs::read(&blob)), original);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn dormant_feature_pointer_recovers_after_another_feature_validates_revision() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let foo_id = snapshot_ids().pop_first().unwrap();
    let foo_blobs = snapshot_blobs(&foo_id);
    let foo_fingerprint = fingerprint_file(&p, "variant");
    let foo_pointer = fingerprint_pointer(&foo_fingerprint);
    let common_fingerprint = fingerprint_file(&p, "common");
    let common_pointer = fingerprint_pointer(&common_fingerprint);
    let common_inventory = object_path(&blob_root(), "units-v1", &common_pointer.1);
    let common_modified = t!(t!(fs::metadata(&common_inventory)).modified());
    assert_eq!(foo_pointer.0, cache_revision(&blob_root()));
    assert!(snapshot_units(&blob_root(), &foo_id).contains(&foo_pointer.1));

    build_snapshot(&p, "bar");
    let bar_id = *snapshot_ids().iter().find(|id| **id != foo_id).unwrap();
    let bar_blobs = snapshot_blobs(&bar_id);
    backdate_snapshot(&foo_id, 1);
    clean_blobs(&p, "1GiB", false);
    let revision = cache_revision(&blob_root());
    assert_ne!(revision, foo_pointer.0);
    assert!(!object_path(&blob_root(), "units-v1", &foo_pointer.1).exists());

    build_fresh_snapshot(&p, "bar");
    p.process(&p.bin("app")).with_stdout_data("12\n").run();
    assert_eq!(
        fingerprint_pointer(&common_fingerprint),
        (revision, common_pointer.1)
    );
    assert_eq!(
        t!(t!(fs::metadata(&common_inventory)).modified()),
        common_modified
    );
    assert_eq!(fingerprint_pointer(&foo_fingerprint), foo_pointer);
    assert_eq!(snapshot_blobs(&bar_id), bar_blobs);

    build_fresh_snapshot(&p, "foo");
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
    assert_eq!(
        fingerprint_pointer(&foo_fingerprint),
        (revision, foo_pointer.1)
    );
    assert_eq!(snapshot_ids(), BTreeSet::from([foo_id, bar_id]));
    assert_eq!(snapshot_blobs(&foo_id), foo_blobs);
    assert_eq!(snapshot_blobs(&bar_id), bar_blobs);
}

fn fresh_pointer_recovers_missing_cache(remove_store: bool) {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    let fingerprint = fingerprint_file(&p, "common");
    let pointer = fingerprint_pointer(&fingerprint);
    if remove_store {
        t!(fs::remove_dir_all(blob_root()));
    } else {
        t!(fs::remove_dir_all(blob_root().join("local-v2")));
        // An old pointer must not hide missing data in an otherwise intact store.
        t!(fs::remove_file(object_path(
            &blob_root(),
            "units-v1",
            &pointer.1
        )));
    }
    build_fresh_snapshot(&p, "foo");
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
    let revision = cache_revision(&blob_root());
    assert_ne!(revision, pointer.0);
    assert_eq!(fingerprint_pointer(&fingerprint), (revision, pointer.1));
    assert_eq!(snapshot_ids(), BTreeSet::from([id]));
    assert_eq!(snapshot_blobs(&id), blobs);
}

#[cargo_test]
fn fresh_pointers_recover_after_store_deletion() {
    fresh_pointer_recovers_missing_cache(true);
}

#[cargo_test]
fn fresh_pointers_recover_after_local_metadata_recreation() {
    fresh_pointer_recovers_missing_cache(false);
}

#[cargo_test]
fn missing_receipt_validates_pointers_even_with_matching_revision() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    let fingerprint = fingerprint_file(&p, "common");
    let pointer = fingerprint_pointer(&fingerprint);
    for receipt in receipts(&blob_root()) {
        t!(fs::remove_file(receipt));
    }
    t!(fs::remove_file(object_path(
        &blob_root(),
        "units-v1",
        &pointer.1
    )));

    build_fresh_snapshot(&p, "foo");
    assert_eq!(fingerprint_pointer(&fingerprint), pointer);
    assert_eq!(snapshot_blobs(&id), blobs);
    assert!(usage_time(&id) > 1);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn new_cargo_home_does_not_trust_old_fingerprint_pointers() {
    let p = snapshot_project();
    p.cargo("vendor --respect-source-config vendor").run();
    t!(fs::create_dir_all(p.root().join(".cargo")));
    p.change_file(
        ".cargo/config.toml",
        r#"
            [source.crates-io]
            replace-with = "vendored"
            [source.vendored]
            directory = "vendor"
        "#,
    );
    build_snapshot(&p, "foo");
    let old_revision = cache_revision(&blob_root());
    let new_home = p.root().join("new-cargo-home");
    // Keep source paths fixed so changing Cargo home does not require rebuilding.
    t!(fs::create_dir_all(&new_home));
    let fingerprint = fingerprint_file(&p, "common");
    let output = p
        .cargo("build --features foo -Zshared-blob-storage --message-format=json")
        .env("CARGO_HOME", &new_home)
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();
    assert_fresh_artifacts(&output.stdout);
    let new_root = new_home.join("blobs");
    let pointer = fingerprint_pointer(&fingerprint);
    assert_ne!(pointer.0, old_revision);
    assert_eq!(pointer.0, cache_revision(&new_root));
    let ids = snapshot_ids_at(&new_root);
    assert_eq!(ids.len(), 1);
    assert!(snapshot_units(&new_root, ids.first().unwrap()).contains(&pointer.1));
    snapshot_blobs_at(&new_root, ids.first().unwrap());
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn failed_build_does_not_refresh_snapshot_and_recovers_completed_units_after_gc() {
    Package::new("changing", "0.1.0")
        .file(
            "src/lib.rs",
            "pub fn value() -> &'static str { env!(\"BLOB_STORAGE_TEST_VALUE\") }",
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
                changing = "0.1.0"
            "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", changing::value()); }",
        )
        .build();
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "original")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("original\n")
        .run();
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    backdate_snapshot(id, 1);
    let fingerprint = fingerprint_file(&p, "changing");
    let original_pointer = fingerprint_pointer(&fingerprint);
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
    let completed_pointer = fingerprint_pointer(&fingerprint);
    assert_ne!(completed_pointer.1, original_pointer.1);
    assert!(object_path(&blob_root(), "units-v1", &completed_pointer.1).is_file());
    assert_eq!(snapshot_ids(), ids);
    assert_eq!(usage_time(id), 1);

    clean_blobs(&p, "0", false);
    assert!(!object_path(&blob_root(), "units-v1", &completed_pointer.1).exists());
    p.change_file(
        "src/main.rs",
        "fn main() { println!(\"{}\", changing::value()); }",
    );
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    let recovered = fingerprint_pointer(&fingerprint);
    assert_eq!(
        recovered,
        (cache_revision(&blob_root()), completed_pointer.1)
    );
    let ids = snapshot_ids();
    assert_eq!(ids.len(), 1);
    let id = ids.first().unwrap();
    assert!(snapshot_units(&blob_root(), id).contains(&completed_pointer.1));
    snapshot_blobs(id);
}

#[cargo_test]
fn malformed_optional_pointer_recaptures_without_recompiling() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    let fingerprint = fingerprint_file(&p, "common");
    let pointer = fingerprint_pointer(&fingerprint);
    let record = t!(fs::read_to_string(&fingerprint));
    let base = record.split_once('\n').unwrap().0;
    t!(fs::write(&fingerprint, format!("{base}\nblob-v1 invalid")));
    t!(fs::remove_file(object_path(
        &blob_root(),
        "units-v1",
        &pointer.1
    )));

    build_fresh_snapshot(&p, "foo");
    assert_eq!(fingerprint_pointer(&fingerprint), pointer);
    assert_eq!(snapshot_blobs(&id), blobs);
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}
