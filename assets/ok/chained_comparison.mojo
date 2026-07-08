# Chained comparison `0 <= i < n` — each operand evaluated once, short-circuiting.
def in_range(i: Int, n: Int) -> Bool:
    return 0 <= i < n

def main():
    print(in_range(3, 5))
    print(in_range(7, 5))
    print(in_range(-1, 5))
