# Argument conventions on ordinary parameters (mut/owned).
# expect: argument conventions
def update(mut total: Int, owned label: String):
    total = total + 1
