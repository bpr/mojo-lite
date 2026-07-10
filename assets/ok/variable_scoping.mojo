def main():
    x = 1 # Please ignore the warning for the moment
    y = 1
    if True: # Please ignore the warning for the moment
        x = 4
        print("inner x:", x)
        var y = 4
        print("inner y:", y)
    print("outer x:", x)
    print("outer y:", y)
