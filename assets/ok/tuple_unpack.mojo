# Tuple unpacking `x, y = t` — binds each target to the corresponding element.
var point: Tuple[Int, Int] = (3, 4)
var x: Int = 0
var y: Int = 0
x, y = point
