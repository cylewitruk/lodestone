; Supplementary Rust tags: doc comments (@doc), impl blocks, constants, statics and trait
; method signatures. Evaluated before the grammar's own tags.scm so these patterns win for
; nodes both match (tree-sitter-tags keeps the lowest pattern index per name node).

(declaration_list
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (function_item name: (identifier) @name) @definition.method
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(declaration_list
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (function_signature_item name: (identifier) @name) @definition.method
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(declaration_list
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (const_item name: (identifier) @name) @definition.constant
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (function_item name: (identifier) @name) @definition.function
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  [
    (struct_item name: (type_identifier) @name)
    (enum_item name: (type_identifier) @name)
    (union_item name: (type_identifier) @name)
  ] @definition.class
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (type_item name: (type_identifier) @name) @definition.type
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (trait_item name: (type_identifier) @name) @definition.interface
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (mod_item name: (identifier) @name) @definition.module
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (macro_definition name: (identifier) @name) @definition.macro
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  [
    (const_item name: (identifier) @name)
    (static_item name: (identifier) @name)
  ] @definition.constant
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))

(
  (attribute_item)*
  .
  [(line_comment) (block_comment)]* @doc
  .
  (attribute_item)*
  .
  (impl_item type: [
    (type_identifier) @name
    (generic_type type: (type_identifier) @name)
    (scoped_type_identifier name: (type_identifier) @name)
  ]) @definition.impl
  (#strip! @doc "^(//[/!]?|/\\*[*!]?|\\*/)\\s?|\\s*\\*/$"))
