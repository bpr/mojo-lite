# expect: is_same_type
# A type predicate has no runtime Bool form: used in a runtime `if` (not a comptime
# if) it is an unresolved name, so the program is rejected.
def name[T: AnyType]() -> String:
    if is_same_type[T, Int]():
        return "int"
    else:
        return "other"

def main():
    print(name[Int]())
