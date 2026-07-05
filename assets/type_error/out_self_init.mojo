# Hand-written __init__ with `out self` — parsed, semantics deferred.
# expect: out self
@fieldwise_init
struct Counter:
    var n: Int

    def __init__(out self, start: Int):
        self.n = start
