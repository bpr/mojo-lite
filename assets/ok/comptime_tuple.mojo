# Richer compile-time values: `comptime for` over a compile-time tuple of strings,
# and over a compile-time list of ints (data-driven unrolling).
comptime states = ("empty", "occupied", "deleted")
comptime sizes = [2, 4, 8]

def main():
    comptime for state in states:
        print(state)
    comptime for n in sizes:
        print(n * n)
