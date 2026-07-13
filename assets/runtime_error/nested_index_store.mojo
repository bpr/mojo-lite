# expect: cannot index
from std.collections.list import List

def main():
    var matrix: List[List[Int]] = List[List[Int]]()
    var row: List[Int] = List[Int]()
    row.append(1)
    matrix.append(row)
    matrix[0][0] = 9
