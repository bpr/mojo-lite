# expect: explicit-destroy obligation for 'resource' was abandoned: close the resource
@explicit_destroy("close the resource")
struct Resource(ImplicitlyDeletable where False):
    var id: Int

    def __init__(out self, id: Int):
        self.id = id

    def close(deinit self):
        pass

def main():
    var resource = Resource(1)
    print(resource.id)
