from list import List

struct Set[T: Equatable & Copyable & Movable]:
    var items: List[Self.T]

    def __init__(out self):
        self.items = List[Self.T]()

    def __contains__(self, value: Self.T) -> Bool:
        for item in self.items:
            if item == value:
                return True
        return False

    def add(mut self, value: Self.T):
        if not (value in self):
            self.items.append(value)

    def __len__(self) -> Int:
        return len(self.items)

    def __iter__(self) -> List[Self.T]:
        return self.items
