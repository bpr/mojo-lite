@fieldwise_init
struct Point:
    var x: Int

def main():
    comptime reflected = reflect[Point].field_type["x"]()
    var value: reflected.T = 1
    print(value)
