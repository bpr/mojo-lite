# Tuple unpacking `x, y = t` — parsed and grammar-documented, semantics deferred.
# expect: tuple unpacking
var point: Tuple[Int, Int] = (3, 4)
var x: Int = 0
var y: Int = 0
x, y = point
