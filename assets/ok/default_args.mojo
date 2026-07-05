# Optional argument with a default value (Mojo functions manual).
def my_pow(base: Int, exp: Int = 2) -> Int:
    return base ** exp

def main():
    print(my_pow(3))
    print(my_pow(3, 3))
