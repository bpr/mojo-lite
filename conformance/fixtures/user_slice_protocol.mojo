@fieldwise_init
struct Window:
    var base: Int

    def __getitem__(self, part: Slice) -> Int:
        return self.base + part.start.or_else(0) + part.end.or_else(0) + part.step.or_else(1)

def main():
    var window = Window(10)
    print(window[1:5:2])
    print(window[:5])
