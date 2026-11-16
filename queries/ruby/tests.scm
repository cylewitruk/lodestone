; Minitest test methods and `*Test` classes, and RSpec/Minitest-spec blocks.
((method name: (identifier) @_n) @test (#match? @_n "^test_"))

((class name: (constant) @_n) @test (#match? @_n "Test$"))

((call method: (identifier) @_m) @test
  (#match? @_m "^(describe|context|it|specify|example|feature|scenario|shared_examples|shared_context)$"))
