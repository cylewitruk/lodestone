; Swift tags with doc comments (@doc). Replaces the grammar's tags.scm, which tags a whole class
; as each of its methods and every local `let` as a property. Earlier patterns win for a name
; both match (tree-sitter-tags keeps the lowest pattern index), so members come first.

; Members: documented, then any (an anchor after zero comments inside a parent pattern
; would only match the first member).
(class_body
  [(comment) (multiline_comment)]+ @doc
  .
  [
    (function_declaration name: (simple_identifier) @name)
    (init_declaration name: "init" @name)
    (deinit_declaration "deinit" @name)
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(class_body
  [
    (function_declaration name: (simple_identifier) @name)
    (init_declaration name: "init" @name)
    (deinit_declaration "deinit" @name)
  ] @definition.method)

(enum_class_body
  [(comment) (multiline_comment)]+ @doc
  .
  [
    (function_declaration name: (simple_identifier) @name)
    (init_declaration name: "init" @name)
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(enum_class_body
  [
    (function_declaration name: (simple_identifier) @name)
    (init_declaration name: "init" @name)
  ] @definition.method)

(protocol_body
  [(comment) (multiline_comment)]+ @doc
  .
  [
    (protocol_function_declaration name: (simple_identifier) @name)
    (init_declaration name: "init" @name)
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(protocol_body
  [
    (protocol_function_declaration name: (simple_identifier) @name)
    (init_declaration name: "init" @name)
  ] @definition.method)

(class_body
  [(comment) (multiline_comment)]+ @doc
  .
  (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @definition.property
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.property))

(class_body
  (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @definition.property)

; Types and free functions.
(
  [(comment) (multiline_comment)]* @doc
  .
  (class_declaration declaration_kind: "extension" name: (user_type (type_identifier) @name)) @definition.impl
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.impl))

(
  [(comment) (multiline_comment)]* @doc
  .
  (class_declaration name: (type_identifier) @name) @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  [(comment) (multiline_comment)]* @doc
  .
  (protocol_declaration name: (type_identifier) @name) @definition.interface
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.interface))

(
  [(comment) (multiline_comment)]* @doc
  .
  (typealias_declaration name: (type_identifier) @name) @definition.type
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.type))

(
  [(comment) (multiline_comment)]* @doc
  .
  (function_declaration name: (simple_identifier) @name) @definition.function
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.function))
