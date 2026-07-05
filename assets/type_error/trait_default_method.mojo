# Trait default method (a real body instead of `...`) — parsed, semantics deferred.
# expect: trait default method
trait DefaultQuackable:
    def quack(self):
        print("Quack")
