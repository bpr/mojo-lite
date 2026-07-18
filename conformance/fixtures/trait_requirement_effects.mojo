@fieldwise_init
struct ValidationError:
    var reason: String

trait Fallible:
    def run(self) raises ValidationError -> Int: ...

@fieldwise_init
struct Worker(Fallible):
    var value: Int

    def run(self) raises ValidationError -> Int:
        if self.value < 0:
            raise ValidationError("negative")
        return self.value

def invoke[T: Fallible](value: T) raises ValidationError -> Int:
    return value.run()

def main():
    try:
        print(invoke(Worker(7)))
    except error:
        print(error.reason)
    try:
        _ = invoke(Worker(-1))
    except error:
        print(error.reason)
