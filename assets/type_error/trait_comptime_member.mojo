# Trait `comptime` member requirement (associated compile-time constant/alias) —
# parsed, semantics deferred.
# expect: trait comptime member
trait Repeater:
    comptime count: Int
    def repeat(self):
        ...
