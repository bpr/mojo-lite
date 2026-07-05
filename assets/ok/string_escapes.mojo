# The full Mojo string-escape set decodes to real String values that run.
def main():
    # \x hex, octal \ooo, \u (4 hex), and \U (8 hex) all name a code point.
    var letters: String = "\x41\102C\U00000044"  # "ABCD"
    var greeting: String = "café"                 # "café"
    var tab: String = "a\tb"                           # a real tab
    print(letters, greeting, tab)
    print(len(greeting))                               # 5 bytes (é is 2)
