# A method with a `mut` ordinary parameter writes the mutated argument back to the
# caller — runs identically on the tree-walker and the VM.
@fieldwise_init
struct Counter:
    var n: Int
    def add_into(self, mut dest: Counter):
        dest.n = dest.n + self.n

def main():
    var a: Counter = Counter(10)
    var b: Counter = Counter(5)
    a.add_into(b)
    print(b.n)
    a.add_into(b)
    print(b.n)
