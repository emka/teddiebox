//! Tells cargo which environment variables the build depends on, and sets
//! the version the box compares with the update manifest.
//!
//! `TEDDIEBOX_LANGUAGE` and `TEDDIEBOX_SLIX_PASSWORD` are compiled in with
//! `option_env!`. Without the `rerun-if-env-changed` lines, changing one in
//! `.envrc.local` would not rebuild, and the box would keep the old value.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_LANGUAGE");
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_SLIX_PASSWORD");
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_RELEASE");

    // The development console commands, enabled with a `cfg` rather than a
    // cargo feature, because a feature cannot be set from an environment
    // variable. This allows `#[cfg(bench)]` on whole items.
    //
    // `TEDDIEBOX_RELEASE=0` (or empty, `no`, `false`) means "not a release",
    // not just "the variable is set".
    println!("cargo:rustc-check-cfg=cfg(bench)");
    let release = std::env::var("TEDDIEBOX_RELEASE")
        .map(|v| !matches!(v.as_str(), "" | "0" | "no" | "false"))
        .unwrap_or(false);
    if !release {
        println!("cargo:rustc-cfg=bench");
    }

    // The version compared with the update manifest. `git describe --always
    // --dirty` gives a tag if there is one, otherwise a short hash, plus
    // `-dirty` for uncommitted changes.
    //
    // Outside a git checkout this is "unknown", which `teddiebox_ota::decide`
    // treats as a normal version. Only an *empty* version is refused, and
    // this never produces one.
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

    // Rebuild after a commit, so the version is not stale.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
}
