# A comptime `if` inside a generic value-parameter def is resolved per call:
# f[0] takes the `if` branch, f[1] the `else` (Phase 6 monomorphization).
def f[n: Int]() -> Int:
    comptime if n == 0:
        return 10
    else:
        return 20

def main():
    print(f[0](), f[1]())
