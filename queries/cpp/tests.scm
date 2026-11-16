; GoogleTest, Catch2, doctest and Boost.Test test cases.
((function_definition
  declarator: (function_declarator declarator: (identifier) @_m)) @test
  (#match? @_m "^(TEST|TEST_F|TEST_P|TYPED_TEST|TYPED_TEST_P|TEST_CASE|TEST_CASE_METHOD|SCENARIO|BOOST_AUTO_TEST_CASE|BOOST_FIXTURE_TEST_CASE|BOOST_AUTO_TEST_SUITE)$"))
