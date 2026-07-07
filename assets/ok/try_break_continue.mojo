# `break` / `continue` inside a `try` that target an *enclosing loop* cross the
# `try` boundary, running `finally` on the way out. This runs identically on the
# tree-walker and the VM: the escape lowers to an `EscapeJump` the VM propagates as
# a `Flow::Jump` to the outer loop (running each `finally`). A `return` crossing a
# `try` works the same way.
#
# (Still refused, cleanly, on the VM: a `break` targeting a loop declared *inside*
# an enclosing `try` — a region-local target the mini-CFG can't name.)
#
# Expected output:
#   fin 0
#   odd 1
#   fin 1
#   fin 2
#   fin 3
#   done
def main():
    for i in range(5):
        try:
            if i == 3:
                break
            if i % 2 == 0:
                continue
            print("odd", i)
        finally:
            print("fin", i)
    print("done")
