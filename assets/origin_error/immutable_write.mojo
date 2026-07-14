# expect: must be mutable
def bad[origin: Origin[mut=False]](ref[origin] value: Int):
    value = 2
