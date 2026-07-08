# `comptime if` — compile-time branch selection. Only the taken branch is kept
# (and type-checked); the others are dropped before runtime lowering.
comptime WIDTH = 8

def main():
    comptime if WIDTH > 4:
        print("wide")
    elif WIDTH > 0:
        print("narrow")
    else:
        print("empty")
