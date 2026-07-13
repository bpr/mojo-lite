# expect: Hashable
from std.collections.hashdict import HashDict

@fieldwise_init
struct Key(Copyable, Movable):
    var value: Int

def main():
    var values: HashDict[Key, Int] = HashDict[Key, Int]()
    values[Key(1)] = 2
