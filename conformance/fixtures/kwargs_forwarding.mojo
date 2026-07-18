def show(prefix: Int, **options: Int):
    print(prefix, len(options))


def relay(**options: Int):
    show(prefix=7, **options^)


def main():
    relay(left=20, right=22)
