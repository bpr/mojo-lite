# try / except / else / finally with a raising and a non-raising path.
def risky(fail: Bool) -> Int:
    if fail:
        raise "failed"
    return 42

def main():
    try:
        var x: Int = risky(False)
        print("got", x)
    except e:
        print("caught in 1")
    else:
        print("no error")
    finally:
        print("finally 1")

    try:
        var y: Int = risky(True)
        print("unreached")
    except e:
        print("caught in 2")
    finally:
        print("finally 2")
