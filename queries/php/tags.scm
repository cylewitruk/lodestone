; Supplementary PHP tags: doc comments (@doc), methods as methods, enums and constants.
; Evaluated before the grammar's tags.scm.

(
  (comment)* @doc
  .
  [
    (class_declaration name: (name) @name)
    (enum_declaration name: (name) @name)
  ] @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//|#)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  (comment)* @doc
  .
  [
    (interface_declaration name: (name) @name)
    (trait_declaration name: (name) @name)
  ] @definition.interface
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//|#)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.interface))

(
  (comment)* @doc
  .
  (method_declaration name: (name) @name) @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//|#)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(
  (comment)* @doc
  .
  (function_definition name: (name) @name) @definition.function
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//|#)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.function))

(
  (comment)* @doc
  .
  (const_declaration (const_element (name) @name)) @definition.constant
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//|#)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.constant))
