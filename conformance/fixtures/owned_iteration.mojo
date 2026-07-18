struct Item(Movable):
    var value: Int

    def __init__(out self, value: Int):
        self.value = value

    def __init__(out self, *, deinit move: Self):
        self.value = move.value

    def __del__(deinit self):
        print("drop", self.value)

def main():
    var items = [Item(1), Item(2), Item(3)]
    for var item in items^:
        print("take", item.value)
        if item.value == 2:
            break
    print("done")
