# expect: escapes storage
def bad(ref source: Int) -> ref[source] Int:
    var local = 1
    return local
