# expect: does not conform to trait 'Hasher'
struct BrokenHasher(Hasher):
    var state: UInt

    def __init__(out self):
        self.state = UInt(0)

    def update(mut self, value: UInt):
        self.state = self.state + value
