# Integer bases, digit separators, and single/triple-quoted strings all run.
def main():
    var mask: Int = 0xFF_00
    var big: Int = 1_000_000
    var bits: Int = 0b1010
    var name: String = 'Mojo'
    var doc: String = """multi
line"""
    print(mask, big, bits, name)
