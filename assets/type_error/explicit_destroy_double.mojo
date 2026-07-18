# expect: was destroyed more than once
@explicit_destroy("close the resource")
struct Resource(ImplicitlyDeletable where False):
    def __init__(out self):
        pass

    def close(deinit self):
        pass

def main():
    var resource = Resource()
    resource^.close()
    resource^.close()
