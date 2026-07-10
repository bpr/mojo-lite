# Minimal self-hosted iterator traits used by generic algorithms.

trait Iterator:
    comptime Element: AnyType

    def __len__(self) -> Int:
        ...

    def __next__(mut self) -> Self.Element:
        ...

trait Iterable:
    comptime Element: AnyType
    comptime Iter: AnyType

    def __iter__(self) -> Self.Iter:
        ...
