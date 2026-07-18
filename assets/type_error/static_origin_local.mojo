# Static-origin references cannot be manufactured from local storage.
# expect: cannot satisfy StaticOrigin
def observe(ref[StaticOrigin] value: Int):
    print(value)

def main():
    var local = 1
    observe(local)
