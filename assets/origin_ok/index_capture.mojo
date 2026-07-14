def element(ref values: List[Int], index: Int) -> ref[origin_of(values[index])] Int:
    return values[index]

def main():
    var values = List(3, 4)
    ref selected = element(values, 1)
    selected = 12
    print(values[1])
