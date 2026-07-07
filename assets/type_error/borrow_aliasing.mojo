# expect: borrowed mutably and also used
def swap_add(mut a: Int, b: Int):
    a = a + b

def main():
    var x: Int = 5
    swap_add(x, x)
