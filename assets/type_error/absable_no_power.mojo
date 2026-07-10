# expect: operator '**'
# Phase 7: a numeric-operation bound grants only its own operation. `Absable`
# permits `abs(x)`, but not `x ** y` — that needs a `Powable` bound. An opaque
# type parameter carries no operations beyond the ones its bounds promise.
def powit[T: Absable](x: T, y: T) -> T:
    return x ** y

def main():
    print(powit(2, 3))
