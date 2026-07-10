# Phase 7: the self-hosted `math` module supplies `floor`/`ceil`/`trunc`/
# `ceildiv` (not prelude — imported here through the stdlib search root).
# Each is generic over its trait bound; the
# concrete `Int`/`Float64` dunders run after type erasure.
from std.math import floor, ceil, trunc, ceildiv

def main():
    print(floor(3.7), ceil(3.2), trunc(-3.7))
    print(floor(5), ceil(5), trunc(5))
    print(ceildiv(7, 2), ceildiv(-7, 2), ceildiv(8, 2))
    print(ceildiv(7.0, 2.0))
