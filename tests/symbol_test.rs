//! Tests for the canonical overload-symbol module (`src/symbol.rs`): the one
//! owner of signature identity and `$ov$` lowered-name formatting. They pin the
//! external spellings, prove the checker-recorded callee names the exact MIR
//! function (no drift between the two manglings), and scan the source tree so a
//! hand-built overload symbol outside the module is caught.

use std::collections::HashSet;

use mojito::checker::resolve_overload_targets;
use mojito::mir::lower_program;
use mojito::parse;

/// The lowered function names `lower_program` emits for `src`.
fn lowered_names(src: &str) -> HashSet<String> {
    lower_program(&parse(src).expect("parse error"))
        .functions
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn free_function_overloads_get_signature_qualified_names() {
    let names = lowered_names(
        "def pick() -> Int:\n    return 0\n\
         def pick(x: Int) -> Int:\n    return x\n\
         def pick(s: String) -> String:\n    return s\n",
    );
    assert!(names.contains("pick$ov$"), "zero-arg overload: {names:?}");
    assert!(names.contains("pick$ov$Int"), "{names:?}");
    assert!(names.contains("pick$ov$String"), "{names:?}");
}

#[test]
fn non_overloaded_def_keeps_its_source_name() {
    let names = lowered_names("def solo(x: Int) -> Int:\n    return x\n");
    assert!(names.contains("solo"), "{names:?}");
}

#[test]
fn method_and_constructor_overloads_get_qualified_names() {
    let names = lowered_names(
        "struct Box:\n    var n: Int\n\
         \n    def __init__(out self):\n        self.n = 0\n\
         \n    def __init__(out self, n: Int):\n        self.n = n\n\
         \n    def value(self) -> Int:\n        return self.n\n\
         \n    def value(self, add: Int) -> Int:\n        return self.n + add\n",
    );
    assert!(names.contains("Box.__init__$ov$"), "{names:?}");
    assert!(names.contains("Box.__init__$ov$Int"), "{names:?}");
    assert!(names.contains("Box.value$ov$"), "{names:?}");
    assert!(names.contains("Box.value$ov$Int"), "{names:?}");
}

#[test]
fn mojo_copy_constructor_counts_as_copyinit_not_an_init_overload() {
    // One ordinary `__init__` plus the `out self, *, copy: Self` form: the copy
    // constructor is modeled as `__copyinit__`, so neither is overloaded.
    let names = lowered_names(
        "struct Res:\n    var n: Int\n\
         \n    def __init__(out self, n: Int):\n        self.n = n\n\
         \n    def __init__(out self, *, copy: Self):\n        self.n = copy.n\n",
    );
    assert!(names.contains("Res.__init__"), "{names:?}");
    assert!(names.contains("Res.__copyinit__"), "{names:?}");
}

#[test]
fn struct_and_generic_parameter_types_mangle_from_their_annotations() {
    let names = lowered_names(
        "@fieldwise_init\nstruct Point:\n    var x: Int\n\
         @fieldwise_init\nstruct Pair[T: AnyType]:\n    var a: Self.T\n    var b: Self.T\n\
         def pick(p: Point) -> Int:\n    return p.x\n\
         def pick(n: Int) -> Int:\n    return n\n\
         def pick(q: Pair[Int]) -> Int:\n    return q.a\n",
    );
    assert!(names.contains("pick$ov$Point"), "{names:?}");
    assert!(names.contains("pick$ov$Int"), "{names:?}");
    assert!(names.contains("pick$ov$Pair$Int"), "{names:?}");
}

#[test]
fn nested_defs_lift_to_dollar_joined_names() {
    let names = lowered_names(
        "def outer(x: Int) -> Int:\n\
         \x20   def inner(y: Int) -> Int:\n\
         \x20       return y + x\n\
         \x20   return inner(1)\n",
    );
    assert!(names.contains("outer$inner"), "{names:?}");
}

/// The drift regression: every callee the checker records for an overloaded
/// call must name a function the MIR actually emits — including struct-typed,
/// generic, and `Self.T`-typed parameters, which previously mangled differently
/// on the two sides (`pick$ov$Struct$Point` vs `pick$ov$Point`).
#[test]
fn checker_recorded_callees_name_real_mir_functions() {
    let src = "@fieldwise_init\nstruct Point:\n    var x: Int\n\
         @fieldwise_init\nstruct Pair[T: AnyType]:\n    var a: Self.T\n    var b: Self.T\n\
         struct Box:\n    var n: Int\n\
         \n    def __init__(out self):\n        self.n = 0\n\
         \n    def __init__(out self, n: Int):\n        self.n = n\n\
         \n    def get(self) -> Int:\n        return self.n\n\
         \n    def get(self, p: Point) -> Int:\n        return self.n + p.x\n\
         def pick(p: Point) -> Int:\n    return p.x\n\
         def pick(n: Int) -> Int:\n    return n + 1\n\
         def pick(q: Pair[Int]) -> Int:\n    return q.a\n\
         def main():\n\
         \x20   print(pick(Point(7)))\n\
         \x20   print(pick(1))\n\
         \x20   print(pick(Pair(1, 2)))\n\
         \x20   var b: Box = Box(5)\n\
         \x20   print(b.get(), b.get(Point(3)))\n";
    let program = parse(src).expect("parse error");
    let targets = resolve_overload_targets(&program).expect("check error");
    let names: HashSet<String> = lower_program(&program)
        .functions
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(!targets.is_empty(), "expected recorded overload targets");
    for target in targets.values() {
        assert!(
            names.contains(target),
            "checker target '{target}' names no MIR function; emitted: {names:?}"
        );
    }
}

/// Repository hygiene: the `$ov$` spelling may exist only in the canonical
/// symbol module. A new hand-built overload symbol anywhere else in `src/`
/// reintroduces the checker/MIR/VM drift this module exists to prevent.
#[test]
fn ov_spelling_appears_only_in_the_symbol_module() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    scan_rs_files(&root, &mut offenders);
    assert!(
        offenders.is_empty(),
        "'$ov$' outside src/symbol.rs — route it through mojito::symbol: {offenders:?}"
    );
}

fn scan_rs_files(dir: &std::path::Path, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan_rs_files(&path, offenders);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().is_some_and(|f| f != "symbol.rs")
            && std::fs::read_to_string(&path)
                .expect("read source file")
                .contains("$ov$")
        {
            offenders.push(path.display().to_string());
        }
    }
}
