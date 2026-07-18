//! Module system (Phase 3): `from module import …` links a referenced `.mojo`
//! file's top-level declarations into the program. These tests write a small
//! multi-file layout into a unique temp directory, link the entry file, then check
//! + run it on the VM.

use mojito::{BackendKind, LinkOptions, check_program, link, link_with_options};
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
    let checked = check_program(&program).map_err(|e| format!("type error: {e:?}"))?;
    let mut backend = BackendKind::Vm.make();
    backend
        .run(&checked)
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
    let checked = check_program(&program).unwrap();
    let mut backend = BackendKind::Vm.make();
    backend.run(&checked).unwrap();
    assert_eq!(backend.output(), "42\n");
}

#[test]
fn custom_search_roots_are_tried_in_order() {
    let d = TempDir::new();
    d.write(
        "first/pkg/tool.mojo",
        "def answer() -> Int:\n    return 1\n",
    );
    d.write(
        "second/pkg/tool.mojo",
        "def answer() -> Int:\n    return 2\n",
    );
    let main = d.write(
        "src/main.mojo",
        "from pkg.tool import answer\n\ndef main():\n    print(answer())\n",
    );
    let program = link_with_options(
        &main,
        LinkOptions {
            search_roots: vec![d.0.join("first"), d.0.join("second")],
        },
    )
    .unwrap();
    let checked = check_program(&program).unwrap();
    let mut backend = BackendKind::Vm.make();
    backend.run(&checked).unwrap();
    assert_eq!(backend.output(), "1\n");
}

#[test]
fn importer_directory_precedes_custom_search_roots() {
    let d = TempDir::new();
    d.write("root/pkg/tool.mojo", "def answer() -> Int:\n    return 1\n");
    d.write("src/pkg/tool.mojo", "def answer() -> Int:\n    return 9\n");
    let main = d.write(
        "src/main.mojo",
        "from pkg.tool import answer\n\ndef main():\n    print(answer())\n",
    );
    let program = link_with_options(
        &main,
        LinkOptions {
            search_roots: vec![d.0.join("root")],
        },
    )
    .unwrap();
    let checked = check_program(&program).unwrap();
    let mut backend = BackendKind::Vm.make();
    backend.run(&checked).unwrap();
    assert_eq!(backend.output(), "9\n");
}

#[test]
fn source_package_precedes_same_named_source_module() {
    let d = TempDir::new();
    d.write("choice.mojo", "def answer() -> Int:\n    return 1\n");
    d.write(
        "choice/__init__.mojo",
        "def answer() -> Int:\n    return 2\n",
    );
    let main = d.write(
        "main.mojo",
        "from choice import answer\n\ndef main():\n    print(answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "2\n");
}

#[test]
fn ordinary_directories_can_form_dotted_import_paths() {
    let d = TempDir::new();
    d.write(
        "plain/nested/tool.mojo",
        "def answer() -> Int:\n    return 42\n",
    );
    let main = d.write(
        "main.mojo",
        "import plain.nested.tool\n\ndef main():\n    print(plain.nested.tool.answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "42\n");
}

#[test]
fn linked_declarations_preserve_module_identity_in_checked_program() {
    let d = TempDir::new();
    let module = d.write("library.mojo", "def answer() -> Int:\n    return 42\n");
    let main = d.write(
        "main.mojo",
        "from library import answer\n\ndef main():\n    print(answer())\n",
    );
    let checked = mojito::check_program(&link(&main).unwrap()).unwrap();
    let answer = checked
        .statements()
        .iter()
        .find(|stmt| matches!(&stmt.kind, mojito::ast::StmtKind::Def { name, .. } if name.ends_with("answer")))
        .unwrap();
    let entry = checked
        .statements()
        .iter()
        .find(
            |stmt| matches!(&stmt.kind, mojito::ast::StmtKind::Def { name, .. } if name == "main"),
        )
        .unwrap();
    assert_eq!(answer.module.as_deref(), Some(module.to_str().unwrap()));
    assert_eq!(entry.module.as_deref(), Some(main.to_str().unwrap()));
}

#[test]
fn linked_expression_locations_include_their_source_module() {
    let d = TempDir::new();
    let library = d.write(
        "lib.mojo",
        "def pick(x: Int) -> Int:\n    return x\n\ndef pick(x: String) -> String:\n    return x\n\ndef from_lib() -> Int:\n    return pick(1)\n",
    );
    let entry = d.write(
        "main.mojo",
        "from lib import pick, from_lib\n\ndef pick(x: Bool) -> Bool:\n    return x\n\ndef main():\n    print(from_lib(), pick(True))\n",
    );
    let checked = mojito::check_program(&link(&entry).expect("link")).expect("check");

    let sources: std::collections::HashSet<_> = checked
        .overload_targets()
        .keys()
        .filter_map(|location| location.source.as_deref())
        .collect();
    assert!(sources.contains(library.to_str().unwrap()));
    assert!(sources.contains(entry.to_str().unwrap()));

    let from_lib = checked
        .statements()
        .iter()
        .find(|statement| matches!(&statement.kind, mojito::ast::StmtKind::Def { name, .. } if name.ends_with("from_lib")))
        .expect("imported function");
    let mojito::ast::StmtKind::Def { body, .. } = &from_lib.kind else {
        unreachable!()
    };
    let mojito::ast::StmtKind::Return(Some(call)) = &body[0].kind else {
        panic!("expected return call")
    };
    assert_eq!(call.source.as_deref(), Some(library.to_str().unwrap()));
    assert_eq!(body[0].module.as_deref(), Some(library.to_str().unwrap()));
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

#[test]
fn aliases_qualified_imports_and_same_named_declarations_do_not_collide() {
    let d = TempDir::new();
    d.write("left.mojo", "def answer() -> Int:\n    return 1\n");
    d.write("right.mojo", "def answer() -> Int:\n    return 2\n");
    let main = d.write(
        "main.mojo",
        "from left import answer as left_answer\nimport right as r\n\ndef main():\n    print(left_answer(), r.answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "1 2\n");
}

#[test]
fn unaliased_dotted_import_uses_the_full_qualified_path() {
    let d = TempDir::new();
    d.write("pkg/__init__.mojo", "");
    d.write("pkg/tool.mojo", "def answer() -> Int:\n    return 42\n");
    let main = d.write(
        "main.mojo",
        "import pkg.tool\n\ndef main():\n    print(pkg.tool.answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "42\n");
}

#[test]
fn dotted_import_prefix_is_shadowed_as_one_namespace_tree() {
    let d = TempDir::new();
    d.write("pkg/__init__.mojo", "");
    d.write("pkg/tool.mojo", "def answer() -> Int:\n    return 42\n");
    let main = d.write(
        "main.mojo",
        "import pkg.tool\n\ndef echo(pkg: Int) -> Int:\n    return pkg\n\ndef main():\n    print(echo(7))\n",
    );
    assert_eq!(run(&main).unwrap(), "7\n");
}

#[test]
fn dotted_namespace_resolves_exported_types() {
    let d = TempDir::new();
    d.write("pkg/__init__.mojo", "");
    d.write(
        "pkg/models.mojo",
        "@fieldwise_init\nstruct Box:\n    var value: Int\n",
    );
    let main = d.write(
        "main.mojo",
        "import pkg.models\n\ndef main():\n    var box: pkg.models.Box = pkg.models.Box(9)\n    print(box.value)\n",
    );
    assert_eq!(run(&main).unwrap(), "9\n");
}

#[test]
fn local_bindings_shadow_imported_members() {
    let d = TempDir::new();
    d.write("values.mojo", "comptime value = 41\n");
    let main = d.write(
        "main.mojo",
        "from values import value\n\ndef main():\n    var value: Int = 7\n    print(value)\n",
    );
    assert_eq!(run(&main).unwrap(), "7\n");
}

#[test]
fn imports_inside_functions_and_blocks_are_lexically_scoped() {
    let d = TempDir::new();
    d.write("util.mojo", "def answer() -> Int:\n    return 42\n");
    let main = d.write(
        "main.mojo",
        "def main():\n    from util import answer\n    if True:\n        import util as nested\n        print(nested.answer())\n    print(answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "42\n42\n");

    let bad = d.write(
        "bad.mojo",
        "def main():\n    if True:\n        from util import answer\n        print(answer())\n    print(answer())\n",
    );
    assert!(run(&bad).unwrap_err().contains("answer"));
}

#[test]
fn package_init_reexports_members() {
    let d = TempDir::new();
    d.write("tools/value.mojo", "def answer() -> Int:\n    return 42\n");
    d.write("tools/__init__.mojo", "from .value import answer\n");
    let main = d.write(
        "main.mojo",
        "from tools import answer\n\ndef main():\n    print(answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "42\n");
}

#[test]
fn package_init_can_reexport_a_submodule_namespace() {
    let d = TempDir::new();
    d.write("tools/value.mojo", "def answer() -> Int:\n    return 42\n");
    d.write("tools/__init__.mojo", "from . import value\n");
    let main = d.write(
        "main.mojo",
        "import tools\n\ndef main():\n    print(tools.value.answer())\n",
    );
    assert_eq!(run(&main).unwrap(), "42\n");
}

#[test]
fn package_submodule_requires_reexport_or_explicit_import() {
    let d = TempDir::new();
    d.write("tools/__init__.mojo", "");
    d.write("tools/value.mojo", "def answer() -> Int:\n    return 42\n");
    let direct = d.write(
        "direct.mojo",
        "import tools.value\n\ndef main():\n    print(tools.value.answer())\n",
    );
    assert_eq!(run(&direct).unwrap(), "42\n");

    let hidden = d.write(
        "hidden.mojo",
        "import tools\n\ndef main():\n    print(tools.value.answer())\n",
    );
    assert!(run(&hidden).is_err());
}

#[test]
fn sibling_modules_are_not_implicitly_visible() {
    let d = TempDir::new();
    d.write("pkg/__init__.mojo", "");
    d.write("pkg/tool.mojo", "def answer() -> Int:\n    return 42\n");
    d.write(
        "pkg/use.mojo",
        "def indirect() -> Int:\n    return tool.answer()\n",
    );
    let main = d.write(
        "main.mojo",
        "from pkg.use import indirect\n\ndef main():\n    print(indirect())\n",
    );
    assert!(run(&main).unwrap_err().contains("tool"));
}

#[test]
fn dots_only_relative_import_binds_a_sibling_module_namespace() {
    let d = TempDir::new();
    d.write("pkg/__init__.mojo", "");
    d.write("pkg/tool.mojo", "def answer() -> Int:\n    return 42\n");
    d.write(
        "pkg/use.mojo",
        "from . import tool\n\ndef indirect() -> Int:\n    return tool.answer()\n",
    );
    let main = d.write(
        "main.mojo",
        "from pkg.use import indirect\n\ndef main():\n    print(indirect())\n",
    );
    assert_eq!(run(&main).unwrap(), "42\n");
}

#[test]
fn wildcard_import_hides_underscore_prefixed_declarations() {
    let d = TempDir::new();
    d.write(
        "api.mojo",
        "def shown() -> Int:\n    return 1\n\ndef _hidden() -> Int:\n    return 2\n",
    );
    let main = d.write(
        "main.mojo",
        "from api import *\n\ndef main():\n    print(shown())\n",
    );
    assert_eq!(run(&main).unwrap(), "1\n");
    let bad = d.write(
        "bad.mojo",
        "from api import *\n\ndef main():\n    print(_hidden())\n",
    );
    assert!(run(&bad).unwrap_err().contains("_hidden"));
}

#[test]
fn imported_trait_effect_types_are_rewritten_with_their_module() {
    let d = TempDir::new();
    d.write(
        "validation.mojo",
        "@fieldwise_init\nstruct ValidationError:\n    var reason: String\n\ntrait Validates:\n    def validate(self) raises ValidationError -> Int: ...\n\n@fieldwise_init\nstruct Validator(Validates):\n    var value: Int\n    def validate(self) raises ValidationError -> Int:\n        if self.value < 0:\n            raise ValidationError(\"negative\")\n        return self.value\n\ndef invoke[T: Validates](value: T) raises ValidationError -> Int:\n    return value.validate()\n",
    );
    let main = d.write(
        "main.mojo",
        "from validation import ValidationError, Validator, invoke\n\ndef main() raises ValidationError:\n    print(invoke(Validator(7)))\n",
    );

    assert_eq!(run(&main).unwrap(), "7\n");
}
