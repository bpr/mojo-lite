from std.algorithms import StaticSized, type_tag, default_capacity, capacity_blocks, static_size, first_or
from std.collections.list import List
from std.collections.set import Set

struct Tiny(StaticSized):
    comptime size = 4

struct Wide(StaticSized):
    comptime size = 17

comptime TINY_SIZE = static_size[Tiny]()
comptime WIDE_SIZE = static_size[Wide]()

def main():
    print(type_tag[Int](), type_tag[String](), type_tag[Bool]())
    print(default_capacity(), capacity_blocks[3]())
    print(TINY_SIZE, WIDE_SIZE)
    var xs: List[Int] = List[Int]()
    xs.append(42)
    print(first_or[List[Int]](xs, -1))
    var empty: List[String] = List[String]()
    print(first_or[List[String]](empty, "fallback"))
    var s: Set[Int] = Set[Int]()
    s.add(7)
    print(first_or[Set[Int]](s, -1))
