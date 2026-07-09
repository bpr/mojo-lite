# expect: return
# Instantiating f[1] selects the `else` branch, which returns a String from an
# `-> Int` function — a type error surfaced only because that branch is taken.
def f[n: Int]() -> Int:
    comptime if n == 0:
        return 1
    else:
        return "bad"

def main():
    print(f[1]())
