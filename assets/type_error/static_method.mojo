# A @staticmethod (no `self` receiver) — parsed, semantics deferred.
# expect: staticmethod
struct Factory:
    var seed: Int

    @staticmethod
    def create(x: Int) -> Int:
        return x
