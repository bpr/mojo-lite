# An integer literal coerces to both Int and Float64, so neither constructor
# overload is uniquely best.
# expect: ambiguous overloaded constructor
struct Box:
    var n: Int

    def __init__(out self, x: Int):
        self.n = x

    def __init__(out self, x: Float64):
        self.n = 0

def main():
    var b: Box = Box(1)
