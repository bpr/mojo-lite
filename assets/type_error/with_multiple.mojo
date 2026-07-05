# Multiple comma-separated context managers, one without an `as` binding.
# expect: with statement
def copy(src: String, dst: String):
    with open(src, "r") as f_in, open(dst, "w") as f_out:
        f_out.write(f_in.read())
