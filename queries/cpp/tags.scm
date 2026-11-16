; C++ tags with doc comments (@doc). Replaces the grammar's tags.scm, which tags only a
; function's declarator (not its body). Earlier patterns win for a name both match
; (tree-sitter-tags keeps the lowest pattern index), so specific forms come first.

; Members declared or defined in a class body: documented, then any (an anchor after zero
; comments inside a parent pattern would only match the first member).
(field_declaration_list
  (comment)+ @doc
  .
  [
    (function_definition declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])
    (field_declaration declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])
    (declaration declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])
    (template_declaration [(function_definition declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ]) (declaration declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])])
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(field_declaration_list
  [
    (function_definition declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])
    (field_declaration declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])
    (declaration declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])
    (template_declaration [(function_definition declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ]) (declaration declarator: [
      (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name))
    ])])
  ] @definition.method)


; Out-of-line member definitions, templated first.
(
  (comment)* @doc
  .
  (template_declaration [
    (function_definition declarator: [
      (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ])
      (pointer_declarator declarator: (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
      (reference_declarator (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
    ])
    (declaration declarator: [
      (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ])
      (pointer_declarator declarator: (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
      (reference_declarator (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
    ])
  ]) @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(
  (comment)* @doc
  .
  [
    (function_definition declarator: [
      (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ])
      (pointer_declarator declarator: (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
      (reference_declarator (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
    ])
    (declaration declarator: [
      (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ])
      (pointer_declarator declarator: (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
      (reference_declarator (function_declarator declarator: [
        (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name)
        (qualified_identifier
          name: (qualified_identifier name: [(identifier) (destructor_name) (operator_name)] @name))
      ]))
    ])
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

; Free functions and their declarations, templated first.
(
  (comment)* @doc
  .
  (template_declaration [
    (function_definition declarator: [
      (function_declarator declarator: [(identifier) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (operator_name)] @name))
    ])
    (declaration declarator: [
      (function_declarator declarator: [(identifier) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (operator_name)] @name))
    ])
  ]) @definition.function
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.function))

(
  (comment)* @doc
  .
  [
    (function_definition declarator: [
      (function_declarator declarator: [(identifier) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (operator_name)] @name))
    ])
    (declaration declarator: [
      (function_declarator declarator: [(identifier) (operator_name)] @name)
      (pointer_declarator declarator: (function_declarator declarator: [(identifier) (operator_name)] @name))
      (reference_declarator (function_declarator declarator: [(identifier) (operator_name)] @name))
    ])
  ] @definition.function
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.function))

; Classes, structs and unions with a body, templated first.
(
  (comment)* @doc
  .
  (template_declaration [
    (class_specifier name: (type_identifier) @name body: (_))
    (struct_specifier name: (type_identifier) @name body: (_))
    (union_specifier name: (type_identifier) @name body: (_))
  ]) @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  (comment)* @doc
  .
  [
    (class_specifier name: (type_identifier) @name body: (_))
    (struct_specifier name: (type_identifier) @name body: (_))
    (union_specifier name: (type_identifier) @name body: (_))
  ] @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  (comment)* @doc
  .
  (template_declaration (alias_declaration name: (type_identifier) @name)) @definition.type
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.type))

(
  (comment)* @doc
  .
  [
    (enum_specifier name: (type_identifier) @name body: (_))
    (type_definition declarator: (type_identifier) @name)
    (alias_declaration name: (type_identifier) @name)
  ] @definition.type
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.type))

(
  (comment)* @doc
  .
  (namespace_definition name: [(namespace_identifier) (nested_namespace_specifier)] @name) @definition.module
  (#strip! @doc "(?m)^[ \\t]*(?:/\\*+!?|\\*+/|\\*|//[/!]?)[ \\t]?|[ \\t]*\\*+/[ \\t]*$")
  (#select-adjacent! @doc @definition.module))

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
