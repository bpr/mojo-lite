# `comptime for` (compile-time, unrolled loop) — parsed, semantics deferred.
# The modern Mojo spelling; the older `@parameter for` is deprecated.
# expect: comptime for
def main():
    comptime for i in range(4):
        print(i)
