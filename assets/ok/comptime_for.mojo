# `comptime for` — compile-time unrolling. The loop variable is substituted with
# its literal value in each unrolled copy of the body.
def main():
    comptime for i in range(4):
        print(i, i * i)
