; Clarity definitions: functions, traits and their signatures, constants, data variables, maps
; and tokens. The `;;` comments directly above a definition are its doc.

(
  (comment)* @doc
  .
  (function_definition
    [
      (private_function (function_signature . (identifier) @name))
      (read_only_function (function_signature . (identifier) @name))
      (public_function (function_signature . (identifier) @name))
    ]) @definition.function
  (#strip! @doc "^;;+[ \t]?")
  (#select-adjacent! @doc @definition.function))

(
  (comment)* @doc
  .
  (trait_definition . (identifier) @name) @definition.interface
  (#strip! @doc "^;;+[ \t]?")
  (#select-adjacent! @doc @definition.interface))

(trait_definition
  (function_signature_for_trait . (identifier) @name) @definition.method)

(
  (comment)* @doc
  .
  (constant_definition . (identifier) @name) @definition.constant
  (#strip! @doc "^;;+[ \t]?")
  (#select-adjacent! @doc @definition.constant))

(
  (comment)* @doc
  .
  (variable_definition . (identifier) @name) @definition.variable
  (#strip! @doc "^;;+[ \t]?")
  (#select-adjacent! @doc @definition.variable))

(
  (comment)* @doc
  .
  (mapping_definition . (identifier) @name) @definition.map
  (#strip! @doc "^;;+[ \t]?")
  (#select-adjacent! @doc @definition.map))

(
  (comment)* @doc
  .
  (token_definition
    [
      (fungible_token_definition . (identifier) @name)
      (non_fungible_token_definition . (identifier) @name)
    ]) @definition.token
  (#strip! @doc "^;;+[ \t]?")
  (#select-adjacent! @doc @definition.token))
