use std::fs;
use std::path::{Path, PathBuf};

use crate::prelude::*;
use cargo_test_support::paths::ReadOnly;
use cargo_test_support::registry::Package;
use cargo_test_support::{paths, prelude::*, project, str, t};

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

    assert!(!paths::home().join(".cargo/shared-storage").exists());
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
    let hash = hash.as_str();
    let blob = paths::home()
        .join(".cargo/shared-storage/blobs")
        .join(&hash[..2])
        .join(&hash[2..]);
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

#[cargo_test]
fn should_deduplicate_basic_rustc_artifacts() {
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

    let verify_files_deduplicated = || {
        for extension in ["rlib", "rmeta"] {
            let artifact = p
                .glob(&format!(
                    "target/debug/build/bar/*/out/libbar-*.{extension}"
                ))
                .next()
                .unwrap()
                .unwrap();
            let contents = t!(fs::read(&artifact));
            let hash = blake3::hash(&contents).to_hex();
            let blob = blob(hash.as_str());
            assert!(blob.is_file(), "missing blob: {}", blob.display());
            assert_eq!(t!(fs::read(&blob)), contents);
            assert_linked(&artifact, &blob);
        }
    };

    // Build and verify the files are deduplicated.
    // Then clean and reverify the files are deduplicated from the previous run.

    p.cargo("build")
        .env("__CARGO_TEST_SHARED_STORAGE_FORCE_HARDLINK", "1")
        .arg("-Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();

    verify_files_deduplicated();

    p.cargo("clean").run();

    p.cargo("build")
        .env("__CARGO_TEST_SHARED_STORAGE_FORCE_HARDLINK", "1")
        .arg("-Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();

    verify_files_deduplicated();
}

#[cargo_test]
fn should_save_build_snapshot() {
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
        .env("__CARGO_TEST_SHARED_STORAGE_FORCE_HARDLINK", "1")
        .arg("-Zshared-blob-storage")
        .masquerade_as_nightly_cargo(&["shared-blob-storage"])
        .run();

    shared_storage().assert_dir_layout(
        str![[r#"
[ROOT]/home/.cargo/shared-storage/blobs/[HASH]
[ROOT]/home/.cargo/shared-storage/blobs/[HASH]
[ROOT]/home/.cargo/shared-storage/blobs/[HASH]

"#]],
        &[],
    );
}

#[cargo_test]
fn should_not_rebuild_when_output_mtimes_are_older_than_dependencies() {
    Package::new("bar", "0.1.0")
        .file("src/lib.rs", "pub fn foo() {}")
        .file(
            "build.rs",
            r#"fn main() { println!("cargo::rerun-if-env-changed=FOO") }"#,
        )
        .publish();

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
        .file("src/main.rs", "extern crate bar; fn main() { bar::foo(); }")
        .build();

    let check = |foo: &str, stderr_data: Option<snapbox::data::Inline>| {
        let mut exec = p.cargo("check -v");
        exec.env("FOO", foo)
            .env("__CARGO_TEST_SHARED_STORAGE_FORCE_HARDLINK", "1")
            .arg("-Zshared-blob-storage")
            .masquerade_as_nightly_cargo(&["shared-blob-storage"]);

        if let Some(stderr_data) = stderr_data {
            exec.with_stderr_data(stderr_data);
        }

        exec.run();
    };

    // Check causing the artifacts to be linked into blob storage
    check("1", None);

    let past = filetime::FileTime::now().unix_seconds() - 3600;
    p.root().move_in_time(|_, _| (past, 0));
    p.build_dir().move_in_time(|_, _| (past, 0));

    // Check again and verify that we are fresh
    check(
        "1",
        Some(str![[r#"
[FRESH] bar v0.1.0
[FRESH] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]]),
    );

    // Check again, this time changing FOO which will cause an expected rebuild
    check("2", None);

    // Verify that we are fresh, even when outputs deduplicated from blob storage
    check(
        "2",
        Some(str![[r#"
[FRESH] bar v0.1.0
[FRESH] foo v0.1.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]]),
    );
}

fn shared_storage() -> PathBuf {
    paths::home().join(".cargo/shared-storage")
}

fn blob(hash: &str) -> PathBuf {
    shared_storage()
        .join("blobs")
        .join(&hash[..2])
        .join(&hash[2..])
}

#[track_caller]
fn assert_linked(a: impl AsRef<Path>, b: impl AsRef<Path>) {
    let a = a.as_ref();
    let b = b.as_ref();
    assert!(
        same_file::is_same_file(a, b).expect("failed to compare file identities"),
        "{} and {} are not hardlinked",
        a.display(),
        b.display()
    );
}
