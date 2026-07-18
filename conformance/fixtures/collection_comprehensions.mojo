def main() raises:
    var values = {3, 1, 3, 2}
    print(len(values), 1 in values, 9 in values)

    var mapping = {"a": 1, "b": 2, "a": 9}
    print(len(mapping), mapping["a"], "b" in mapping)
    var empty: Dict[String, Int] = {}
    print(len(empty))

    var squares = [x * x for x in range(6) if x % 2 == 0]
    var products = [x * 10 + y for x in range(2) for y in range(3)]
    var residues = {x % 3 for x in range(7)}
    var table = {x: x * x for x in range(4)}
    print(squares)
    print(products)
    print(residues)
    print(table)
