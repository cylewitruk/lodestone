//! Compiles grammars vendored as generated C (`vendor/`), for languages without a crates.io
//! grammar crate, and links clang's runtime where ONNX Runtime needs it.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_FEATURE_LANG_CLARITY").is_some() {
        grammar("clarity");
    }
    if env::var_os("CARGO_FEATURE_FASTEMBED").is_some()
        && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
    {
        link_clang_runtime();
    }
}

/// ONNX Runtime's prebuilt macOS library calls `__isPlatformVersionAtLeast`, which clang's
/// runtime defines; under LTO nothing else provides it and the link fails.
fn link_clang_runtime() {
    let compiler = cc::Build::new().get_compiler();
    let dir = Command::new(compiler.path())
        .arg("-print-runtime-dir")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    match dir.map(|d| d.join("libclang_rt.osx.a")) {
        Some(lib) if lib.exists() => println!("cargo:rustc-link-arg={}", lib.display()),
        _ => println!(
            "cargo:warning=clang runtime not found; an LTO build with fastembed may not link"
        ),
    }
}

fn grammar(name: &str) {
    let src = Path::new("vendor")
        .join(format!("tree-sitter-{name}"))
        .join("src");
    let mut build = cc::Build::new();
    build
        .std("c11")
        .include(&src)
        .file(src.join("parser.c"))
        .warnings(false);
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        build.flag("-utf-8");
    }
    build.compile(&format!("tree-sitter-{name}"));
    println!("cargo:rerun-if-changed={}", src.display());
}
