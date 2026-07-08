# A struct cannot declare both @fieldwise_init and a hand-written __init__:
# each defines a constructor (the decorator generates __init__).
# expect: both @fieldwise_init and a hand-written __init__
@fieldwise_init
struct Counter:
    var n: Int

    def __init__(out self, start: Int):
        self.n = start
