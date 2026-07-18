def positive[value: Int where value > 0]() -> Int:
    return value

def main():
    print(positive[1]())
