//! Self-hosting proof (Phase 6, first installment): the `stdlib/` collection types
//! are written **in mojito itself** — ordinary *generic* structs (`List[T]`,
//! `Optional[T]`, `Set[T]`, `Dict[K, V]`), no compiler intrinsic. Each test writes
//! a small entry program that imports through the bundled `stdlib/std/...` search
//! root and runs on the VM.

use mojito::{BackendKind, elaborate, link};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mojito_selfhost_{}_{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::write(&path, contents).expect("write entry");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(entry: &Path) -> Result<String, String> {
    let program = link(entry).map_err(|e| e.to_string())?;
    let program = elaborate(program).map_err(|e| format!("comptime error: {e}"))?;
    let checked = mojito::check_program(&program).map_err(|e| format!("type error: {e:?}"))?;
    let mut backend = BackendKind::Vm.make();
    backend
        .run(&checked)
        .map_err(|e| format!("runtime error: {e:?}"))?;
    Ok(backend.output())
}

#[test]
fn self_hosted_generic_optional() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.optional import Optional\n\ndef main():\n    var a: Optional[Int] = Optional[Int](42, True)\n    var b: Optional[Int] = Optional[Int](0, False)\n    print(a.is_some(), a.or_else(-1))\n    print(b.is_some(), b.or_else(-1))\n",
    );
    assert_eq!(run(&main).unwrap(), "true 42\nfalse -1\n");
}

#[test]
fn self_hosted_generic_list_grows_indexes_iterates() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var xs: List[Int] = List[Int]()\n    var i: Int = 0\n    while i < 10:\n        xs.append(i * i)\n        i = i + 1\n    print(len(xs))\n    print(xs[0], xs[9])\n    xs[0] = 100\n    var total: Int = 0\n    for x in xs:\n        total = total + x\n    print(total)\n",
    );
    // 10 elements (grew past cap 4); 0²=0, 9²=81; sum = 100 + (1+4+…+81) = 385.
    assert_eq!(run(&main).unwrap(), "10\n0 81\n385\n");
}

#[test]
fn self_hosted_generic_list_has_value_semantics() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var a: List[Int] = List[Int]()\n    a.append(1)\n    a.append(2)\n    var b: List[Int] = a\n    b.append(99)\n    b[0] = 555\n    print(len(a), len(b))\n    print(a[0], b[0])\n",
    );
    // `var b = a` deep-copies via __copyinit__ — b's mutations don't touch a.
    assert_eq!(run(&main).unwrap(), "2 3\n1 555\n");
}

#[test]
fn self_hosted_generic_set_deduplicates_contains_iterates() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.set import Set\n\ndef main():\n    var s: Set[Int] = Set[Int]()\n    s.add(3)\n    s.add(3)\n    s.add(5)\n    print(len(s))\n    print(3 in s, 4 in s)\n    var total: Int = 0\n    for x in s:\n        total = total + x\n    print(total)\n",
    );
    assert_eq!(run(&main).unwrap(), "2\ntrue false\n8\n");
}

#[test]
fn self_hosted_generic_dict_sets_gets_updates_iterates() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.dict import Dict\n\ndef main():\n    var d: Dict[String, Int] = Dict[String, Int]()\n    d[\"a\"] = 10\n    d[\"b\"] = 20\n    d[\"a\"] = 15\n    print(len(d))\n    print(\"a\" in d, \"z\" in d)\n    print(d[\"a\"], d.get(\"z\", -1))\n    var total: Int = 0\n    for key in d:\n        total = total + d[key]\n    print(total)\n",
    );
    assert_eq!(run(&main).unwrap(), "2\ntrue false\n15 -1\n35\n");
}

#[test]
fn self_hosted_hash_backed_set() {
    // Phase 6: a hash-backed `HashSet[T]` (buckets chosen via `key.__hash__()`)
    // works for two key types — `Int` (intrinsic scalar hash) and `String`.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.hashset import HashSet\n\ndef main():\n    var s: HashSet[Int] = HashSet[Int]()\n    s.add(3)\n    s.add(3)\n    s.add(11)\n    s.add(19)\n    print(len(s))\n    print(s.contains(11), s.contains(4))\n    var w: HashSet[String] = HashSet[String]()\n    w.add(\"mojo\")\n    w.add(\"lite\")\n    w.add(\"mojo\")\n    print(len(w))\n    print(w.contains(\"lite\"), w.contains(\"rust\"))\n",
    );
    assert_eq!(run(&main).unwrap(), "3\ntrue false\n2\ntrue false\n");
}

// --- Nested self-hosted lists (roadmap §2: the hash-set bucket-array shape) ---
//
// Characterization matrix for `List[List[T]]` where `List` is the self-hosted
// `std.collections.list` struct. The `#[ignore]`d tests assert the *correct*
// value semantics and graduate when the VM copy/write-back fixes land.

#[test]
fn nested_list_builds_and_reads_chained_index() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var m: List[List[Int]] = List[List[Int]]()\n    var r0: List[Int] = List[Int]()\n    r0.append(1)\n    r0.append(2)\n    var r1: List[Int] = List[Int]()\n    r1.append(3)\n    m.append(r0)\n    m.append(r1)\n    print(len(m))\n    print(m[0][0], m[0][1], m[1][0])\n    print(len(m[1]))\n    var total: Int = 0\n    for x in m[0]:\n        total = total + x\n    print(total)\n",
    );
    assert_eq!(run(&main).unwrap(), "2\n1 2 3\n1\n3\n");
}

#[test]
fn nested_list_append_copies_row() {
    // Passing a row to `append` copies it (by-value argument): later mutation of
    // the original row must not reach the stored one.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var m: List[List[Int]] = List[List[Int]]()\n    var row: List[Int] = List[Int]()\n    row.append(1)\n    m.append(row)\n    row[0] = 42\n    row.append(7)\n    print(m[0][0], len(m[0]))\n",
    );
    assert_eq!(run(&main).unwrap(), "1 1\n");
}

#[test]
fn nested_list_copy_is_deep() {
    // `var n = m` must deep-copy the rows: mutating a row read out of the copy
    // (and stored back into the copy) must not reach the original.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var m: List[List[Int]] = List[List[Int]]()\n    var row: List[Int] = List[Int]()\n    row.append(1)\n    m.append(row)\n    var n: List[List[Int]] = m\n    var r: List[Int] = n[0]\n    r[0] = 99\n    n[0] = r\n    print(m[0][0], n[0][0])\n",
    );
    assert_eq!(run(&main).unwrap(), "1 99\n");
}

#[test]
fn nested_list_getitem_returns_a_copy() {
    // `m[0]` yields a value-semantic copy of the row, not an alias of its buffer.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var m: List[List[Int]] = List[List[Int]]()\n    var row: List[Int] = List[Int]()\n    row.append(1)\n    m.append(row)\n    var r: List[Int] = m[0]\n    r[0] = 77\n    print(m[0][0], r[0])\n",
    );
    assert_eq!(run(&main).unwrap(), "1 77\n");
}

#[test]
fn nested_list_mut_method_through_index_chain() {
    // `m[0].append(5)` — a mutating method through an indexed place: read the
    // row, mutate the copy, write it back via `__setitem__`.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\ndef main():\n    var m: List[List[Int]] = List[List[Int]]()\n    var row: List[Int] = List[Int]()\n    row.append(1)\n    m.append(row)\n    m[0].append(5)\n    print(len(m[0]), m[0][1])\n",
    );
    assert_eq!(run(&main).unwrap(), "2 5\n");
}

#[test]
fn nested_list_as_struct_field_bucket_shape() {
    // The exact hash-set shape: `self.buckets[i].append(v)` inside a `mut self`
    // method, with `buckets: List[List[Int]]` a field.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.list import List\n\nstruct Grid:\n    var buckets: List[List[Int]]\n\n    def __init__(out self):\n        self.buckets = List[List[Int]]()\n        self.buckets.append(List[Int]())\n        self.buckets.append(List[Int]())\n\n    def add(mut self, i: Int, v: Int):\n        self.buckets[i].append(v)\n\n    def total(self, i: Int) -> Int:\n        var t: Int = 0\n        for x in self.buckets[i]:\n            t = t + x\n        return t\n\ndef main():\n    var g: Grid = Grid()\n    g.add(0, 3)\n    g.add(1, 4)\n    g.add(0, 5)\n    print(g.total(0), g.total(1))\n",
    );
    assert_eq!(run(&main).unwrap(), "8 4\n");
}

#[test]
fn self_hosted_hashset_copy_and_list_shadowing() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.hashset import HashSet\nfrom std.collections.list import List\n\ndef main():\n    var s: HashSet[Int] = HashSet[Int]()\n    s.add(1)\n    var t: HashSet[Int] = s.copy()\n    t.add(9)\n    print(len(s), len(t), s.contains(9), t.contains(9))\n    var xs: List[Int] = List[Int]()\n    xs.append(7)\n    print(xs[0])\n",
    );
    assert_eq!(run(&main).unwrap(), "1 2 false true\n7\n");
}

#[test]
fn self_hosted_dict_views_get_and_snapshots() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.dict import Dict\n\ndef main():\n    var d: Dict[String, Int] = Dict[String, Int]()\n    d[\"a\"] = 1\n    d[\"b\"] = 2\n    var keys = d.keys()\n    var values = d.values()\n    var items = d.items()\n    d[\"c\"] = 3\n    print(len(keys), len(values), len(items), len(d))\n    print(keys[0], keys[1], values[0], values[1])\n    print(items[0].key, items[0].value)\n    print(d.get(\"a\").is_some(), d.get(\"z\").is_some())\n    print(d.get(\"z\", 99))\n    for key in d:\n        print(key, d[key])\n",
    );
    assert_eq!(
        run(&main).unwrap(),
        "2 2 2 3\na b 1 2\na 1\ntrue false\n99\na 1\nb 2\nc 3\n"
    );
}

#[test]
fn hash_dict_matches_list_dict_and_preserves_order() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.dict import Dict\nfrom std.collections.hashdict import HashDict\n\ndef main():\n    var a: Dict[Int, Int] = Dict[Int, Int]()\n    var b: HashDict[Int, Int] = HashDict[Int, Int]()\n    var i: Int = 0\n    while i < 20:\n        a[i] = i * 10\n        b[i] = i * 10\n        i = i + 1\n    a[3] = 333\n    b[3] = 333\n    print(len(a), len(b), b.bucket_count())\n    for key in a:\n        print(key, a[key])\n    print(\"---\")\n    for key in b:\n        print(key, b[key])\n    print(b.get(100).is_some(), b.get(100, -1))\n",
    );
    let output = run(&main).unwrap();
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("20 20 32"));
    let rest: Vec<&str> = lines.collect();
    let divider = rest.iter().position(|line| *line == "---").unwrap();
    assert_eq!(&rest[..divider], &rest[divider + 1..divider * 2 + 1]);
    assert_eq!(rest.last(), Some(&"false -1"));
}

#[test]
fn hash_dict_copy_has_value_semantics() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.hashdict import HashDict\n\ndef main():\n    var a: HashDict[String, Int] = HashDict[String, Int]()\n    a[\"x\"] = 1\n    var b = a.copy()\n    b[\"x\"] = 9\n    b[\"y\"] = 2\n    print(len(a), a[\"x\"], \"y\" in a)\n    print(len(b), b[\"x\"], \"y\" in b)\n",
    );
    assert_eq!(run(&main).unwrap(), "1 1 false\n2 9 true\n");
}

#[test]
fn hash_dict_missing_subscript_raises() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.collections.hashdict import HashDict\n\ndef main():\n    var d: HashDict[String, Int] = HashDict[String, Int]()\n    try:\n        print(d[\"missing\"])\n    except e:\n        print(e)\n",
    );
    assert_eq!(run(&main).unwrap(), "Error(\"missing key\")\n");
}

#[test]
fn kwargs_are_owned_self_hosted_hash_dicts() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "def show(prefix: Int, **options: Int):\n    print(prefix, len(options))\n    for key in options:\n        print(key, options[key])\n    options[\"local\"] = 9\n    print(options.get(\"missing\", -1), len(options))\n\ndef main():\n    show(7, first=1, second=2)\n    show(8)\n",
    );
    assert_eq!(
        run(&main).unwrap(),
        "7 2\nfirst 1\nsecond 2\n-1 3\n8 0\n-1 1\n"
    );
}

#[test]
fn self_hosted_math_rounding_helpers() {
    // Phase 7: the self-hosted `math` module (not prelude — must be imported)
    // exposes `floor`/`ceil`/`trunc`/`ceildiv`, generic over their trait bounds;
    // built-in `Int`/`Float64` supply the dunders intrinsically after erasure.
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.math import floor, ceil, trunc, ceildiv\n\ndef main():\n    print(floor(3.7), ceil(3.2), trunc(-3.7))\n    print(floor(5), ceil(5))\n    print(ceildiv(7, 2), ceildiv(-7, 2))\n    print(ceildiv(7.0, 2.0))\n",
    );
    assert_eq!(run(&main).unwrap(), "3.0 4.0 -3.0\n5 5\n4 -3\n4.0\n");
}

#[test]
fn self_hosted_algorithms_use_comptime_facts() {
    let main = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("ok")
        .join("self_hosted_algorithms.mojo");
    assert_eq!(run(&main).unwrap(), "1 2 0\n8 24\n4 17\n42\nfallback\n7\n");
}
