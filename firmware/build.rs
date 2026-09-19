//! Tells cargo which environment variables the build depends on, and stamps
//! the build with a version the box can compare against a server's manifest.
//!
//! `TEDDIEBOX_LANGUAGE` and `TEDDIEBOX_SLIX_PASSWORD` are read with
//! `option_env!`, which bakes their values into the binary at compile time.
//! Without this, changing one in `.envrc.local` would leave the previous value
//! in a cached build — the box would keep speaking the old language, or keep
//! the old password, and a configuration change that silently does nothing is
//! worse than one that fails.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_LANGUAGE");
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_SLIX_PASSWORD");
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_RELEASE");

    // The bench console commands, as a `cfg` rather than a cargo feature.
    //
    // Cargo cannot turn a feature on from the environment — features come from
    // the manifest or the command line and nothing else — but a build script
    // can read the environment and emit a `cfg`, which is the same compile-time
    // switch by a different name. It buys what `option_env!` could not: `#[cfg(
    // bench)]` on whole items, not just a `const` that folds a branch.
    //
    // Read the way `CONFIG.TXT` reads its one boolean. `TEDDIEBOX_RELEASE=0`
    // meaning "yes, release" is the trap a plain presence check walks into, and
    // an environment variable set to `0` is what people write when they mean
    // off.
    println!("cargo:rustc-check-cfg=cfg(bench)");
    let release = std::env::var("TEDDIEBOX_RELEASE")
        .map(|v| !matches!(v.as_str(), "" | "0" | "no" | "false"))
        .unwrap_or(false);
    if !release {
        println!("cargo:rustc-cfg=bench");
    }

    // The version an update is decided against. `git describe --always
    // --dirty` gives a tag when there is one, a short hash when there is not,
    // and a `-dirty` suffix for an uncommitted tree — so a box flashed from a
    // working copy never claims to be the commit it was built near.
    //
    // A build outside a git checkout falls back to "unknown", which
    // `teddiebox_ota::decide` treats as a version like any other. It is the
    // *empty* string that is refused, and this never produces one.
    let version = Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=TEDDIEBOX_VERSION={version}");

    // Without this, a commit made after a build leaves the previous version
    // baked into a cached artefact — and a version that silently does not
    // change is exactly what this whole mechanism cannot survive.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
}
