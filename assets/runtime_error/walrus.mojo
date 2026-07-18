# Walrus evaluation occurs before the later runtime failure.
# expect: boom
def main() raises:
    var first: Int = (n := 5)
    raise Error("boom")
