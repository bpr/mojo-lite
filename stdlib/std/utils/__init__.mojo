# Public import surface for compiler-provided utility types.
#
# Variant's storage and parameter-pack behavior are intrinsic to the checked
# type/MIR boundary.  This declaration deliberately supplies only the stdlib
# name: source must still use current Mojo's `from std.utils import Variant`,
# and the checker supplies `Variant[T, ...]` semantics after linking.
struct Variant:
    pass
