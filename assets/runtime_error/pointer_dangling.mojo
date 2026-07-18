# A dangling placeholder is non-null identity but never dereferenceable.
# expect: dangling
def main():
    var pointer = UnsafePointer[Int].dangling()
    print(pointer[0])
