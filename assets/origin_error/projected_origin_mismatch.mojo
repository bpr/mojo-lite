# expect: escapes storage
@fieldwise_init
struct Pair:
    var left: Int
    var right: Int
    def bad(ref self) -> ref[origin_of(self.left)] Int:
        return self.right
