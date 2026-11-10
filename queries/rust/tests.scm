; Nodes that mark Rust test code: functions carrying a test attribute (`#[test]`,
; `#[tokio::test]`, `#[rstest]`, rstest_reuse `#[apply(..)]`, `#[test_case(..)]`), possibly with
; other attributes or comments in between, and `#[cfg(test)]` items.
((attribute_item (attribute (identifier) @_attr))
  .
  [(attribute_item) (line_comment) (block_comment)]*
  .
  (function_item) @test
  (#match? @_attr "^(test|rstest|apply|test_case|template|quickcheck|proptest)$"))

((attribute_item (attribute (scoped_identifier name: (identifier) @_attr)))
  .
  [(attribute_item) (line_comment) (block_comment)]*
  .
  (function_item) @test
  (#match? @_attr "^(test|rstest)$"))

((attribute_item (attribute (identifier) @_cfg arguments: (token_tree (identifier) @_test)))
  .
  [(attribute_item) (line_comment) (block_comment)]*
  .
  (_) @test
  (#eq? @_cfg "cfg") (#eq? @_test "test"))
