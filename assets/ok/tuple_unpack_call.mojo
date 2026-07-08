# Unpacking a tuple returned from a function (evaluated once).
def pair() -> Tuple[Int, String]:
    return (1, "one")

var a: Int = 0
var b: String = ""
a, b = pair()
