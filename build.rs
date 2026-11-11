//! Compiles grammars vendored as generated C (`vendor/`), for languages without a crates.io
//! grammar crate.

use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_FEATURE_LANG_CLARITY").is_some() {
        grammar("clarity");
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
