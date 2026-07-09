# expect: associated type 'size'
trait Fixed:
    comptime size: Int

def bad[C: Fixed](c: C) -> C.size:
    return 0
