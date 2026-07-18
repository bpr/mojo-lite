def generic_size[T: Copyable & ImplicitlyDeletable](**options: T) -> Int:
    return len(options)


trait Counts:
    def count[Element: Copyable & ImplicitlyDeletable](self, **options: Element) -> Int: ...


@fieldwise_init
struct Counter(Counts):
    var bias: Int

    def size[T: Copyable & ImplicitlyDeletable](self, **options: T) -> Int:
        return self.bias + len(options)

    def count[Element: Copyable & ImplicitlyDeletable](self, **options: Element) -> Int:
        return self.bias + len(options)

    def relay(self, **options: Int) -> Int:
        return self.size(**options^)

    @staticmethod
    def static_size[T: Copyable & ImplicitlyDeletable](**options: T) -> Int:
        return len(options)


def count_through_bound[Target: Counts](target: Target, **options: Int) -> Int:
    return target.count(**options^)


def main():
    var counter = Counter(10)
    print(generic_size(first=1, second=2))
    print(counter.size(left="a", right="b"))
    print(counter.relay(one=1, two=2, three=3))
    print(Counter.static_size(a=1, b=2, c=3, d=4))
    print(count_through_bound(counter, left=1, right=2))
