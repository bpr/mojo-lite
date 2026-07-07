# Argument conventions on ordinary parameters: `mut` (a reference whose
# mutations are written back) and `owned` (takes ownership).
def update(mut total: Int, owned label: String):
    total = total + 1
