; Supplementary Java tags: doc comments (@doc), constructors, enums, records and annotation
; types. Evaluated before the grammar's tags.scm.

(
  [(line_comment) (block_comment)]* @doc
  .
  [
    (class_declaration name: (identifier) @name)
    (enum_declaration name: (identifier) @name)
    (record_declaration name: (identifier) @name)
  ] @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  [(line_comment) (block_comment)]* @doc
  .
  [
    (interface_declaration name: (identifier) @name)
    (annotation_type_declaration name: (identifier) @name)
  ] @definition.interface
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.interface))

(
  [(line_comment) (block_comment)]* @doc
  .
  [
    (method_declaration name: (identifier) @name)
    (constructor_declaration name: (identifier) @name)
    (compact_constructor_declaration name: (identifier) @name)
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))
