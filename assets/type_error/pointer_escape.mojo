# An origin-bearing pointer cannot outlive the checked storage it designates.
# expect: returned pointer escapes
def escape() -> UnsafePointer[Int]:
    var local = 7
    return UnsafePointer(to=local)

def main():
    print(1)
