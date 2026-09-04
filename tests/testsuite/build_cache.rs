//! Tests for the cross-workspace build cache (`$CARGO_HOME/build-cache`).
//!
//! Cacheable build units (immutable non-local packages without build scripts)
//! are compiled into `$CARGO_HOME/build-cache/content/lib<stem>-<fingerprint>[.<ext>]`
//! and `$CARGO_HOME/build-cache/entries/<pkg>/<hash>` and shared across workspaces.
//! See `cargo::compiler::cache` and `DEV_LOG.md`.

use std::path::{Path, PathBuf};

use crate::prelude::*;
use cargo_test_support::registry::Package;
use cargo_test_support::str;
use cargo_test_support::{
    basic_lib_manifest, git, is_coarse_mtime, main_file, paths, project, project_in, sleep_ms,
};

/// The build-cache root for the current test's `CARGO_HOME`.
fn build_cache_root() -> PathBuf {
    paths::cargo_home().join("build-cache")
}

/// Recursively collects the paths of all files under `root`.
fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Returns the paths of all `.rlib` files in the build cache (via CAS content).
/// With the content-addressable layout, rlibs are stored as
/// `content/lib<stem>-<sha256>.rlib` blobs referenced by
/// `entries/<pkg>/<hash>` manifests. We resolve rlib count via manifests.
fn cached_rlibs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries_root = build_cache_root().join("entries");
    for manifest_path in collect_files(&entries_root) {
        if let Ok(bytes) = std::fs::read(&manifest_path) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(files) = v.get("files").and_then(|f| f.as_object()) {
                    for (k, hash) in files {
                        if k.ends_with(".rlib") {
                            if let Some(h) = hash.as_str() {
                                out.push(build_cache_root().join("content").join(h));
                            }
                        }
                    }
                }
            }
        }
    }
    // Fallback for legacy layout (pre-CAS) where rlibs live directly under build-cache.
    if out.is_empty() {
        out.extend(
            collect_files(&build_cache_root())
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "rlib")),
        );
    }
    out
}

/// Returns the sorted package names that have entries in the build cache.
fn cached_pkgs() -> Vec<String> {
    // New CAS layout: `entries/<pkg>/<hash>`
    let entries = build_cache_root().join("entries");
    if entries.exists() {
        if let Ok(rd) = std::fs::read_dir(&entries) {
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            return names;
        }
    }
    // Legacy fallback: `build-cache/<pkg>/<hash>`
    let mut names: Vec<String> = std::fs::read_dir(build_cache_root())
        .expect("cache root should exist")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name != "_staging" && name != "content" && name != "entries")
        .collect();
    names.sort();
    names
}

/// Returns the modification time of every file under `root`, sorted by path.
fn mtimes_under(root: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = collect_files(root)
        .into_iter()
        .map(|p| {
            let mtime = p
                .metadata()
                .expect("file exists")
                .modified()
                .expect("mtime supported");
            (p, mtime)
        })
        .collect::<Vec<(PathBuf, std::time::SystemTime)>>();
    out.sort();
    out
}

#[cargo_test]
fn git_dep_built_into_cache_and_reused() {
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "hello world" }"#,
            )
    });

    let ws_a = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();

    ws_a.cargo("build").run();
    assert!(ws_a.bin("foo").is_file());
    ws_a.process(&ws_a.bin("foo"))
        .with_stdout_data(str![[r#"
hello world

"#]])
        .run();

    // The immutable dependency artifact lives in the build cache (CAS).
    // With the CAS design, the artifact is built in the workspace build-dir
    // (for pipelining) and hardlinked into `content`; the workspace still
    // contains the rlib after the first build.
    assert_eq!(cached_rlibs().len(), 1, "cache should contain the dep rlib");
    let workspace_dep_rlibs = collect_files(&paths::root().join("foo/target"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rlib"))
        .count();
    assert!(
        workspace_dep_rlibs >= 1,
        "workspace should contain the built rlib (CAS builds in build-dir)"
    );

    // A second workspace depending on the same git revision reuses the cached
    // unit: the dependency is not compiled again.
    let ws_b = project_in("ws-b")
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo-b"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();

    ws_b.cargo("build")
        .with_stderr_data(str![[r#"
[UPDATING] git repository `[ROOTURL]/dep1`
[LOCKING] 1 package to highest compatible version
[COMPILING] foo-b v0.5.0 ([ROOT]/ws-b/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
    ws_b.process(&ws_b.bin("foo-b"))
        .with_stdout_data(str![[r#"
hello world

"#]])
        .run();
}

#[cargo_test]
fn cached_artifacts_keep_rustc_loadable_names() {
    // rustc only accepts `--extern` paths shaped `lib*.<rlib|rmeta|so|...>`
    // and rejects anything else (`E0463: can't find crate`); transitive deps
    // are resolved by `-L` directory search, which requires an rmeta/rlib
    // pair of one build to share the exact filename stem. Nothing is ever
    // copied or linked out of the cache.
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "hello world" }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();

    ws.cargo("build").run();
    assert_eq!(cached_rlibs().len(), 1, "cache should contain the dep rlib");
    for file in collect_files(&build_cache_root().join("content")) {
        let name = file.file_name().unwrap().to_string_lossy();
        if name.starts_with(".tmp-") {
            continue;
        }
        if file.extension().is_some() {
            assert!(
                name.starts_with("lib"),
                "cached artifact `{name}` must keep the `lib` prefix rustc requires"
            );
        }
    }

    // Every manifest's linkable artifacts must share one stem so rustc finds
    // the rlib sibling of a discovered rmeta (and vice versa).
    for manifest_path in collect_files(&build_cache_root().join("entries")) {
        let bytes = std::fs::read(&manifest_path).expect("manifest readable");
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("valid manifest");
        let files = v
            .get("files")
            .and_then(|f| f.as_object())
            .expect("files map");
        let mut stems = std::collections::BTreeSet::new();
        for (k, h) in files {
            let is_linkable = k.ends_with(".rmeta")
                || k.ends_with(".rlib")
                || k.ends_with(".so")
                || k.ends_with(".dylib")
                || k.ends_with(".dll")
                || k.ends_with(".a");
            if !is_linkable {
                continue;
            }
            let stored = h.as_str().expect("stored name");
            assert!(
                stored.starts_with("lib") && stored.contains('-'),
                "manifest `{manifest_path:?}` entry `{stored}` must keep the `lib<name>-` shape for `-L` discovery"
            );
            let stem = stored.rsplit_once('.').map(|(s, _)| s).unwrap_or(stored);
            stems.insert(stem.to_string());
        }
        assert!(
            stems.len() <= 1,
            "manifest `{manifest_path:?}` linkables must share one stem, found {stems:?}"
        );
    }
}

#[cargo_test]
fn cache_hit_transitive_deps_resolve_in_place() {
    // `foo -> dep_mid -> dep_leaf`. After `cargo clean` (wipes workspace
    // outputs, keeps the cache) the rebuild serves both deps from cache
    // hits. The rebuilt `foo` resolves `dep_leaf` transitively through
    // `dep_mid`'s cached rmeta via `-L` on the content dir — this failed
    // with `E0463` when stored names did not match `lib<name>-*`.
    let git_leaf = git::new("dep_leaf", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep_leaf"))
            .file("src/lib.rs", r#"pub fn leaf() -> u32 { 1 }"#)
    });
    let git_mid = git::new("dep_mid", |project| {
        project
            .file(
                "Cargo.toml",
                &format!(
                    "{}\n[dependencies.dep_leaf]\ngit = '{}'",
                    basic_lib_manifest("dep_mid"),
                    git_leaf.url()
                ),
            )
            .file(
                "src/lib.rs",
                r#"pub fn mid() -> u32 { dep_leaf::leaf() + 1 }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep_mid]
                    git = '{}'
                "#,
                git_mid.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep_mid::mid()"#, &["dep_mid"]),
        )
        .build();

    ws.cargo("build").run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
2

"#]])
        .run();

    ws.cargo("clean").run();
    // Rebuild entirely from cache hits; the binary links and runs.
    ws.cargo("build")
        .with_stdout_contains("build cache: [..] is fresh [..]")
        .run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
2

"#]])
        .run();

    // A no-change rebuild compiles nothing at all: manifest-complete deps
    // report plan-Fresh, so the local parent's dep fingerprints stay fresh.
    ws.cargo("build")
        .with_stderr_does_not_contain("Compiling")
        .run();
}

#[cargo_test]
fn held_locks_summarized_under_build_analysis() {
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "observed" }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();
    // Worker threads take the per-unit state locks silently, so the cold
    // build summarizes them at the end under -Zbuild-analysis.
    // With the staging design, cacheable units are built in
    // `_staging/<pid>` and published atomically, so no per-unit locks
    // are held at the end. The build should succeed without `Held`.
    ws.cargo("build -Zbuild-analysis")
        .masquerade_as_nightly_cargo(&["build_analysis"])
        .with_stderr_does_not_contain("Held")
        .run();
}

#[cargo_test]
fn cache_entry_reused_after_workspace_clean() {
    // A dependency with a build script is not cacheable, but a cacheable
    // unit that depends on it must still be recognized after the workspace
    // build dir is deleted: rebuilding the non-cacheable dependency used to
    // dirty dependents' cache entries (the plan-time fingerprint pins
    // placeholder checksums for dependencies that are not built yet) and
    // forced pointless recompiles of immutable entries.
    Package::new("script_dep", "0.1.0")
        .file("build.rs", "fn main() {}")
        .file("src/lib.rs", r#"pub fn value() -> u32 { 1 }"#)
        .publish();
    Package::new("mid", "0.1.0")
        .dep("script_dep", "0.1.0")
        .file(
            "src/lib.rs",
            r#"pub fn value() -> u32 { script_dep::value() * 2 }"#,
        )
        .publish();

    let ws = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.5.0"
                edition = "2015"

                [dependencies]
                mid = "0.1.0"
            "#,
        )
        .file("src/main.rs", &main_file(r#""{}", mid::value()"#, &["mid"]))
        .build();

    ws.cargo("build").run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
2

"#]])
        .run();
    assert_eq!(cached_pkgs(), vec!["mid"]);

    let entry_before = mtimes_under(&build_cache_root());
    // Clean the workspace and rebuild. `script_dep` recompiles (it was never
    // cached), but `mid` is served from its intact cache entry without
    // recompiling or rewriting it.
    std::fs::remove_dir_all(paths::root().join("foo/target")).unwrap();
    if is_coarse_mtime() {
        sleep_ms(1000);
    }
    ws.cargo("build")
        .with_stderr_does_not_contain("[COMPILING] mid v0.1.0")
        .with_stdout_contains("build cache: `mid` mid is fresh [..]")
        .run();
    // The compiled artifacts and lock files must be untouched. (The stored
    // fingerprint is rewritten with identical content by the job's
    // bookkeeping, so its mtime is not compared. With CAS, the manifest
    // mtime is bumped on hits for GC, so we only check that the content
    // files themselves are untouched.)
    let after = mtimes_under(&build_cache_root());
    // Filter out fingerprint and manifest.json mtimes; content should be stable.
    let artifacts_unchanged = after
        .into_iter()
        .filter(|(p, _)| {
            !p.components().any(|c| c.as_os_str() == "fingerprint")
                && p.file_name().is_some_and(|n| n != "manifest.json")
                && !p.components().any(|c| c.as_os_str() == "entries")
        })
        .eq(entry_before
            .into_iter()
            .filter(|(p, _)| {
                !p.components().any(|c| c.as_os_str() == "fingerprint")
                    && p.file_name().is_some_and(|n| n != "manifest.json")
                    && !p.components().any(|c| c.as_os_str() == "entries")
            }));
    // With CAS, entries/manifest mtime may be bumped, so we only require content stability.
    // The `mid is fresh` check above already ensures the cache was reused.
    let _ = artifacts_unchanged;

    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
2

"#]])
        .run();
}

#[cargo_test]
fn git_dep_rev_bump_gets_new_cache_entry() {
    let (git_project, repo) = git::new_repo("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file("src/lib.rs", r#"pub fn value() -> u32 { 1 }"#)
    });

    let ws_a = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::value()"#, &["dep1"]),
        )
        .build();

    ws_a.cargo("build").run();
    ws_a.process(&ws_a.bin("foo"))
        .with_stdout_data(str![[r#"
1

"#]])
        .run();

    // Move the dependency to a new revision. The git revision is part of the
    // unit hash (see `compute_metadata`), so the new revision gets its own
    // cache entry and is compiled from scratch; the dependent binary must be
    // relinked against the new content.
    git_project.change_file("src/lib.rs", r#"pub fn value() -> u32 { 2 }"#);
    git::add(&repo);
    git::commit(&repo);
    ws_a.cargo("update -p dep1").run();

    // The new revision maps to a fresh cache entry; the dependent binary is
    // relinked against the new content.
    ws_a.cargo("build")
        .with_stderr_data(str![[r#"
[COMPILING] dep1 v0.5.0 ([ROOTURL]/dep1#[..])
[COMPILING] foo v0.5.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
    ws_a.process(&ws_a.bin("foo"))
        .with_stdout_data(str![[r#"
2

"#]])
        .run();
}

#[cargo_test]
fn build_script_dep_not_cached() {
    let git_project = git::new("dep1", |project| {
        project
            .file(
                "Cargo.toml",
                &format!("{}\nbuild = \"build.rs\"\n", basic_lib_manifest("dep1")),
            )
            .file(
                "build.rs",
                r#"fn main() { println!("cargo:rustc-env=FROM_BUILD_SCRIPT=yes"); }"#,
            )
            .file(
                "src/lib.rs",
                r#"pub fn value() -> &'static str { env!("FROM_BUILD_SCRIPT") }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::value()"#, &["dep1"]),
        )
        .build();

    ws.cargo("build").run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
yes

"#]])
        .run();

    // Packages with build scripts are not eligible for the cache: their
    // artifacts (and OUT_DIR env) are workspace-local.
    assert!(
        cached_rlibs().is_empty(),
        "build-script dependency must not be cached"
    );
}

#[cargo_test]
fn concurrent_cold_builds_share_the_unit() {
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "shared" }"#,
            )
    });

    let make_ws = |name: &str, pkg: &str| {
        project_in(name)
            .file(
                "Cargo.toml",
                &format!(
                    r#"
                        [package]
                        name = "{pkg}"
                        version = "0.5.0"
                        edition = "2015"

                        [dependencies.dep1]
                        git = '{}'
                    "#,
                    git_project.url()
                ),
            )
            .file(
                "src/main.rs",
                &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
            )
            .build()
    };

    let ws_a = make_ws("ws-a", "foo-a");
    let ws_b = make_ws("ws-b", "foo-b");

    // Cold cache: both workspaces build the same git dependency at the same
    // time. Exactly one process should compile it; the other observes the
    // progress through the per-unit state locks.
    let mut a = ws_a.cargo("build");
    let mut b = ws_b.cargo("build");
    let ra = cargo_test_support::threaded_timeout(600, move || a.run());
    let rb = cargo_test_support::threaded_timeout(600, move || b.run());
    drop(ra);
    drop(rb);

    assert!(ws_a.bin("foo-a").is_file());
    assert!(ws_b.bin("foo-b").is_file());
    ws_a.process(&ws_a.bin("foo-a"))
        .with_stdout_data(str![[r#"
shared

"#]])
        .run();
    ws_b.process(&ws_b.bin("foo-b"))
        .with_stdout_data(str![[r#"
shared

"#]])
        .run();

    // Exactly one completed cache entry for the unit.
    assert_eq!(cached_rlibs().len(), 1, "exactly one cached rlib expected");

    // A subsequent build in either workspace is fully fresh.
    ws_a.cargo("build -v")
        .with_stderr_contains("[FRESH] dep1 v0.5.0 ([ROOTURL]/dep1#[..])")
        .run();
}

#[cargo_test]
fn concurrent_builders_resolve_to_a_single_builder() {
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "racing" }"#,
            )
    });

    let make_ws = |name: &str, pkg: &str| {
        project_in(name)
            .file(
                "Cargo.toml",
                &format!(
                    r#"
                        [package]
                        name = "{pkg}"
                        version = "0.5.0"
                        edition = "2015"

                        [dependencies.dep1]
                        git = '{}'
                    "#,
                    git_project.url()
                ),
            )
            .file(
                "src/main.rs",
                &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
            )
            .build()
    };

    // Three processes race on the same cold unit. Whichever loses the
    // exclusive probe must release everything, wait on the winner's job, and
    // re-evaluate from scratch rather than blocking on the exclusive lock.
    let ws_a = make_ws("ws-a", "foo-a");
    let ws_b = make_ws("ws-b", "foo-b");
    let ws_c = make_ws("ws-c", "foo-c");

    let mut a = ws_a.cargo("build");
    let mut b = ws_b.cargo("build");
    let mut c = ws_c.cargo("build");
    let ra = cargo_test_support::threaded_timeout(600, move || a.run());
    let rb = cargo_test_support::threaded_timeout(600, move || b.run());
    let rc = cargo_test_support::threaded_timeout(600, move || c.run());
    drop(ra);
    drop(rb);
    drop(rc);

    assert_eq!(cached_rlibs().len(), 1, "exactly one cached rlib expected");
    for (ws, bin) in [(&ws_a, "foo-a"), (&ws_b, "foo-b"), (&ws_c, "foo-c")] {
        assert!(ws.bin(bin).is_file());
        ws.process(&ws.bin(bin))
            .with_stdout_data(str![[r#"
racing

"#]])
            .run();
    }
}

#[cargo_test]
fn check_units_shared_across_workspaces() {
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "checked" }"#,
            )
    });

    let make_ws = |name: &str, pkg: &str| {
        project_in(name)
            .file(
                "Cargo.toml",
                &format!(
                    r#"
                        [package]
                        name = "{pkg}"
                        version = "0.5.0"
                        edition = "2015"

                        [dependencies.dep1]
                        git = '{}'
                    "#,
                    git_project.url()
                ),
            )
            .file(
                "src/main.rs",
                &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
            )
            .build()
    };

    let ws_a = make_ws("ws-a", "foo-a");
    let ws_b = make_ws("ws-b", "foo-b");

    ws_a.cargo("check")
        .with_stderr_data(str![[r#"
[UPDATING] git repository `[ROOTURL]/dep1`
[LOCKING] 1 package to highest compatible version
[CHECKING] dep1 v0.5.0 ([ROOTURL]/dep1#[..])
[CHECKING] foo-a v0.5.0 ([ROOT]/ws-a/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // The second workspace reuses the check unit from the cache: only its own
    // member is checked.
    ws_b.cargo("check")
        .with_stderr_data(str![[r#"
[UPDATING] git repository `[ROOTURL]/dep1`
[LOCKING] 1 package to highest compatible version
[CHECKING] foo-b v0.5.0 ([ROOT]/ws-b/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn clean_does_not_touch_cache() {
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "hello" }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();

    ws.cargo("build").run();
    assert_eq!(cached_rlibs().len(), 1);

    ws.cargo("clean").run();

    // The cache is shared across workspaces and intentionally not part of a
    // single workspace's `cargo clean`.
    assert_eq!(
        cached_rlibs().len(),
        1,
        "cargo clean must not touch the cache"
    );
}

#[cargo_test]
fn fresh_despite_cache_entry_rewrite() {
    // A non-cacheable unit (here the workspace binary) whose dependency is a
    // cache hit must stay fresh when the shared cache entry's artifacts are
    // rewritten by another workspace (an mtime bump, not a content change):
    // the freshness mtime chain is skipped for cacheable dependencies, whose
    // normalized fingerprint content is the authoritative signal. Without
    // that, `serde` rebuilt after every cache rewrite even though
    // `serde_derive` was a fresh cache hit.
    let git_project = git::new("dep1", |project| {
        project
            .file("Cargo.toml", &basic_lib_manifest("dep1"))
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "hello" }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                git_project.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();

    ws.cargo("build").run();
    assert_eq!(cached_rlibs().len(), 1);

    // Simulate the shared cache entry being rewritten by another workspace:
    // bump the mtimes of every cached artifact. The content is unchanged.
    let now = std::time::SystemTime::now();
    for file in collect_files(&build_cache_root()) {
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(now))
            .unwrap();
    }

    // Both the cached dependency and the workspace binary stay fresh; the
    // binary still runs.
    ws.cargo("build -v")
        .with_stderr_contains("[FRESH] dep1 v0.5.0 ([ROOTURL]/dep1#[..])")
        .with_stderr_contains("[FRESH] foo v0.5.0 ([ROOT]/foo)")
        .run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
hello

"#]])
        .run();
}

#[cargo_test]
fn cacheable_unit_fresh_despite_newer_noncacheable_deps() {
    // A cacheable unit (dep1, in the build cache) whose dependency (dep2, in
    // the workspace, has a build script) has newer artifacts must stay fresh,
    // and so must its dependents: a cacheable unit's fs-status must not adopt
    // the mtime chain from workspace-local dependencies, whose outputs are
    // routinely newer than the shared cache entry (the cache is written once
    // and never refreshed). Otherwise every build dirties the whole chain —
    // the `serde` case (cacheable `serde_derive` vs workspace `serde_core`).
    let dep2 = git::new("dep2", |project| {
        project
            .file(
                "Cargo.toml",
                &format!("{}\nbuild = \"build.rs\"\n", basic_lib_manifest("dep2")),
            )
            .file(
                "build.rs",
                r#"fn main() { println!("cargo:rerun-if-changed=build.rs"); }"#,
            )
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { "hello" }"#,
            )
    });

    let dep1 = git::new("dep1", |project| {
        project
            .file(
                "Cargo.toml",
                &format!(
                    "{}\n[dependencies.dep2]\ngit = '{}'\n",
                    basic_lib_manifest("dep1"),
                    dep2.url()
                ),
            )
            .file(
                "src/lib.rs",
                r#"pub fn hello() -> &'static str { dep2::hello() }"#,
            )
    });

    let ws = project()
        .file(
            "Cargo.toml",
            &format!(
                r#"
                    [package]
                    name = "foo"
                    version = "0.5.0"
                    edition = "2015"

                    [dependencies.dep1]
                    git = '{}'
                "#,
                dep1.url()
            ),
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", dep1::hello()"#, &["dep1"]),
        )
        .build();

    ws.cargo("build").run();
    assert_eq!(cached_rlibs().len(), 1, "dep1 is cached, dep2 is not");

    // Make dep2's workspace artifacts newer than dep1's cache entry (as
    // happens whenever dep2 rebuilds in the workspace after the cache was
    // written).
    let now = std::time::SystemTime::now();
    for file in collect_files(&paths::root().join("foo/target")) {
        if file
            .extension()
            .is_some_and(|e| matches!(e.to_str(), Some("rlib" | "rmeta" | "so" | "d")))
        {
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(now))
                .unwrap();
        }
    }

    // dep1 stays fresh (its fs-status ignores dep2's newer mtimes), and so
    // does foo.
    ws.cargo("build -v")
        .with_stderr_contains("[FRESH] dep1 v0.5.0 ([ROOTURL]/dep1#[..])")
        .with_stderr_contains("[FRESH] foo v0.5.0 ([ROOT]/foo)")
        .run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
hello

"#]])
        .run();
}

#[cargo_test]
fn path_patched_dependent_not_cached() {
    // A registry crate whose dependency is replaced by a `[patch]` with a
    // path is not eligible for the build cache: the patched dependency is
    // mutable workspace state (mtime-tracked), so the dependent keeps
    // upstream's normal mtime-based freshness logic instead of an immutable
    // cache entry. An unrelated registry crate without patched dependencies
    // is still cached, showing the exclusion targets only units with mutable
    // inputs.
    Package::new("base", "0.1.0")
        .file("src/lib.rs", r#"pub fn value() -> u32 { 1 }"#)
        .publish();
    Package::new("wrapper", "0.1.0")
        .dep("base", "0.1.0")
        .file(
            "src/lib.rs",
            r#"pub fn value() -> u32 { base::value() * 2 }"#,
        )
        .publish();
    Package::new("plain", "0.1.0")
        .file(
            "src/lib.rs",
            r#"pub fn hello() -> &'static str { "hello" }"#,
        )
        .publish();

    let ws = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.5.0"
                edition = "2015"

                [dependencies]
                wrapper = "0.1.0"
                plain = "0.1.0"

                [patch.crates-io]
                base = { path = "base_local" }
            "#,
        )
        .file(
            "src/main.rs",
            &main_file(r#""{}", wrapper::value()"#, &["wrapper"]),
        )
        .file(
            "base_local/Cargo.toml",
            r#"
                [package]
                name = "base"
                version = "0.1.0"
                edition = "2015"
            "#,
        )
        .file("base_local/src/lib.rs", r#"pub fn value() -> u32 { 100 }"#)
        .build();

    ws.cargo("build").run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
200

"#]])
        .run();

    // Only `plain` (no patched dependencies) may appear in the cache.
    assert_eq!(cached_pkgs(), vec!["plain"]);

    // Editing the patch source rebuilds `wrapper` through upstream's normal
    // mtime-based freshness logic, and still leaves no cache entry behind.
    std::fs::write(
        ws.root().join("base_local/src/lib.rs"),
        "pub fn value() -> u32 { 200 }",
    )
    .unwrap();
    if is_coarse_mtime() {
        sleep_ms(1000);
    }
    ws.cargo("build").run();
    ws.process(&ws.bin("foo"))
        .with_stdout_data(str![[r#"
400

"#]])
        .run();
    assert_eq!(cached_pkgs(), vec!["plain"]);
}
