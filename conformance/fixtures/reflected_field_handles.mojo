struct Coordinates:
    var x: Int
    var y: Float64

struct Point:
    var coordinates: Coordinates

def main():
    comptime coordinates = reflect[Point].field["coordinates"]
    comptime y = coordinates.field_at[1]
    var value: y.T = 3.5
    print(value)
