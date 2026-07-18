def main():
    var value = 40
    ref first = value
    ref second = first
    second += 2
    print(value)
