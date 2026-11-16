; PHPUnit `test*` methods, `@test` docblocks and `*Test` classes; Pest test blocks.
((method_declaration name: (name) @_n) @test (#match? @_n "^test"))

((comment) @_c . (method_declaration) @test (#match? @_c "@test\\b"))

((class_declaration name: (name) @_n) @test (#match? @_n "Test$"))

((function_call_expression function: (name) @_f) @test
  (#match? @_f "^(test|it|describe)$"))
