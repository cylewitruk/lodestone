; C# tags with doc comments (@doc). Replaces the grammar's tags.scm, which does not compile
; under tree-sitter-tags (it uses a bare `@module` capture).

(
  (comment)* @doc
  .
  [
    (class_declaration name: (identifier) @name)
    (struct_declaration name: (identifier) @name)
    (record_declaration name: (identifier) @name)
  ] @definition.class
  (#strip! @doc "(?m)^[ \\t]*(?:///?[ \\t]?(?:</?summary>)?|/\\*+|\\*+/|\\*)[ \\t]?|[ \\t]*(?:</summary>|\\*+/)[ \\t]*$")
  (#select-adjacent! @doc @definition.class))

(
  (comment)* @doc
  .
  (interface_declaration name: (identifier) @name) @definition.interface
  (#strip! @doc "(?m)^[ \\t]*(?:///?[ \\t]?(?:</?summary>)?|/\\*+|\\*+/|\\*)[ \\t]?|[ \\t]*(?:</summary>|\\*+/)[ \\t]*$")
  (#select-adjacent! @doc @definition.interface))

(
  (comment)* @doc
  .
  [
    (enum_declaration name: (identifier) @name)
    (delegate_declaration name: (identifier) @name)
  ] @definition.type
  (#strip! @doc "(?m)^[ \\t]*(?:///?[ \\t]?(?:</?summary>)?|/\\*+|\\*+/|\\*)[ \\t]?|[ \\t]*(?:</summary>|\\*+/)[ \\t]*$")
  (#select-adjacent! @doc @definition.type))

(
  (comment)* @doc
  .
  [
    (method_declaration name: (identifier) @name)
    (constructor_declaration name: (identifier) @name)
    (destructor_declaration name: (identifier) @name)
  ] @definition.method
  (#strip! @doc "(?m)^[ \\t]*(?:///?[ \\t]?(?:</?summary>)?|/\\*+|\\*+/|\\*)[ \\t]?|[ \\t]*(?:</summary>|\\*+/)[ \\t]*$")
  (#select-adjacent! @doc @definition.method))

(
  (comment)* @doc
  .
  (property_declaration name: (identifier) @name) @definition.property
  (#strip! @doc "(?m)^[ \\t]*(?:///?[ \\t]?(?:</?summary>)?|/\\*+|\\*+/|\\*)[ \\t]?|[ \\t]*(?:</summary>|\\*+/)[ \\t]*$")
  (#select-adjacent! @doc @definition.property))

(
  (comment)* @doc
  .
  (namespace_declaration name: [(identifier) (qualified_name)] @name) @definition.module
  (#strip! @doc "(?m)^[ \\t]*(?:///?[ \\t]?(?:</?summary>)?|/\\*+|\\*+/|\\*)[ \\t]?|[ \\t]*(?:</summary>|\\*+/)[ \\t]*$")
  (#select-adjacent! @doc @definition.module))
