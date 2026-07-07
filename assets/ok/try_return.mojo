# A `return` inside a `try` crosses the boundary, running `finally` on the way out
# — and a `finally` that returns overrides. Runs identically on tree-walker and VM.
def classify(x: Int) -> String:
    try:
        if x > 0:
            return "pos"
        return "nonpos"
    finally:
        print("checked", x)

def main():
    print(classify(5))
    print(classify(-2))
