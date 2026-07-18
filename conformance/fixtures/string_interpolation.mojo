@fieldwise_init
struct Point(Writable):
    var x: Int
    var y: Int

def main():
    var point = Point(2, 5)
    print(t"point={point}, sum={point.x + point.y}")
