# A self-hosted, generic `Optional[T]` — an ordinary mojito struct (no compiler
# intrinsic). A present value or an absent one.
struct Optional[T: Copyable & Movable]:
    var values: List[Self.T]

    def __init__(out self):
        self.values = List[Self.T]()

    def __init__(out self, val: Self.T, present: Bool):
        self.values = List[Self.T]()
        if present:
            self.values.append(val)

    def is_some(self) -> Bool:
        return len(self.values) == 1

    def or_else(self, default: Self.T) -> Self.T:
        if self.is_some():
            return self.values[0]
        return default
