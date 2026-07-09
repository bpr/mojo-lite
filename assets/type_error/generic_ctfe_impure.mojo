# expect: not a compile-time value
def bad() -> Int:
    print("no")
    return 1

comptime X = bad()
