## Reuse

Before adding a helper, type, trait, macro or constant, search for an existing one and list the
candidates found (or "none found, searched: ...") before writing code. Search by concept, not by
the name you would have chosen: grep several synonyms, then read the helper homes below.

Shared helpers live in (lowest layer first; a crate can only reuse from layers below it):

| Where | What |
| ---- | ---- |
| `stacks-common/src/util/` | time (`mod.rs`), hex and hashing (`hash.rs`), newtype/serde macros (`macros.rs`), sqlite (`db.rs`), `retry`, `lru_cache`, `pipe`, crypto |
| `stacks-common/src/types/`, `codec/`, `address/` | chain ids, `StacksMessageCodec` and read/write helpers, c32/b58 |
| `clarity-types/src/types/`, `stacks-codec/src/` | Clarity values and signatures, transaction and string codecs |
| `stackslib/src/util_lib/` | `db.rs` query/row helpers, Clarity-string newtypes, boot-contract helpers |
| `stackslib/src/core/test_util.rs` | transaction builders usable from other crates' tests |
| `stackslib/src/chainstate/tests/`, `chainstate/stacks/tests/`, `chainstate/nakamoto/tests/`, `net/tests/`, `burnchains/tests/` | chainstate, block, peer and burnchain test harnesses |
| `clarity/src/vm/test_util/`, `clarity/src/vm/tests/` | Clarity test setup and proptest strategies |
| `stacks-node/src/tests/neon_integrations.rs`, `nakamoto_integrations.rs`, `tests/signer/` | integration helpers: `wait_for*`, `next_block_and*`, `submit_tx`, `SignerTest` |

A new helper or `pub` item needs a caller in the same change.
