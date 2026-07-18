def observe_static(ref[StaticOrigin] value: Int):
    print(value)

def observe_untracked(ref[UntrackedOrigin] value: Int):
    print(value)

def mutate_unsafe(ref[UnsafeAnyOrigin] value: Int):
    value += 1

def main():
    var value = 41
    observe_untracked(value)
    mutate_unsafe(value)
    print(value)
