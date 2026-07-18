def captured_total() -> Int:
    var total = 0
    var base = 40
    var offset = 2
    def add() unified {mut total, offset^, imm}:
        total = base + offset
    add()
    return total

def main():
    print(captured_total())
