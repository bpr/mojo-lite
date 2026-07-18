@explicit_destroy("close the resource")
struct Resource(ImplicitlyDeletable where False):
    var id: Int

    def __init__(out self, id: Int):
        self.id = id

    def close(deinit self):
        print("closed", self.id)

def main():
    var resource = Resource(7)
    resource^.close()
