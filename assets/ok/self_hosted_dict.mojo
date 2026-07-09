from ...stdlib.dict import Dict

def main():
    var d: Dict[String, String] = Dict[String, String]()
    d["name"] = "mojo-lite"
    d["phase"] = "self-host"
    print(d["name"], " : ",d["phase"])
    d["phase"] = "stdlib" # Overwrite existing value
    print(d["name"], " : ",d["phase"])

    var count: Int = 0
    for entry in d:
        count = count + 1
        print(entry.key)
        print(entry.value)
    print(count)

    # Check that copying a dictionary preserves value semantics
    var d2: Dict[String, String] = d.copy()
    print(d2["name"], " : ",d2["phase"])
    d2["phase"] = "copy"
    print(d2["name"], " : ",d2["phase"])
    print(d["name"], " : ",d["phase"])
