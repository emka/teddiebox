//! Tells cargo which environment variables the build depends on.
//!
//! `TEDDIEBOX_LANGUAGE` and `TEDDIEBOX_SLIX_PASSWORD` are read with
//! `option_env!`, which bakes their values into the binary at compile time.
//! Without this, changing one in `.envrc.local` would leave the previous value
//! in a cached build — the box would keep speaking the old language, or keep
//! the old password, and a configuration change that silently does nothing is
//! worse than one that fails.
fn main() {
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_LANGUAGE");
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_SLIX_PASSWORD");
}
