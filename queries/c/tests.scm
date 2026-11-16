; Unity/CMocka-style test functions named `test_*`.
((function_definition
  declarator: (function_declarator declarator: (identifier) @_n)) @test
  (#match? @_n "^test_"))
