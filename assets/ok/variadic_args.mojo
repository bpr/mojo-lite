# Homogeneous variadic *args (Mojo functions manual).
def sum(*values: Int) -> Int:
    var total: Int = 0
    for value in values:
        total = total + value
    return total

def main():
    print(sum())
    print(sum(1, 2, 3))
    print(sum(10, 20, 30, 40))
