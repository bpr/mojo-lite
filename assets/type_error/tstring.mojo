# Interpolated values must satisfy Writable.
# expect: Writable
struct Opaque:
    var value: Int

def greet(value: Opaque):
    print(t"opaque={value}")
