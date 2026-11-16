; C tags with doc comments (@doc). Replaces the grammar's tags.scm, which tags only a function's
; declarator (not its body) and enum references as definitions.

(
  (comment)* @doc
  .
  [
    (function_definition declarator: [
      (function_declarator declarator: (identifier) @name)
      (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
      (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)))
    ])
    (declaration declarator: [
      (function_declarator declarator: (identifier) @name)
      (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))
      (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name)))
    ])
  ] @definition.function
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.function))

(
  (comment)* @doc
  .
  [
    (struct_specifier name: (type_identifier) @name body: (_))
    (union_specifier name: (type_identifier) @name body: (_))
  ] @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  (comment)* @doc
  .
  [
    (enum_specifier name: (type_identifier) @name body: (_))
    (type_definition declarator: (type_identifier) @name)
  ] @definition.type
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.type))

(
  (comment)* @doc
  .
  (preproc_function_def name: (identifier) @name) @definition.macro
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.macro))

; Object-like macros with a value; bare `#define X` (include guards, flags) is skipped.
(
  (comment)* @doc
  .
  (preproc_def name: (identifier) @name value: (_)) @definition.constant
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.constant))
