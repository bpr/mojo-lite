# A self-hosted, generic `Optional[T]` — an ordinary mojo-lite struct (no compiler
# intrinsic). A present value or an absent one.
struct Optional[T: Copyable & Movable]:
    var present: Bool
    var val: Self.T

    def __init__(out self, val: Self.T, present: Bool):
        self.val = val
        self.present = present

    def is_some(self) -> Bool:
        return self.present

    def or_else(self, default: Self.T) -> Self.T:
        if self.present:
            return self.val
        return default
