; NUnit, xUnit and MSTest test methods and fixtures.
((method_declaration
  (attribute_list (attribute name: (identifier) @_a))) @test
  (#match? @_a "^(Test|TestCase|TestCaseSource|Theory|Fact|TestMethod|DataTestMethod)$"))

((class_declaration
  (attribute_list (attribute name: (identifier) @_a))) @test
  (#match? @_a "^(TestFixture|TestClass)$"))
