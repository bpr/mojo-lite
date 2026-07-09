# The built-in `is_same_type[T, U]()` type predicate drives a comptime branch on a
# type parameter (Phase 7): name[Int] takes the `int` branch, name[String] the else.
def name[T: AnyType]() -> String:
    comptime if is_same_type[T, Int]():
        return "int"
    else:
        return "other"

def main():
    print(name[Int]())
    print(name[String]())
