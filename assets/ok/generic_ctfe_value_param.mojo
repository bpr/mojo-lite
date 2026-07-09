def choose_width[n: Int]() -> Int:
    if n < 8:
        return 8
    return n

comptime W = choose_width[4]()

comptime if W == 8:
    pass
else:
    var wrong: Int = "choose_width[4] should be 8"

def main():
    print(W)
