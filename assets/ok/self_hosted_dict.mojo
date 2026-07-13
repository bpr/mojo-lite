from std.collections.dict import Dict

def main():
    var d: Dict[String, String] = Dict[String, String]()
    d["name"] = "mojito"
    d["phase"] = "self-host"
    print(d["name"], " : ",d["phase"])
    d["phase"] = "stdlib" # Overwrite existing value
    print(d["name"], " : ",d["phase"])

    var count: Int = 0
    for key in d:
        count = count + 1
        print(key)
        print(d[key])
    print(count)

    try:
        print(d["missing"])
    except e:
        print("Caught error: ", e)

    # Check that copying a dictionary preserves value semantics
    var d2: Dict[String, String] = d.copy()
    print(d2["name"], " : ",d2["phase"])
    d2["phase"] = "copy"
    print(d2["name"], " : ",d2["phase"])
    print(d["name"], " : ",d["phase"])
