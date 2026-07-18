def main():
    var total = 0
    for value in range(4):
        total += value
    else:
        print("for complete", total)

    var index = 0
    while index < 3:
        if index == 1:
            break
        index += 1
    else:
        print("not reached")
    print("break skipped else", index)
