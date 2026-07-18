@fieldwise_init
struct Scope:
    var value: Int
    def __enter__(self) -> Int:
        print("enter")
        return self.value
    def __exit__(self) raises:
        print("exit")
        raise Error("exit failed")

def main():
    try:
        with Scope(42) as value:
            print(value)
    except:
        print("caught exit")
