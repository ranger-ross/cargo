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
const SNAPSHOT_MAGIC: &[u8] = b"cargo-shared-storage-snapshot-v2\0";
const CACHE_ENTRY_MAGIC: &[u8] = b"cargo-shared-storage-cache-entry-v3\0";

/// A snapshot member. Cacheable units are tracked by cache entry and other
/// non-local units by unit output.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Member {
    UnitOutput(Digest),
    CacheEntry(String),
}

fn shared_storage() -> PathBuf {
    paths::cargo_home().join("shared-storage")
}

fn blob_root() -> PathBuf {
    shared_storage().join("blobs")
}

fn blob_path(hash: &Digest) -> PathBuf {
    let hex = blake3::Hash::from(*hash).to_hex();
    blob_root().join(&hex[..2]).join(&hex[2..])
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
    // A build script makes this package deduplicated but not cacheable.
    Package::new("scripted", "0.1.0")
        .file(
            "build.rs",
            r#"fn main() { println!("cargo:rustc-env=SCRIPTED_ZERO=0"); }"#,
        )
        .file(
            "src/lib.rs",
            r#"pub fn zero() -> u32 { env!("SCRIPTED_ZERO").parse().unwrap() }"#,
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
                scripted = "0.1.0"
                [features]
                foo = ["variant/foo"]
                bar = ["variant/bar"]
            "#,
        )
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    println!("{}", common::value() + variant::value() + scripted::zero());
                }
            "#,
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
        ["app", "build-script-build", "common", "scripted", "variant"]
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
        .map(|entry| t!(entry))
        .filter(|entry| t!(entry.file_type()).is_file())
        .map(|entry| {
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

fn snapshot_members(id: &Digest) -> BTreeSet<Member> {
    let data = object_bytes("snapshots", id);
    let mut bytes = data.strip_prefix(SNAPSHOT_MAGIC).unwrap();
    let mut members = BTreeSet::new();
    for _ in 0..take_u64(&mut bytes) {
        members.insert(Member::UnitOutput(take_digest(&mut bytes)));
    }
    for _ in 0..take_u64(&mut bytes) {
        let len = take_u64(&mut bytes) as usize;
        let (name, rest) = bytes.split_at(len);
        bytes = rest;
        members.insert(Member::CacheEntry(
            std::str::from_utf8(name).unwrap().to_owned(),
        ));
    }
    assert!(bytes.is_empty());
    members
}

fn member_path(member: &Member) -> PathBuf {
    match member {
        Member::UnitOutput(id) => object_path("unit-output", id),
        Member::CacheEntry(name) => shared_storage().join("cache-entries").join(name),
    }
}

/// Blob hashes and sizes referenced by a unit output or cache entry.
fn member_blobs(member: &Member) -> Vec<(Digest, u64)> {
    let (data, magic, header, trailer) = match member {
        Member::UnitOutput(id) => (object_bytes("unit-output", id), UNIT_OUTPUT_MAGIC, 0, 0),
        // Cache entries add an input guard and per-file mode and mtime.
        Member::CacheEntry(_) => (t!(fs::read(member_path(member))), CACHE_ENTRY_MAGIC, 8, 16),
    };
    let mut bytes = data.strip_prefix(magic).unwrap();
    assert!(matches!(bytes[0], 1..=3));
    bytes = &bytes[1 + header..];
    let mut blobs = Vec::new();
    for _ in 0..take_u64(&mut bytes) {
        let path_len = take_u64(&mut bytes) as usize;
        bytes = &bytes[path_len..];
        let hash = take_digest(&mut bytes);
        let size = take_u64(&mut bytes);
        bytes = &bytes[trailer..];
        blobs.push((hash, size));
    }
    assert!(bytes.is_empty());
    blobs
}

fn snapshot_blobs(id: &Digest) -> BTreeSet<PathBuf> {
    let mut blobs = BTreeSet::new();
    for member in snapshot_members(id) {
        for (hash, size) in member_blobs(&member) {
            let path = blob_path(&hash);
            let contents = t!(fs::read(&path));
            assert_eq!(contents.len() as u64, size);
            assert_eq!(blake3::hash(&contents).as_bytes(), &hash);
            blobs.insert(path);
        }
    }
    blobs
}

fn usage_file(id: &Digest) -> PathBuf {
    shared_storage()
        .join("snapshots/usage")
        .join(blake3::Hash::from(*id).to_hex().as_str())
}

fn usage_time(id: &Digest) -> u64 {
    t!(fs::read_to_string(usage_file(id)))
        .trim()
        .parse()
        .unwrap()
}

fn backdate_snapshot(id: &Digest, timestamp: u64) {
    t!(fs::write(usage_file(id), format!("{timestamp}\n")));
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

/// Cache entries are keyed by the unit hash, which names the unit directory.
fn cache_entry_member(fingerprint: &Path) -> Member {
    let unit_dir = fingerprint.parent().unwrap().parent().unwrap();
    Member::CacheEntry(unit_dir.file_name().unwrap().to_str().unwrap().to_owned())
}

/// The member a fingerprint points at.
fn tracked_member(fingerprint: &Path) -> Member {
    let data = t!(fs::read_to_string(fingerprint));
    let (_, pointer) = data.split_once('\n').unwrap();
    if pointer == "cache-entry-v1" {
        return cache_entry_member(fingerprint);
    }
    let hash = pointer.strip_prefix("unit-output-v1 ").unwrap();
    Member::UnitOutput(*t!(blake3::Hash::from_hex(hash)).as_bytes())
}

fn compiled(stderr: &[u8], name: &str) -> bool {
    std::str::from_utf8(stderr)
        .unwrap()
        .lines()
        .any(|line| line.contains("Running") && line.contains(&format!("--crate-name {name} ")))
}

/// Whether `-v` output shows a build script being executed.
fn ran_build_script(stderr: &[u8]) -> bool {
    std::str::from_utf8(stderr)
        .unwrap()
        .lines()
        .any(|line| line.contains("Running") && line.contains("build_script_build`"))
}

#[cargo_test]
fn disabled_without_unstable_flag() {
    let p = snapshot_project();
    p.cargo("build --features foo").run();
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
    let blob = blob_path(blake3::hash(&contents).as_bytes());
    assert_eq!(t!(fs::read(&blob)), contents);
    let id = snapshot_ids().pop_first().unwrap();
    assert!(snapshot_blobs(&id).contains(&blob));
    // Non-local units, including build scripts, their runs, and consumers of
    // their output, appear as cache entries.
    let members = snapshot_members(&id);
    let common = tracked_member(&fingerprint_file(&p, "common"));
    assert!(matches!(common, Member::CacheEntry(_)), "{common:?}");
    let scripted = tracked_member(&fingerprint_file(&p, "scripted"));
    assert!(matches!(scripted, Member::CacheEntry(_)), "{scripted:?}");
    assert!(members.contains(&common) && members.contains(&scripted));
    let entries = members
        .iter()
        .filter(|member| matches!(member, Member::CacheEntry(_)))
        .count();
    assert_eq!((entries, members.len() - entries), (5, 0), "{members:?}");
    let local = t!(fs::read(p.bin("app")));
    assert!(!blob_path(blake3::hash(&local).as_bytes()).exists());
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
fn identical_build_snapshot_refreshes_only_its_usage() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    let blobs = snapshot_blobs(id);
    let before: BTreeMap<_, _> = cache_files()
        .into_iter()
        .filter(|(path, _)| !path.starts_with("shared-storage/snapshots/usage"))
        .collect();
    let now = t!(SystemTime::now().duration_since(UNIX_EPOCH)).as_secs();
    backdate_snapshot(id, now - 60);
    build_fresh_snapshot(&p, "foo");
    assert_eq!(snapshot_ids(), ids);
    assert!(usage_time(id) >= now);
    assert_eq!(snapshot_blobs(id), blobs);
    let after: BTreeMap<_, _> = cache_files()
        .into_iter()
        .filter(|(path, _)| !path.starts_with("shared-storage/snapshots/usage"))
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
    let data = t!(fs::read_to_string(&fingerprint));
    assert!(!data.contains('\n'), "{data}");
    let entry = cache_entry_member(&fingerprint);
    let original = t!(fs::read(member_path(&entry)));
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    // The snapshot names the same cache entry, whose contents now describe the
    // recaptured outputs.
    assert_eq!(snapshot_ids(), BTreeSet::from([id]));
    assert_eq!(tracked_member(&fingerprint), entry);
    assert!(snapshot_members(&id).contains(&entry));
    assert_ne!(t!(fs::read(member_path(&entry))), original);
    snapshot_blobs(&id);
}

#[cargo_test]
fn snapshots_without_usage_do_not_retain_blobs() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
    t!(fs::remove_dir_all(shared_storage().join("snapshots/usage")));
    clean_blobs(&p, "1GiB", false);
    assert!(snapshot_ids().is_empty());
    for blob in blobs {
        assert!(!blob.exists());
    }
    p.process(&p.bin("app")).with_stdout_data("11\n").run();
}

#[cargo_test]
fn snapshot_usage_is_shared_across_workspaces() {
    // Every dependency is cacheable, so both workspaces track the same members.
    Package::new("common", "0.1.0")
        .file("src/lib.rs", "pub fn value() -> u32 { 10 }")
        .publish();
    let workspace = |name: &str| {
        project()
            .at(name)
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
                "src/main.rs",
                "fn main() { println!(\"{}\", common::value()); }",
            )
            .build()
    };
    let build = |p: &Project| {
        p.cargo("build -Zshared-blob-storage")
            .masquerade_as_nightly_cargo(&["shared-blob-storage"])
            .run();
    };
    build(&workspace("first"));
    let ids = snapshot_ids();
    let id = ids.first().unwrap();
    backdate_snapshot(id, 1);
    build(&workspace("second"));
    assert_eq!(snapshot_ids(), ids);
    assert!(usage_time(id) > 1);
}

fn incomplete_cache_entry_preserves_other_snapshot(damage: &str) {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let foo_id = snapshot_ids().pop_first().unwrap();
    let foo_blobs = snapshot_blobs(&foo_id);
    let foo_members = snapshot_members(&foo_id);
    build_snapshot(&p, "bar");
    let bar_id = *snapshot_ids().iter().find(|id| **id != foo_id).unwrap();
    let bar_blobs = snapshot_blobs(&bar_id);
    let bar_members = snapshot_members(&bar_id);
    // Only the `variant` cache entry differs between the feature sets.
    let broken: Vec<_> = foo_members.difference(&bar_members).collect();
    assert!(matches!(broken[..], [Member::CacheEntry(_)]), "{broken:?}");
    let path = member_path(broken[0]);
    match damage {
        "missing" => t!(fs::remove_file(&path)),
        "truncated" => {
            let bytes = t!(fs::read(&path));
            t!(fs::write(&path, &bytes[..bytes.len() / 2]));
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
fn missing_cache_entry_preserves_shared_blobs() {
    incomplete_cache_entry_preserves_other_snapshot("missing");
}
#[cargo_test]
fn truncated_cache_entry_preserves_shared_blobs() {
    incomplete_cache_entry_preserves_other_snapshot("truncated");
}

#[cargo_test]
fn cache_entry_replaced_by_another_workspace_is_not_recaptured() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let fingerprint = fingerprint_file(&p, "common");
    let entry = tracked_member(&fingerprint);
    let path = member_path(&entry);
    // Another workspace can publish the same unit hash with a different input
    // guard. Fresh builds keep using the entry rather than rewriting it.
    let mut replaced = t!(fs::read(&path));
    replaced[CACHE_ENTRY_MAGIC.len() + 1] ^= 1;
    t!(fs::write(&path, &replaced));
    build_fresh_snapshot(&p, "foo");
    assert_eq!(t!(fs::read(&path)), replaced);
    assert_eq!(tracked_member(&fingerprint), entry);
    assert_eq!(snapshot_ids(), BTreeSet::from([id]));
    assert!(snapshot_members(&id).contains(&entry));
}

#[cargo_test]
fn fresh_outputs_repopulate_deleted_storage() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    let blobs = snapshot_blobs(&id);
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
    let entry = tracked_member(&fingerprint);
    let original = t!(fs::read(member_path(&entry)));
    let record = t!(fs::read_to_string(&fingerprint));
    let base = record.split_once('\n').unwrap().0;
    t!(fs::write(
        &fingerprint,
        format!("{base}\ncache-entry-v1 invalid")
    ));
    t!(fs::remove_file(member_path(&entry)));
    build_fresh_snapshot(&p, "foo");
    assert_eq!(tracked_member(&fingerprint), entry);
    assert_eq!(t!(fs::read(member_path(&entry))), original);
    snapshot_blobs(&id);
}

#[cargo_test]
fn failed_build_does_not_refresh_snapshot_usage() {
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
    let completed = tracked_member(&fingerprint_file(&p, "changing"));
    assert!(member_path(&completed).is_file());
    clean_blobs(&p, "0", false);
    assert!(!member_path(&completed).exists());
    p.change_file(
        "src/main.rs",
        "fn main() { println!(\"{}\", changing::value()); }",
    );
    p.cargo("run -Zshared-blob-storage")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("replacement\n")
        .run();
    assert!(snapshot_members(snapshot_ids().first().unwrap()).contains(&completed));
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
    let blob = blob_path(blake3::hash(&original).as_bytes());
    let mut wrong = original.clone();
    wrong[0] ^= 1;
    let replacement = blob_root().join("replacement");
    t!(fs::write(&replacement, wrong));
    t!(fs::rename(replacement, &blob));
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv --features foo -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_contains("11")
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
        .with_stdout_contains("12")
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
fn build_scripts_and_proc_macros_are_restored() {
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
    // Build scripts, their runs, and proc-macros restore like their consumers.
    for (name, expected) in [
        ("build_script_build", false),
        ("macro_dep", false),
        ("scripted", false),
        ("consumer", false),
        ("app", true),
    ] {
        assert_eq!(compiled(&output.stderr, name), expected, "{name}");
    }
    assert!(
        !ran_build_script(&output.stderr),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[cargo_test]
fn build_script_runs_with_symlinks_are_restored() {
    Package::new("linked", "0.1.0")
        .file(
            "build.rs",
            r#"
            fn main() {
                let out = std::env::var("OUT_DIR").unwrap();
                std::fs::write(format!("{out}/value.rs"), "pub const VALUE: u32 = 5;").unwrap();
                let _ = std::fs::remove_file(format!("{out}/linked.rs"));
                std::os::unix::fs::symlink("value.rs", format!("{out}/linked.rs")).unwrap();
            }
        "#,
        )
        .file(
            "src/lib.rs",
            r#"include!(concat!(env!("OUT_DIR"), "/linked.rs"));"#,
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
        linked = "0.1.0"
    "#,
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{}\", linked::VALUE); }",
        )
        .build();
    p.cargo("run -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("5\n")
        .run();
    p.cargo("clean").run();
    let output = p
        .cargo("run -vv -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("5\n")
        .run();
    assert!(
        !ran_build_script(&output.stderr),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!compiled(&output.stderr, "linked"));
}

/// Build scripts whose output depends on declared environment variables. A
/// clean build restores their runs unless a declared value changed.
fn untracked_script_project() -> Project {
    Package::new("generated", "0.1.0")
        .file(
            "build.rs",
            r#"
            fn main() {
                println!("cargo::rerun-if-env-changed=GENERATED_VALUE");
                let value = std::env::var("GENERATED_VALUE").unwrap();
                let out = std::env::var("OUT_DIR").unwrap();
                std::fs::write(
                    format!("{out}/value.rs"),
                    format!("pub const VALUE: &str = {value:?};"),
                )
                .unwrap();
            }
        "#,
        )
        .file(
            "src/lib.rs",
            r#"include!(concat!(env!("OUT_DIR"), "/value.rs"));"#,
        )
        .publish();
    Package::new("configured", "0.1.0")
        .file(
            "build.rs",
            r#"
            fn main() {
                println!("cargo::rerun-if-env-changed=CONFIGURED_VALUE");
                println!("cargo::rustc-check-cfg=cfg(flavor, values(\"one\", \"two\"))");
                let value = std::env::var("CONFIGURED_VALUE").unwrap();
                println!("cargo::rustc-cfg=flavor=\"{value}\"");
            }
        "#,
        )
        .file(
            "src/lib.rs",
            r#"
            #[cfg(flavor = "one")]
            pub const VALUE: &str = "one";
            #[cfg(flavor = "two")]
            pub const VALUE: &str = "two";
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
        generated = "0.1.0"
        configured = "0.1.0"
    "#,
        )
        .file(
            "src/main.rs",
            r#"fn main() { println!("{} {}", generated::VALUE, configured::VALUE); }"#,
        )
        .build()
}

fn run_untracked(p: &Project, generated: &str, configured: &str) -> Vec<u8> {
    p.cargo("clean").run();
    p.cargo("run -vv -Zshared-blob-storage")
        .env("GENERATED_VALUE", generated)
        .env("CONFIGURED_VALUE", configured)
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_contains(format!("{generated} {configured}"))
        .run()
        .stderr
}

#[cargo_test]
fn generated_out_dir_inputs_are_part_of_the_cache_key() {
    let p = untracked_script_project();
    run_untracked(&p, "one", "one");
    let same = run_untracked(&p, "one", "one");
    assert!(!compiled(&same, "generated"));
    assert!(!ran_build_script(&same));
    // The changed value rejects the cached run, and the new `OUT_DIR` contents
    // change the consumer's key.
    let changed = run_untracked(&p, "two", "one");
    assert!(compiled(&changed, "generated"));
    assert!(!compiled(&changed, "configured"));
}

#[cargo_test]
fn build_script_cfgs_are_part_of_the_cache_key() {
    let p = untracked_script_project();
    run_untracked(&p, "one", "one");
    let changed = run_untracked(&p, "one", "two");
    assert!(compiled(&changed, "configured"));
    assert!(!compiled(&changed, "generated"));
}

#[cargo_test]
fn corrupt_snapshot_usage_stops_collection_before_deleting_blobs() {
    let p = snapshot_project();
    build_snapshot(&p, "foo");
    let id = snapshot_ids().pop_first().unwrap();
    t!(fs::write(usage_file(&id), "not a timestamp\n"));
    let before = cache_files();
    p.cargo("clean gc --max-blob-size 0 -Zgc -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["gc", "shared-blob-storage"])
        .with_status(101)
        .with_stderr_contains("[ERROR] invalid snapshot usage timestamp: [..]")
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
