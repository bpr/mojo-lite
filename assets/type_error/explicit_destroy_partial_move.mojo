# expect: is incomplete and cannot use a whole-value destructor
@explicit_destroy("close the resource")
struct Resource(ImplicitlyDeletable where False):
    var id: Int

    def __init__(out self, id: Int):
        self.id = id

    def close(deinit self):
        pass

def consume(var value: Int):
    pass

def main():
    var resource = Resource(1)
    consume(resource.id^)
    resource^.close()
