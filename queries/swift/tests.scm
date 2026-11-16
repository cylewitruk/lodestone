; XCTest `test*` methods and `*Tests` classes; Swift Testing `@Test` functions and `@Suite` types.
((function_declaration name: (simple_identifier) @_n) @test (#match? @_n "^test"))

((class_declaration name: (type_identifier) @_n) @test (#match? @_n "Tests?$"))

((function_declaration
  (modifiers (attribute (user_type (type_identifier) @_a)))) @test
  (#eq? @_a "Test"))

((class_declaration
  (modifiers (attribute (user_type (type_identifier) @_a)))) @test
  (#eq? @_a "Suite"))
