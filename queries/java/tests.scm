; JUnit/TestNG test methods, and classes named `*Test` or `*Tests`.
((method_declaration
  (modifiers [(marker_annotation name: (identifier) @_a) (annotation name: (identifier) @_a)])) @test
  (#match? @_a "^(Test|ParameterizedTest|RepeatedTest|TestFactory|TestTemplate)$"))

((class_declaration name: (identifier) @_n) @test (#match? @_n "Tests?$"))
