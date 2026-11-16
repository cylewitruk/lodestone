; Elixir tags with `@doc` and `@moduledoc` strings (@doc), possibly followed by other attributes
; such as `@spec`, and `defimpl`. Replaces the grammar's tags.scm, whose `@ignore` patterns for
; attributes flush a module's tag before its documented duplicate arrives. Documented forms come
; first (tree-sitter-tags keeps the lowest pattern index).
;
; tree-sitter-tags accepts only its own capture names. Predicates use `@ignore` (which it also
; takes as the name, so only before `@name`) and `@local.reference` (unused in tags patterns).

(
  (unary_operator
    operator: "@"
    operand: (call
      target: (identifier) @local.reference
      (arguments [(string (quoted_content) @doc) (sigil (quoted_content) @doc)])))
  .
  (unary_operator)*
  .
  (call
    target: (identifier) @ignore
    (arguments
      [
        (identifier) @name
        (call target: (identifier) @name)
        (binary_operator left: (call target: (identifier) @name) operator: "when")
      ])) @definition.function
  (#eq? @local.reference "doc")
  (#any-of? @ignore "def" "defmacro" "defguard" "defdelegate" "defn")
  (#strip! @doc "(?m)^[ \\t]+"))

(call
  target: (identifier) @ignore
  (arguments (alias) @name)
  (do_block
    .
    (unary_operator
      operator: "@"
      operand: (call
        target: (identifier) @local.reference
        (arguments [(string (quoted_content) @doc) (sigil (quoted_content) @doc)]))))
  (#any-of? @ignore "defmodule" "defprotocol")
  (#eq? @local.reference "moduledoc")
  (#strip! @doc "(?m)^[ \\t]+")) @definition.module

(call
  target: (identifier) @ignore
  (arguments . (alias) @name)
  (#eq? @ignore "defimpl")) @definition.impl

(call
  target: (identifier) @ignore
  (arguments (alias) @name)
  (#any-of? @ignore "defmodule" "defprotocol")) @definition.module

(call
  target: (identifier) @ignore
  (arguments
    [
      (identifier) @name
      (call target: (identifier) @name)
      (binary_operator left: (call target: (identifier) @name) operator: "when")
    ])
  (#any-of? @ignore "def" "defp" "defdelegate" "defguard" "defguardp" "defmacro" "defmacrop" "defn" "defnp")) @definition.function
