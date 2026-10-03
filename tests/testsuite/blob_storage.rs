use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
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

fn snapshot_db() -> rusqlite::Connection {
    t!(rusqlite::Connection::open(
        paths::cargo_home().join("blobs/index.sqlite")
    ))
}

fn snapshot_ids() -> BTreeSet<Vec<u8>> {
    let db = snapshot_db();
    let mut stmt = t!(db.prepare("SELECT id FROM snapshot"));
    t!(t!(stmt.query_map([], |row| row.get(0))).collect())
}

fn snapshot_blobs(id: &[u8]) -> BTreeSet<PathBuf> {
    let db = snapshot_db();
    let mut stmt = t!(db.prepare(
        "SELECT DISTINCT unit_output.blob_hash FROM snapshot_member
         JOIN unit_output ON unit_output.result_id = snapshot_member.result_id
         WHERE snapshot_member.snapshot_id = ?1"
    ));
    let hashes = t!(stmt.query_map([id], |row| row.get::<_, Vec<u8>>(0)));
    hashes
        .map(|hash| {
            let digest: [u8; 32] = t!(t!(hash).as_slice().try_into());
            paths::cargo_home()
                .join("blobs")
                .join(blake3::Hash::from(digest).to_hex().as_str())
        })
        .collect()
}

fn usage_time(id: &[u8]) -> u64 {
    t!(snapshot_db().query_row(
        "SELECT last_used FROM snapshot_usage WHERE snapshot_id = ?1",
        [id],
        |row| row.get(0)
    ))
}

fn backdate_snapshot(id: &[u8], timestamp: u64) {
    assert_eq!(
        t!(snapshot_db().execute(
            "UPDATE snapshot_usage SET last_used = ?1 WHERE snapshot_id = ?2",
            rusqlite::params![timestamp, id],
        )),
        1
    );
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
    for _ in 0..3 {
        build_snapshot(&p, "foo");
        assert_eq!(snapshot_ids(), ids);
        assert_eq!(usage_time(id), recent);
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
    let index = paths::cargo_home().join("blobs/index.sqlite");
    let before = t!(fs::read(&index));

    clean_blobs(&p, "0", true);
    assert_eq!(t!(fs::read(&index)), before);
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
    // Existing build-dir outputs must remain usable by Cargo too, including
    // recovery of the index mapping removed by pressure GC.
    p.cargo("run --features foo -Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .with_stdout_data("11\n")
        .run();
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

    // Rebuild the same unit slot while index tracking is disabled. env! embeds
    // different bytes and changes the fingerprint without changing the unit's
    // output path, unlike toggling a feature or changing RUSTFLAGS.
    p.cargo("clean -p changing").run();
    p.cargo("run")
        .env("BLOB_STORAGE_TEST_VALUE", "replacement")
        .with_stdout_data("replacement\n")
        .run();
    let replacement = t!(fs::read(&artifact));
    assert_ne!(replacement, original);
    let replacement_blob = paths::cargo_home()
        .join("blobs")
        .join(blake3::hash(&replacement).to_hex().as_str());
    assert!(!replacement_blob.exists());

    // Everything is fresh now, but the stored mapping describes the old
    // generation. Re-enabling tracking must capture the replacement, not
    // renew the old graph merely because the unit output path still matches.
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
