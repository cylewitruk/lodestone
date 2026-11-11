//! The Clarity grammar: generated C vendored in `vendor/tree-sitter-clarity` and compiled by
//! `build.rs`, since it is not on crates.io.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_clarity() -> *const ();
}

pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_clarity) };
