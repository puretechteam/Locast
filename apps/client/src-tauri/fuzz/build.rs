//! MSVC link fix for the fuzz binaries.
//!
//! On `*-windows-msvc`, cargo-fuzz normally injects
//! `-Clink-arg=/include:main` through RUSTFLAGS so link.exe pulls
//! libFuzzer's `main` into the fuzz binary. RUSTFLAGS reach every
//! linked artifact, including the `cdylib` crate type of the
//! `locast-client` dependency, and a DLL cannot resolve `main`
//! (LNK2001). Run cargo-fuzz with `--no-include-main-msvc` and
//! scope the same argument to this crate's binaries instead.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/include:main");
    }
}
