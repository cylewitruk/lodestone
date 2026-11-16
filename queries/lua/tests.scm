; busted blocks and luaunit-style `test*` functions.
((function_call name: (identifier) @_f) @test (#match? @_f "^(describe|it|test|pending)$"))

((function_declaration
  name: [
    (identifier) @_n
    (dot_index_expression field: (identifier) @_n)
    (method_index_expression method: (identifier) @_n)
  ]) @test
  (#match? @_n "^[Tt]est"))
