# Phase 7: the self-hosted `math` module supplies `floor`/`ceil`/`trunc`/
# `ceildiv` (not prelude — imported here via a relative stdlib path, as the
# self-host algorithms fixture does). Each is generic over its trait bound; the
# concrete `Int`/`Float64` dunders run after type erasure.
from ...stdlib.math import floor, ceil, trunc, ceildiv

def main():
    print(floor(3.7), ceil(3.2), trunc(-3.7))
    print(floor(5), ceil(5), trunc(5))
    print(ceildiv(7, 2), ceildiv(-7, 2), ceildiv(8, 2))
    print(ceildiv(7.0, 2.0))
