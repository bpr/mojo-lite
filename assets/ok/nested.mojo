def outer():
    def nested():
        print("I am nested")
    nested()

def main():
    outer()
