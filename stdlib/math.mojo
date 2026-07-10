# Self-hosted numeric rounding helpers (Phase 7). These mirror Mojo's `math`
# module: they are *not* prelude builtins (unlike `abs`/`round`/`divmod`), so
# they must be imported — `from math import floor, ceil, trunc, ceildiv`.
#
# Each is generic over the trait that supplies its dunder; the concrete numeric
# type's implementation runs after type erasure (`Int`/`Float64` have intrinsic
# `__floor__`/`__ceil__`/`__trunc__`/`__ceildiv__`).

def floor[T: Floorable](value: T) -> T:
    return value.__floor__()

def ceil[T: Ceilable](value: T) -> T:
    return value.__ceil__()

def trunc[T: Truncable](value: T) -> T:
    return value.__trunc__()

def ceildiv[T: CeilDivable](numerator: T, denominator: T) -> T:
    return numerator.__ceildiv__(denominator)
