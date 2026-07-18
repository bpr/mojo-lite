# Writing the owner while an origin-bearing pointer is live is a loan conflict.
# expect: conflicts with live reference
def main():
    var x = 1
    var p = UnsafePointer(to=x)
    x = 5
    print(p[0])
