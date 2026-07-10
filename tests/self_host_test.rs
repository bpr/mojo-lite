//! Self-hosting proof (Phase 6, first installment): the `stdlib/` collection types
//! are written **in mojo-lite itself** — ordinary *generic* structs (`List[T]`,
//! `Optional[T]`, `Set[T]`, `Dict[K, V]`), no compiler intrinsic. Each test writes
//! a small entry program that imports through the bundled `stdlib/std/...` search
//! root and runs on the VM.

use mojo_lite::{BackendKind, check, elaborate, link};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mojo_lite_selfhost_{}_{}", std::process::id(), id));
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
    check(&program).map_err(|e| format!("type error: {e:?}"))?;
    let mut backend = BackendKind::Vm.make();
    backend
        .run(&program)
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
        "from std.collections.dict import Dict\n\ndef main():\n    var d: Dict[String, Int] = Dict[String, Int]()\n    d[\"a\"] = 10\n    d[\"b\"] = 20\n    d[\"a\"] = 15\n    print(len(d))\n    print(\"a\" in d, \"z\" in d)\n    print(d[\"a\"], d.get_or(\"z\", -1))\n    var total: Int = 0\n    for entry in d:\n        total = total + entry.value\n    print(total)\n",
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
