# A t-string (interpolation) — parsed into sub-expressions, semantics deferred.
# expect: t-string
def greet(name: String, count: Int):
    print(t"Hello {name}, you have {count + 1} messages")
