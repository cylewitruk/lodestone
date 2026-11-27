# Configuration

Settings live in `~/.lodestone/config.toml`. Every key is optional. `LODESTONE_HOME` moves the data
directory.

```toml
# socket = "/path/to/daemon.sock"  # default: <data dir>/daemon.sock
# port = 4600               # also listen on 127.0.0.1:4600; off by default (Windows: always 4600)
idle_timeout_secs = 1800    # on-demand daemon: exit this long after the last session; 0 = never
model = "jina-code-v2"      # alias into [models]; see `lode models`
# backend = "candle-cpu"    # candle-metal (macOS default) | candle-cpu (default elsewhere) | fastembed | remote
batch_size = 64
cache_max_gb = 4            # embedding cache budget (vector data, in GB); oldest entries go first
chunk_tokens = 200
vector_precision = "f32"    # f32 | f16
doc_vectors = false         # also embed each item's doc comment; used only with the reranker

[languages]
tree_sitter_config = "~/.config/tree-sitter/config.json"
extensions = { "cljc" = "clojure" }
index_untagged_as_text = ["md", "txt", "adoc", "rst"]  # indexed as plain text when no grammar claims them

[rerank]
enabled = false             # ~0.4 s per search on Metal, ~2.6 s on CPU; hurts well-phrased queries
hf_repo = "Alibaba-NLP/gte-reranker-modernbert-base"
candidates = 20             # 50 for weaker embedding models such as coderank
max_tokens = 192

[search]
lexical_weight = 0.0        # BM25 fusion (reciprocal rank); off by default
identifier_lexical_weight = 0.0

[hooks]
duplicate_similarity = 0.75 # edit hint: name existing definitions this similar (cosine, jina-code-v2)
```

## Backends

| Backend | Where | Notes |
| ---- | ---- | ---- |
| `candle-metal` | macOS (default) | GPU through Metal |
| `candle-cpu` | Linux, Windows (default) | no GPU backend there |
| `fastembed` | anywhere | ONNX Runtime, CPU only; build with `--features fastembed` (about 20 MB more). `LODESTONE_FASTEMBED_EP=coreml` uses CoreML on macOS |
| `remote` | anywhere | any OpenAI-compatible embeddings endpoint, set in `[remote]` (`url`, and `api_key_env` naming the variable holding the key) |

```bash
cargo install --locked lodestone-cli --features fastembed
```

## Models

- **Built in** (`lode models`): `jina-code-v2` (default), `qwen3-0.6b`, `harrier-0.6b`,
  `coderank`, `gte-modernbert`, `granite-r2`, `granite-small-r2`, `ultrabert-150m`,
  `embeddinggemma-2`, `nomic-text-v1.5`, `bge-small`, `minilm-l6`, and `jina-code-0.5b`
  (non-commercial licence). fastembed has its own list.
- **Switching models** needs `lode index --force`. The cache keeps each model's vectors apart, and
  so it does for a model whose settings change (`document_prefix`, `max_tokens`, `pooling`, …):
  those re-embed on the next index.
- **Upgrading from v0.1.1 after changing those settings:** v0.1.1 kept vectors made with changed
  settings under the model's plain id, which the built-in settings use again, and its cache is
  shared, so any worktree indexed with that model may hold them. If you changed them on v0.1.1,
  run `lode stop`, delete `~/.lodestone/cache.sqlite`, and run `lode index --force` in every
  worktree indexed with that model.
- **Rerankers:** a Hugging Face cross-encoder in `[rerank] hf_repo` (candle) of a supported family:
  ModernBERT (gte, granite), BERT (ms-marco-MiniLM), XLM-RoBERTa (bge-reranker,
  jina-reranker-v2), Qwen3-Reranker or jina-reranker-v3; or a fastembed reranker in
  `[rerank] model`. The [decision log](decision-log.md) compares them.

## Languages

- **Built-in grammars** are Cargo features, all on by default: `lang-rust`, `lang-python`,
  `lang-typescript`, `lang-svelte`, `lang-go`, `lang-java`, `lang-c`, `lang-cpp`, `lang-csharp`,
  `lang-ruby`, `lang-php`, `lang-lua`, `lang-elixir`, `lang-swift`, `lang-markdown`,
  `lang-clarity`. Clarity's grammar isn't on crates.io, so it's vendored in
  `vendor/tree-sitter-clarity/`.
- **Adding a language needs no code:** clone a grammar into one of tree-sitter's
  `parser-directories`; its `queries/tags.scm` is used.
- **Extra queries** (`tags.scm`, `tests.scm`) go in `~/.lodestone/queries/<language>/`.
- **`.h` headers** are C unless they use C++-only syntax (a class, template, namespace, access
  specifier, `std::`, or a header such as `<string>`). Mapping `h` under `[languages] extensions`
  turns this off.
- **Svelte:** each `<script>` block is indexed with the TypeScript or JavaScript grammar, markup as
  text, `<style>` not at all. Items keep the component's line numbers.
