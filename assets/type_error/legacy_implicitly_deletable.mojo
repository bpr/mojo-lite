# expect: unknown trait 'ImplicitlyDestructible'
struct Legacy(ImplicitlyDestructible):
    var value: Int

def main():
    pass
