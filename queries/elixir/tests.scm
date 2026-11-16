; ExUnit `test`/`describe` blocks and `*Test` modules.
((call target: (identifier) @_t) @test (#any-of? @_t "test" "describe" "property"))

((call target: (identifier) @_m (arguments (alias) @_n)) @test
  (#eq? @_m "defmodule")
  (#match? @_n "Test$"))
