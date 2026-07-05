# A `ref[origin] T` return type (Mojo requires an origin on a ref return) —
# parsed, semantics deferred. `origin_of(x)` parses as an ordinary call expression.
# expect: reference type
def first(x: Int) -> ref[origin_of(x)] Int:
    return x
