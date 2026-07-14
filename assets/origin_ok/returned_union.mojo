def choose(ref left: Int, ref right: Int, first: Bool) -> ref[left, right] Int:
    if first:
        return left
    return right

def main():
    var left = 1
    var right = 2
    ref selected = choose(left, right, False)
    selected = 9
    print(left, right)
