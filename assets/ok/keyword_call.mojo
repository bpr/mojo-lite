# Keyword arguments at a call site (Mojo functions manual).
def my_pow(base: Int, exp: Int) -> Int:
    return base ** exp

def main():
    print(my_pow(exp=3, base=2))
    print(my_pow(2, exp=4))
