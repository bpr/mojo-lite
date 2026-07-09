# Trait `comptime` member requirements are associated facts. A conforming struct
# must define each required fact explicitly.
# expect: missing comptime member 'Element'
trait HasElement:
    comptime Element: AnyType

@fieldwise_init
struct Box[T: AnyType](HasElement):
    var value: Self.T
