# The walrus operator parses and type-checks but is unsupported at eval.
# expect: walrus
var first: Int = (n := 5)
