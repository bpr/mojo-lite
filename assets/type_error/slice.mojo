# Slice subscript — parsed, semantics deferred.
# expect: slice subscript
def mid(xs: List[Int]) -> List[Int]:
    return xs[1:3]
