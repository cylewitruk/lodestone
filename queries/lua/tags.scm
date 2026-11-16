; Supplementary Lua tags: doc comments (@doc), including LuaLS `---` annotations. Evaluated
; before the grammar's tags.scm.

(
  (comment)* @doc
  .
  (function_declaration
    name: [
      (identifier) @name
      (dot_index_expression field: (identifier) @name)
    ]) @definition.function
  (#strip! @doc "(?m)^[ \\t]*--+(?:\\[=*\\[)?[ \\t]?|\\]=*\\][ \\t]*$")
  (#select-adjacent! @doc @definition.function))

(
  (comment)* @doc
  .
  (function_declaration name: (method_index_expression method: (identifier) @name)) @definition.method
  (#strip! @doc "(?m)^[ \\t]*--+(?:\\[=*\\[)?[ \\t]?|\\]=*\\][ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(
  (comment)* @doc
  .
  (assignment_statement
    (variable_list
      .
      name: [
        (identifier) @name
        (dot_index_expression field: (identifier) @name)
      ])
    (expression_list . value: (function_definition))) @definition.function
  (#strip! @doc "(?m)^[ \\t]*--+(?:\\[=*\\[)?[ \\t]?|\\]=*\\][ \\t]*$")
  (#select-adjacent! @doc @definition.function))
