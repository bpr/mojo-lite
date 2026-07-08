//! Self-hosting proof (Phase 6, first installment): the `stdlib/` collection types
//! are written **in mojo-lite itself** — ordinary *generic* structs (`List[T]`,
//! `Optional[T]`), no compiler intrinsic. Each test copies the real `stdlib/*.mojo`
//! files into a temp directory alongside a small entry program, links them
//! (`from module import …`), and runs on the VM.

use mojo_lite::{BackendKind, check, link};
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
    /// Copy a real `stdlib/<name>` file into the temp dir (so imports resolve there).
    fn add_stdlib(&self, name: &str) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib").join(name);
        std::fs::copy(&src, self.0.join(name))
            .unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
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
    d.add_stdlib("optional.mojo");
    let main = d.write(
        "main.mojo",
        "from optional import Optional\n\ndef main():\n    var a: Optional[Int] = Optional[Int](42, True)\n    var b: Optional[Int] = Optional[Int](0, False)\n    print(a.is_some(), a.or_else(-1))\n    print(b.is_some(), b.or_else(-1))\n",
    );
    assert_eq!(run(&main).unwrap(), "true 42\nfalse -1\n");
}

#[test]
fn self_hosted_generic_list_grows_indexes_iterates() {
    let d = TempDir::new();
    d.add_stdlib("list.mojo");
    let main = d.write(
        "main.mojo",
        "from list import List\n\ndef main():\n    var xs: List[Int] = List[Int]()\n    var i: Int = 0\n    while i < 10:\n        xs.append(i * i)\n        i = i + 1\n    print(len(xs))\n    print(xs[0], xs[9])\n    xs[0] = 100\n    var total: Int = 0\n    for x in xs:\n        total = total + x\n    print(total)\n",
    );
    // 10 elements (grew past cap 4); 0²=0, 9²=81; sum = 100 + (1+4+…+81) = 385.
    assert_eq!(run(&main).unwrap(), "10\n0 81\n385\n");
}

#[test]
fn self_hosted_generic_list_has_value_semantics() {
    let d = TempDir::new();
    d.add_stdlib("list.mojo");
    let main = d.write(
        "main.mojo",
        "from list import List\n\ndef main():\n    var a: List[Int] = List[Int]()\n    a.append(1)\n    a.append(2)\n    var b: List[Int] = a\n    b.append(99)\n    b[0] = 555\n    print(len(a), len(b))\n    print(a[0], b[0])\n",
    );
    // `var b = a` deep-copies via __copyinit__ — b's mutations don't touch a.
    assert_eq!(run(&main).unwrap(), "2 3\n1 555\n");
}
