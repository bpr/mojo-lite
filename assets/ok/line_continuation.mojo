# A backslash before a newline continues a statement onto the next line (the
# continued line's indentation is not significant).
def main():
    var total: Int = 1 + \
        2 + \
        3
    print(total)
