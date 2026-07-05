# `comptime if` (compile-time conditional) — parsed, semantics deferred.
# The modern Mojo spelling; the older `@parameter if` is deprecated.
# expect: comptime if
comptime WIDTH = 8
comptime if WIDTH > 4:
    var mode: Int = 1
else:
    var mode: Int = 0
