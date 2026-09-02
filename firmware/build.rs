//! Tells cargo which environment variables the build depends on.
//!
//! `TEDDIEBOX_LANGUAGE` is read with `option_env!`, which bakes its value into
//! the binary at compile time. Without this, changing it in `.envrc.local`
//! would leave the previous language in a cached build and the box would keep
//! speaking it — a configuration change that silently does nothing is worse
//! than one that fails.
fn main() {
    println!("cargo:rerun-if-env-changed=TEDDIEBOX_LANGUAGE");
}
