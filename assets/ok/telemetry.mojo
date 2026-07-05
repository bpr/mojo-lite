def scan_stats() -> Tuple[Int, Int]:
    var num_scans = 4
    var total_points = 512
    return (total_points, num_scans)


def main():
    var stats = scan_stats()
    var scans = stats[1]
    var points = stats[0]
    print("scans:", scans)
    print("points:", points)
