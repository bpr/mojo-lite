# Trait inheritance / refinement `trait Bird(Animal):` — parsed, semantics deferred.
# expect: trait inheritance
trait Animal:
    def eat(self):
        ...

trait Bird(Animal):
    def fly(self):
        ...
