@explicit_destroy("close the child")
struct Child(ImplicitlyDeletable where False):
    var id: Int

    def __init__(out self, id: Int):
        self.id = id

    def close(deinit self):
        print("child", self.id)

@explicit_destroy("finish the aggregate")
struct Aggregate(ImplicitlyDeletable where False):
    var child: Child
    var count: Int

    def __init__(out self, child: Child, count: Int):
        self.child = child
        self.count = count

    def finish(deinit self):
        pass

def consume(var value: Int):
    pass

def main():
    var aggregate = Aggregate(Child(7), 2)
    aggregate.child^.close()
    consume(aggregate.count^)

    var rebuilt = Aggregate(Child(8), 3)
    rebuilt.child^.close()
    consume(rebuilt.count^)
    rebuilt.child = Child(9)
    rebuilt.count = 4
    rebuilt^.finish()
