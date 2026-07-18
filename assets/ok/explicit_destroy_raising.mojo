@explicit_destroy("finish the transaction")
struct Transaction(ImplicitlyDeletable where False):
    var id: Int

    def __init__(out self, id: Int):
        self.id = id

    def commit(deinit self) raises:
        raise Error("commit failed")

    def rollback(deinit self):
        print("rolled back", self.id)

def main():
    var transaction = Transaction(9)
    try:
        transaction^.commit()
    except:
        transaction^.rollback()
