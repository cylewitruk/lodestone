; Supplementary Ruby tags: the doc comment of a class or module's first method, which the
; grammar attaches to the class rather than to the method's body. Evaluated before the
; grammar's tags.scm.
(
  (comment)* @doc
  .
  (body_statement
    .
    [
      (method name: (_) @name)
      (singleton_method name: (_) @name)
    ] @definition.method)
  (#strip! @doc "^#\\s*")
  (#select-adjacent! @doc @definition.method))
