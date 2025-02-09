//! Tests for `build.build-dir` config property.
use std::path::PathBuf;

use cargo_test_support::prelude::*;
use cargo_test_support::project;
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX, EXE_SUFFIX};

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

    assert_build_dir_layout(p.root().join("target"), "debug");
    assert_exists(&p.root().join(format!("target/debug/foo{EXE_SUFFIX}")));
    assert_exists(&p.root().join("target/debug/foo.d"));
    assert_not_exists(&p.root().join("build"));
}

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

    assert_build_dir_layout(p.root().join("build"), "debug");
    assert_artifact_dir_layout(p.root().join("target"), "debug");

    // Verify the binary was copied to the `target` dir
    assert_exists(&p.root().join(format!("target/debug/foo{EXE_SUFFIX}")));
    assert_exists(&p.root().join("target/debug/foo.d"));
}

#[cargo_test]
fn libs() {
    // https://doc.rust-lang.org/reference/linkage.html#r-link.staticlib
    let (staticlib_prefix, staticlib_suffix) =
        if cfg!(target_os = "windows") && cfg!(target_env = "msvc") {
            ("", ".lib")
        } else {
            ("lib", ".a")
        };

    // (crate-type, list of final artifacts)
    let lib_types = [
        ("lib", ["libfoo.rlib", "libfoo.d"]),
        (
            "dylib",
            [&format!("{DLL_PREFIX}foo{DLL_SUFFIX}"), "libfoo.d"],
        ),
        (
            "cdylib",
            [&format!("{DLL_PREFIX}foo{DLL_SUFFIX}"), "libfoo.d"],
        ),
        (
            "staticlib",
            [
                &format!("{staticlib_prefix}foo{staticlib_suffix}"),
                "libfoo.d",
            ],
        ),
    ];

    for (lib_type, expected_files) in lib_types {
        let p = project()
            .file("src/lib.rs", r#"fn foo() { println!("Hello, World!") }"#)
            .file(
                "Cargo.toml",
                &format!(
                    r#"
            [package]
            name = "foo"
            version = "0.0.1"
            authors = []
            edition = "2015"

            [lib]
            crate-type = ["{lib_type}"]
            "#
                ),
            )
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

        assert_build_dir_layout(p.root().join("build"), "debug");

        // Verify lib artifacts were copied into the artifact dir
        for expected_file in expected_files {
            assert_exists(&p.root().join(format!("target/debug/{expected_file}")));
        }
    }
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

    assert_build_dir_layout(p.root().join("build"), "release");
    assert_artifact_dir_layout(p.root().join("target"), "release");

    // Verify the binary was copied to the `target` dir
    assert_exists(&p.root().join(format!("target/release/foo{EXE_SUFFIX}")));
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

    assert_build_dir_layout(p.root().join("target"), "debug");
    assert_exists(&p.root().join(format!("target/debug/foo{EXE_SUFFIX}")));
}

#[cargo_test]
fn should_respect_env_var() {
    let p = project()
        .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
        .build();

    p.cargo("build -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .env("CARGO_BUILD_DIR", "build")
        .enable_mac_dsym()
        .run();

    assert_build_dir_layout(p.root().join("build"), "debug");
    assert_exists(&p.root().join(format!("target/debug/foo{EXE_SUFFIX}")));
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

    assert_exists(&docs_dir);
    assert_exists(&docs_dir.join("foo/index.html"));
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

    assert_build_dir_layout(p.root().join("build"), "debug");

    let package_dir = p.root().join("target/package");
    assert_exists(&package_dir);
    assert_exists(&package_dir.join("foo-0.0.1.crate"));
    assert!(package_dir.join("foo-0.0.1.crate").is_file());
    let package_dir = p.root().join("build/package");
    assert_exists(&package_dir);
    assert_exists(&package_dir.join("foo-0.0.1"));
    assert!(package_dir.join("foo-0.0.1").is_dir());
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

    assert_build_dir_layout(p.root().join("build"), "debug");

    p.cargo("clean -Z unstable-options -Z build-dir")
        .masquerade_as_nightly_cargo(&["build-dir"])
        .enable_mac_dsym()
        .run();

    assert_not_exists(&p.root().join("build"));
    assert_not_exists(&p.root().join("target"));
}

#[track_caller]
fn assert_build_dir_layout(path: PathBuf, profile: &str) {
    assert_dir_layout(path, profile, true);
}

#[allow(dead_code)]
#[track_caller]
fn assert_artifact_dir_layout(path: PathBuf, profile: &str) {
    assert_dir_layout(path, profile, false);
}

#[track_caller]
fn assert_dir_layout(path: PathBuf, profile: &str, is_build_dir: bool) {
    println!("checking if {path:?} is a build directory ({is_build_dir})");
    // For things that are in both `target` and the build directory we only check if they are
    // present if `is_build_dir` is true.
    if is_build_dir {
        assert_eq!(
            is_build_dir,
            path.join(profile).is_dir(),
            "Expected {:?} to exist and be a directory",
            path.join(profile)
        );
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

#[track_caller]
fn assert_exists(path: &PathBuf) {
    assert!(
        path.exists(),
        "Expected `{}` to exist but was not found.",
        path.display()
    );
}

#[track_caller]
fn assert_not_exists(path: &PathBuf) {
    assert!(
        !path.exists(),
        "Expected `{}` to NOT exist but was found.",
        path.display()
    );
}
