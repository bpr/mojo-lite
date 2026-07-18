def main():
    comptime counter: Int = 1
    print(counter) # Prints 1
    def inner() unified {imm counter}:
        var local = counter + 3
        print(local) # Prints 4
    inner()
    print(counter) # Prints 1 from outer constant
