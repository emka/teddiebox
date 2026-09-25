//! Locates the static libopus the Nix dev shell built for this target.
//!
//! Does not compile libopus itself. Building it in a build script makes
//! cross-compiling hard and hides the codec's configuration (fixed point, no
//! neural extensions). The Nix flake owns that configuration and passes the
//! finished library by path.

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
