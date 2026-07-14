@fieldwise_init
struct Box:
    var value: Int
    def get(ref self) -> ref[origin_of(self.value)] Int:
        return self.value

def main():
    var box = Box(3)
    ref value = box.get()
    value = 8
    print(box.value)
