def main():
    comptime counter: Int = 1
    print(counter) # Prints 1
    def inner():
        counter = counter + 3 # Outer constant is used here, but a new local variable with the same name is created
        print(counter) # Prints 4
    inner()
    print(counter) # Prints 1 from outer constant
