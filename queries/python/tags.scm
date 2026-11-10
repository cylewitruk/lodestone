; Supplementary Python tags: docstrings (@doc) and methods. Evaluated before the grammar's
; tags.scm.

(class_definition
  body: (block
    (function_definition
      name: (identifier) @name
      body: (block . (expression_statement (string (string_content) @doc))?)) @definition.method))

(class_definition
  body: (block
    (decorated_definition
      definition: (function_definition
        name: (identifier) @name
        body: (block . (expression_statement (string (string_content) @doc))?)) @definition.method)))

(function_definition
  name: (identifier) @name
  body: (block . (expression_statement (string (string_content) @doc))?)) @definition.function

(class_definition
  name: (identifier) @name
  body: (block . (expression_statement (string (string_content) @doc))?)) @definition.class
