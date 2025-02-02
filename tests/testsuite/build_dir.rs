//! Tests for `build.build-dir` config property.

use std::path::PathBuf;

use cargo_test_support::prelude::*;
use cargo_test_support::project;

#[cargo_test]
fn verify_feature_is_disabled_by_feature_flag() {
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

#[cargo_test]
fn binary_with_debug() {
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

mod should_template_build_dir_correctly {
    use cargo_test_support::paths;

    use super::*;

    #[cargo_test]
    fn workspace_root() {
        let p = project()
            .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
            .file(
                ".cargo/config.toml",
                r#"
            [build]
            build-dir = "{workspace-root}/build"
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
    fn cargo_cache() {
        let p = project()
            .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
            .file(
                ".cargo/config.toml",
                r#"
            [build]
            build-dir = "{cargo-cache}/build"
            "#,
            )
            .build();

        p.cargo("build -Z unstable-options -Z build-dir")
            .masquerade_as_nightly_cargo(&["build-dir"])
            .enable_mac_dsym()
            .run();

        assert_build_dir(paths::home().join(".cargo/build"), "debug", true);
        assert_build_dir(p.root().join("target"), "debug", false);

        // Verify the binary was copied to the `target` dir
        assert!(p.root().join("target/debug/foo").is_file());
    }

    #[cargo_test]
    fn workspace_manfiest_path_hash() {
        let p = project()
            .file("src/main.rs", r#"fn main() { println!("Hello, World!") }"#)
            .file(
                ".cargo/config.toml",
                r#"
            [build]
            build-dir = "foo/{workspace-manifest-path-hash}/build"
            "#,
            )
            .build();

        p.cargo("build -Z unstable-options -Z build-dir")
            .masquerade_as_nightly_cargo(&["build-dir"])
            .enable_mac_dsym()
            .run();

        let foo_dir = p.root().join("foo");
        assert!(foo_dir.exists());

        // Since the hash will change between test runs simply find the first directory in `foo`
        // and assume that is the build dir.
        let hash_dir = std::fs::read_dir(foo_dir)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .unwrap();

        let build_dir = hash_dir.path().join("build");
        assert!(build_dir.exists());

        assert_build_dir(build_dir, "debug", true);
        assert_build_dir(p.root().join("target"), "debug", false);

        // Verify the binary was copied to the `target` dir
        assert!(p.root().join("target/debug/foo").is_file());
    }
}

#[track_caller]
fn assert_build_dir(path: PathBuf, profile: &str, is_build_dir: bool) {
    println!("checking if {path:?} is a build directory ({is_build_dir})");
    // For things that are in both `target` and the build directory we only check if they are
    // present if `is_build_dir` is true.
    if is_build_dir {
        assert!(path.join("CACHEDIR.TAG").is_file());
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
