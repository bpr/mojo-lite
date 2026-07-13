def total(**kwargs: Int) -> Int:
    var result: Int = 0
    for key in kwargs:
        result = result + kwargs[key]
    return result

def main():
    print(total(first=1, second=2, third=3))
