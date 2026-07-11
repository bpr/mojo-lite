//! Module system (Phase 3): `from module import …` links a referenced `.mojo`
//! file's top-level declarations into the program. These tests write a small
//! multi-file layout into a unique temp directory, link the entry file, then check
//! + run it on the VM.

use mojito::{BackendKind, LinkOptions, check, link, link_with_options};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A throwaway directory for one test's module files (best-effort cleanup on drop).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mojito_mod_{}_{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create subdir");
        }
        std::fs::write(&path, contents).expect("write module file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Link + check + run the entry file, returning its captured VM output.
fn run(entry: &Path) -> Result<String, String> {
    let program = link(entry).map_err(|e| e.to_string())?;
    check(&program).map_err(|e| format!("type error: {e:?}"))?;
    let mut backend = BackendKind::Vm.make();
    backend
        .run(&program)
        .map_err(|e| format!("runtime error: {e:?}"))?;
    Ok(backend.output())
}

#[test]
fn selective_import_brings_struct_and_fn_into_scope() {
    let d = TempDir::new();
    d.write(
        "collections.mojo",
        "struct Pair:\n    var a: Int\n    var b: Int\n    def __init__(out self, a: Int, b: Int):\n        self.a = a\n        self.b = b\n    def sum(self) -> Int:\n        return self.a + self.b\n\ndef twice(x: Int) -> Int:\n    return x * 2\n",
    );
    let main = d.write(
        "main.mojo",
        "from collections import Pair, twice\n\ndef main():\n    print(Pair(3, 4).sum())\n    print(twice(21))\n",
    );
    assert_eq!(run(&main).unwrap(), "7\n42\n");
}

#[test]
fn wildcard_and_relative_import() {
    let d = TempDir::new();
    d.write(
        "util.mojo",
        "def triple(x: Int) -> Int:\n    return x * 3\n",
    );
    let main = d.write(
        "main.mojo",
        "from .util import *\n\ndef main():\n    print(triple(5))\n",
    );
    assert_eq!(run(&main).unwrap(), "15\n");
}

#[test]
fn transitive_and_dotted_imports() {
    let d = TempDir::new();
    d.write(
        "pkg/base.mojo",
        "def base(x: Int) -> Int:\n    return x + 1\n",
    );
    d.write(
        "mid.mojo",
        "from pkg.base import base\n\ndef mid(x: Int) -> Int:\n    return base(x) * 10\n",
    );
    let main = d.write(
        "main.mojo",
        "from mid import mid\n\ndef main():\n    print(mid(4))\n",
    );
    assert_eq!(run(&main).unwrap(), "50\n");
}

#[test]
fn bundled_stdlib_root_supports_mojo_shaped_imports() {
    let d = TempDir::new();
    let main = d.write(
        "main.mojo",
        "from std.optional import Optional\nfrom std.collections.list import List\n\ndef main():\n    var o: Optional[Int] = Optional[Int](9, True)\n    var xs: List[Int] = List[Int]()\n    xs.append(o.or_else(0))\n    print(xs[0])\n",
    );
    assert_eq!(run(&main).unwrap(), "9\n");
}

#[test]
fn custom_search_root_is_used_after_importer_directory() {
    let d = TempDir::new();
    d.write("lib/pkg/tool.mojo", "def answer() -> Int:\n    return 42\n");
    let main = d.write(
        "src/main.mojo",
        "from pkg.tool import answer\n\ndef main():\n    print(answer())\n",
    );
    let program = link_with_options(
        &main,
        LinkOptions {
            search_roots: vec![d.0.join("lib")],
        },
    )
    .map_err(|e| e.to_string())
    .unwrap();
    check(&program).unwrap();
    let mut backend = BackendKind::Vm.make();
    backend.run(&program).unwrap();
    assert_eq!(backend.output(), "42\n");
}

#[test]
fn missing_module_and_missing_name_error() {
    let d = TempDir::new();
    d.write("m.mojo", "def f(x: Int) -> Int:\n    return x\n");
    let bad_mod = d.write(
        "bad1.mojo",
        "from nope import f\ndef main():\n    print(1)\n",
    );
    assert!(
        run(&bad_mod)
            .unwrap_err()
            .contains("cannot load module 'nope'")
    );
    let bad_name = d.write("bad2.mojo", "from m import g\ndef main():\n    print(1)\n");
    assert!(
        run(&bad_name)
            .unwrap_err()
            .contains("no declaration named 'g'")
    );
}
