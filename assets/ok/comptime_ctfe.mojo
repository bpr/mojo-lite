# CTFE: a pure function runs at compile time to compute a `comptime` constant, and
# module-level comptime constants materialize into functions as literals.
def next_pow2(n: Int) -> Int:
    var p: Int = 1
    while p < n:
        p = p * 2
    return p

comptime CAP = next_pow2(17)
comptime NAME = "buf" + "fer"

def label() -> String:
    return NAME

def main():
    print(CAP)
    print(label())
