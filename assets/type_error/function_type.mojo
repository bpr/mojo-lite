# A function-typed parameter — parsed, semantics deferred.
# expect: function type annotation
def apply(cb: def(Int) thin -> Int, x: Int) -> Int:
    return cb(x)
