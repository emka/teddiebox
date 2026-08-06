//! Locates the static libopus the Nix dev shell built for this target.
//!
//! Deliberately does no compiling of its own. Building libopus from a build
//! script is what made the previous binding uncross-compilable, and it also
//! hides the codec's configuration — fixed point, no neural extensions —
//! inside a Rust crate where nothing reviews it. The flake owns that
//! configuration and hands the finished archive over by path.

use std::env;

fn main() {
    let target = env::var("TARGET").expect("cargo always sets TARGET");
    let var = format!(
        "TEDDIEBOX_OPUS_LIB_DIR_{}",
        target.to_uppercase().replace('-', "_")
    );
    println!("cargo:rerun-if-env-changed={var}");

    let dir = env::var(&var).unwrap_or_else(|_| {
        panic!(
            "{var} is unset, so there is no libopus for {target}.\n\
             Build inside `nix develop`; add a target to flake.nix to support a new one."
        )
    });

    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=static=opus");
}
