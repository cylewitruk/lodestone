//! A grammar that is not compiled in, loaded through tree-sitter's own config, tags files with
//! no lodestone code changes. Set `LODESTONE_TEST_PARSER_DIR` to a directory containing a
//! grammar checkout (e.g. a clone of tree-sitter-scala); skipped otherwise.

use std::path::Path;
use std::sync::Arc;

use lodestone::chunk::{Chunker, LanguageRegistry};
use lodestone::config::LanguagesConfig;
use lodestone::testutil::tempdir;

#[test]
fn loader_grammar_tags_sample_file() {
    let Ok(parser_dir) = std::env::var("LODESTONE_TEST_PARSER_DIR") else {
        eprintln!("LODESTONE_TEST_PARSER_DIR not set; skipping");
        return;
    };
    let home = tempdir();
    let ts_config = home.join("config.json");
    std::fs::write(
        &ts_config,
        serde_json::json!({ "parser-directories": [parser_dir] }).to_string(),
    )
    .unwrap();
    let cfg = LanguagesConfig {
        tree_sitter_config: Some(ts_config),
        ..LanguagesConfig::default()
    };
    let reg = LanguageRegistry::load(&cfg, &home);
    let info = reg.info();
    let external: Vec<_> = info.iter().filter(|l| l.origin != "builtin").collect();
    assert!(
        !external.is_empty(),
        "no external grammar found; issues: {:?}",
        reg.issues
    );
    println!("external grammars: {external:?}");
    let scala = reg
        .for_path(Path::new("x.scala"))
        .expect("scala grammar registered for .scala");
    assert_eq!(scala.name, "scala");
    let chunker = Chunker::new(Arc::new(reg), 300);
    let items = chunker.chunk(
        "src/Ring.scala",
        b"// Ring buffer.\nclass Ring {\n  def push(v: Int): Unit = {}\n}\n\ntrait Sized {\n  def size: Int\n}\n",
    );
    println!("{items:#?}");
    assert!(
        items
            .iter()
            .any(|i| i.qualified_name == "Ring::push" && i.kind == "function" && i.tagged)
    );
    assert!(
        items
            .iter()
            .any(|i| i.name == "Sized" && i.kind == "interface")
    );
}
