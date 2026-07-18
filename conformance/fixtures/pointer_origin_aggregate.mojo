struct Borrowed[origin: Origin]:
    var ptr: UnsafePointer[Int, Self.origin]

struct ExternallyManaged:
    var ptr: UnsafePointer[Int, MutUntrackedOrigin]

def main():
    pass
