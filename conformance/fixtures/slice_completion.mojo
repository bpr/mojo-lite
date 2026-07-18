@fieldwise_init
struct Window:
    var size: Int

    def __getitem__(self, part: Slice) -> Int:
        var normalized = part.indices(self.size)
        return normalized[0] + normalized[1] + normalized[2]


@fieldwise_init
struct Grid:
    var last: Int

    def __getitem__(self, row: Int, columns: Slice) -> Int:
        var normalized = columns.indices(10)
        return row * 100 + normalized[0] + normalized[1] + normalized[2]

    def __setitem__(mut self, row: Int, columns: Slice, value: Int):
        var normalized = columns.indices(10)
        self.last = row * 100 + normalized[0] + normalized[1] + normalized[2] + value


def main():
    var values = [0, 1, 2, 3, 4, 5]
    print(values[1:5:2])
    print(values[::-1])

    var window = Window(10)
    print(window[:5])
    print(window[::-1])

    var grid = Grid(0)
    print(grid[3, 1:8:2])
    grid[3, 1:8:2] = 9
    print(grid.last)
