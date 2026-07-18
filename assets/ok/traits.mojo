trait Animal:
    def make_sound(self): ...

struct Dog(Animal):
    def __init__(out self):
        pass

    def make_sound(self):
        print("Woof!")

def bark[T: Animal](imm a: T):
    a.make_sound()

def main():
    var d = Dog()
    bark(d)
