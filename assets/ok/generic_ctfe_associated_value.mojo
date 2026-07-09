trait Fixed:
    comptime size: Int

struct Buffer[n: Int](Fixed):
    comptime size = Self.n
    var tag: Int

def capacity[T: Fixed]() -> Int:
    return T.size

comptime C = capacity[Buffer[8]]()

comptime if C == 8:
    pass
else:
    var wrong: Int = "capacity[Buffer[8]] should be 8"

def main():
    print(C)
