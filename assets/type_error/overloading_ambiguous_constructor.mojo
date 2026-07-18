# An integer literal converts to both UInt and Float64, and neither is the
# literal's contextual default type, so neither constructor overload is
# uniquely best.
# expect: ambiguous overloaded constructor
struct Box:
    var n: Int

    def __init__(out self, x: UInt):
        self.n = 0

    def __init__(out self, x: Float64):
        self.n = 0

def main():
    var b: Box = Box(1)
