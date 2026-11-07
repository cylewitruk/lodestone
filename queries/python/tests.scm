; Python test code: test_* functions and Test* classes.
((function_definition name: (identifier) @_n) @test (#match? @_n "^test_"))
((class_definition name: (identifier) @_n) @test (#match? @_n "^Test"))
