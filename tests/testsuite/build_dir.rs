//! Tests for `build.build-dir` config property.
use std::path::PathBuf;

use cargo_test_support::prelude::*;
use cargo_test_support::project;

#[cargo_test]
fn binary_with_debug() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            target-dir = "target"
            build-dir = "build"
            "#,
        )
        .build();

    p.cargo("build -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert_build_dir(p.root().join("build"), "debug", true);
    assert_build_dir(p.root().join("target"), "debug", false);

    // Verify the binary was copied to the `target` dir
    assert!(p.root().join("target/debug/foo").is_file());
}

#[cargo_test]
fn binary_with_release() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            target-dir = "target"
            build-dir = "build"
            "#,
        )
        .build();

    p.cargo("build -Z unstable-options -Z build-dir --release")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert_build_dir(p.root().join("build"), "release", true);
    assert_build_dir(p.root().join("target"), "release", false);

    // Verify the binary was copied to the `target` dir
    assert!(p.root().join("target/release/foo").is_file());
}

#[cargo_test]
fn should_default_to_target() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .build();

    p.cargo("build -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert_build_dir(p.root().join("target"), "debug", true);
    // Verify the binary exists in the correct location
    assert!(p.root().join("target/debug/foo").is_file());
}

#[cargo_test]
fn cargo_doc_should_output_to_target_dir() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            target-dir = "target"
            build-dir = "build"
            "#,
        )
        .build();

    p.cargo("doc -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    let docs_dir = p.root().join("target/doc");
    assert!(docs_dir.exists());
    assert!(docs_dir.join("foo/index.html").exists());
}

#[cargo_test]
fn cargo_package_should_output_to_target_dir() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            target-dir = "target"
            build-dir = "build"
            "#,
        )
        .build();

    p.cargo("package -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert_build_dir(p.root().join("build"), "debug", true);

    let package_build_dir = p.root().join("build/package");
    assert!(package_build_dir.exists());
    assert!(package_build_dir.join("foo-0.0.1").exists());
    assert!(package_build_dir.join("foo-0.0.1").is_dir());
    let package_artifact_dir = p.root().join("target/package");
    assert!(package_artifact_dir.exists());
    assert!(package_artifact_dir.join("foo-0.0.1.crate").exists());
    assert!(package_artifact_dir.join("foo-0.0.1.crate").is_file());
}

#[cargo_test]
fn cargo_clean_should_clean_the_target_dir_and_build_dir() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            target-dir = "target"
            build-dir = "build"
            "#,
        )
        .build();

    p.cargo("build -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert_build_dir(p.root().join("build"), "debug", true);

    p.cargo("clean -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert!(!p.root().join("build").exists());
    assert!(!p.root().join("target").exists());
}

#[cargo_test]
fn verify_build_dir_is_disabled_by_feature_flag() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .file(
            ".cargo/config.toml",
            r#"
            [build]
            build-dir = "build"
            "#,
        )
        .build();

    p.cargo("build")
        .masquerade_as_nightly_cargo(&[])
        .enable_mac_dsym()
        .run();

    assert_build_dir(p.root().join("target"), "debug", true);
    assert!(p.root().join("target/debug/foo").is_file());
    assert!(!p.root().join("build").exists());
}

#[track_caller]
fn assert_build_dir(path: PathBuf, profile: &str, is_build_dir: bool) {
    println!("checking if {path:?} is a build directory ({is_build_dir})");
    // For things that are in both `target` and the build directory we only check if they are
    // present if `is_build_dir` is true.
    if is_build_dir {
        assert_eq!(is_build_dir, path.join(profile).is_dir());
    }

    let error_message = |dir: &str| {
        if is_build_dir {
            format!("`{dir}` dir was expected but not found")
        } else {
            format!("`{dir}` dir was not expected but was found")
        }
    };

    assert_eq!(
        is_build_dir,
        path.join(profile).join("deps").is_dir(),
        "{}",
        error_message("deps")
    );
    assert_eq!(
        is_build_dir,
        path.join(profile).join("build").is_dir(),
        "{}",
        error_message("build")
    );
    assert_eq!(
        is_build_dir,
        path.join(profile).join("incremental").is_dir(),
        "{}",
        error_message("incremental")
    );
}
