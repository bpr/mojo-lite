//! Overload rejection coverage (roadmap: "Close Small Correctness and Interface
//! Gaps" §1). Focused negative tests for **duplicate-equivalent declarations**
//! and **ambiguous coercion paths**, plus the positive resolution controls that
//! guard against over-tightening when overload ranking becomes more
//! sophisticated.
//!
//! The regression cases documented in `overload_errors.md` are required tests.
//! The rest of the suite pins correct-by-design behavior — including the deliberately
//! conservative ambiguity rejections that Mojo's full ranking would resolve.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use mojito::mir::lower_program;
use mojito::{BackendKind, TypeError, check, elaborate, link, parse};

/// Parse + elaborate + type-check, returning the checker's verdict.
fn check_source(source: &str) -> Result<(), TypeError> {
    let program = parse(source).expect("parse error");
    let program = elaborate(program).expect("comptime error");
    check(&program)
}

/// Type-check a program that is expected to pass.
fn ok(source: &str) {
    if let Err(e) = check_source(source) {
        panic!("expected the program to type-check, got: {e:?}");
    }
}

/// Type-check a program that is expected to fail, returning the error.
fn err(source: &str) -> TypeError {
    check_source(source).expect_err("expected a type error")
}

/// Assert the program is rejected as a redeclaration of `name`.
fn assert_redeclaration(source: &str, name: &str) {
    match err(source) {
        TypeError::Redeclaration(n) => assert_eq!(n, name),
        other => panic!("expected Redeclaration({name}), got: {other:?}"),
    }
}

/// Assert the program is rejected as an ambiguous overloaded call to `func`.
fn assert_ambiguous(source: &str, func: &str) {
    match err(source) {
        TypeError::BadCall { func: f, reason } => {
            assert_eq!(f, func);
            assert!(
                reason.contains("ambiguous"),
                "expected an ambiguity reason, got: {reason}"
            );
        }
        other => panic!("expected an ambiguous BadCall to {func}, got: {other:?}"),
    }
}

/// Run a checked program on the VM and return its captured output.
fn vm(source: &str) -> String {
    let program = parse(source).expect("parse error");
    let program = elaborate(program).expect("comptime error");
    check(&program).expect("type error");
    let mut backend = BackendKind::Vm.make();
    backend.run(&program).expect("runtime error");
    backend.output()
}

/// The lowered MIR function names for `source`, in emission order (duplicates
/// preserved — a repeated name is a symbol collision).
fn lowered_names(source: &str) -> Vec<String> {
    lower_program(&parse(source).expect("parse error"))
        .functions
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// A throwaway directory for module-linking tests (same pattern as
/// `module_test.rs`).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mojito_ovl_{}_{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::write(&path, contents).expect("write module file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn check_linked(entry: &Path) -> Result<(), TypeError> {
    let program = link(entry).expect("link error");
    check(&program)
}

// =========================================================================
// A. Duplicate-equivalent declarations
// =========================================================================

#[test]
fn rejects_duplicate_zero_arg_signature() {
    assert_redeclaration(
        "def f() -> Int:\n    return 0\n\ndef f() -> Int:\n    return 1\n",
        "f",
    );
}

#[test]
fn rejects_duplicate_differing_only_in_return_type() {
    // Return-type overloads are out of scope: same parameter types = duplicate.
    assert_redeclaration(
        "def f(x: Int) -> Int:\n    return x\n\ndef f(x: Int) -> Float64:\n    return 1.5\n",
        "f",
    );
}

#[test]
fn rejects_duplicate_differing_only_in_parameter_names() {
    // Parameter names are not part of the signature (no keyword-name-directed
    // overloading), so these are duplicate-equivalent.
    assert_redeclaration(
        "def f(a: Int) -> Int:\n    return a\n\ndef f(b: Int) -> Int:\n    return b + 1\n",
        "f",
    );
}

#[test]
fn rejects_duplicate_added_to_an_existing_overload_set() {
    assert_redeclaration(
        "def f(x: Int) -> Int:\n    return 0\n\ndef f(x: String) -> Int:\n    return 1\n\ndef f(x: Int) -> Int:\n    return 2\n",
        "f",
    );
}

#[test]
fn rejects_duplicate_via_canonicalized_type_spelling() {
    // `SIMD[DType.float64, 1]` *is* `Float64` (canonicalized in `ty_from_anno`),
    // so these spell the same signature.
    assert_redeclaration(
        "def f(x: Float64) -> Int:\n    return 0\n\ndef f(x: SIMD[DType.float64, 1]) -> Int:\n    return 1\n",
        "f",
    );
}

#[test]
fn rejects_defaulted_def_joining_an_overload_set() {
    // A def with a default has no fixed arity, so it cannot join a set —
    // conservative, but it must fail as a clean redeclaration (both orders).
    assert_redeclaration(
        "def f(x: Int) -> Int:\n    return x\n\ndef f(x: Int, y: Int = 0) -> Int:\n    return x + y\n",
        "f",
    );
    assert_redeclaration(
        "def f(x: Int, y: Int = 0) -> Int:\n    return x + y\n\ndef f(x: String) -> Int:\n    return len(x)\n",
        "f",
    );
}

#[test]
fn rejects_variadic_def_joining_an_overload_set() {
    assert_redeclaration(
        "def f(x: Int) -> Int:\n    return x\n\ndef f(*xs: String) -> Int:\n    return 0\n",
        "f",
    );
}

#[test]
fn rejects_exact_duplicate_generic_signature() {
    assert_redeclaration(
        "def f[T: AnyType](x: T) -> Int:\n    return 0\n\ndef f[T: AnyType](x: T) -> Int:\n    return 1\n",
        "f",
    );
}

#[test]
fn rejects_duplicate_method_differing_only_in_return_type() {
    assert_redeclaration(
        "@fieldwise_init\nstruct C:\n    var n: Int\n    def m(self) -> Int:\n        return self.n\n    def m(self) -> String:\n        return \"x\"\n",
        "m",
    );
}

#[test]
fn rejects_duplicate_method_differing_only_in_receiver_convention() {
    // Dispatch cannot depend on receiver mutability: `m(self)` and `m(mut self)`
    // with the same parameters are duplicate-equivalent.
    assert_redeclaration(
        "@fieldwise_init\nstruct C:\n    var n: Int\n    def m(self) -> Int:\n        return self.n\n    def m(mut self) -> Int:\n        return self.n\n",
        "m",
    );
}

#[test]
fn rejects_duplicate_constructor_signature() {
    assert_redeclaration(
        "struct C:\n    var n: Int\n    def __init__(out self, n: Int):\n        self.n = n\n    def __init__(out self, m: Int):\n        self.n = m + 1\n",
        "__init__",
    );
}

#[test]
fn rejects_two_mojo_copy_constructors() {
    // Both spell `__copyinit__` after the lifecycle rename, so they collide even
    // though each is written as an `__init__` overload.
    assert_redeclaration(
        "struct C:\n    var n: Int\n    def __init__(out self):\n        self.n = 0\n    def __init__(out self, *, copy: Self):\n        self.n = copy.n\n    def __init__(out self, *, copy: Self):\n        self.n = copy.n + 1\n",
        "__copyinit__",
    );
}

#[test]
fn rejects_fieldwise_init_plus_handwritten_init() {
    match err(
        "@fieldwise_init\nstruct C:\n    var n: Int\n    def __init__(out self):\n        self.n = 0\n",
    ) {
        TypeError::ConflictingConstructor(name) => assert_eq!(name, "C"),
        other => panic!("expected ConflictingConstructor, got: {other:?}"),
    }
}

#[test]
fn rejects_duplicate_trait_requirement() {
    assert_redeclaration(
        "trait T:\n    def m(self) -> Int: ...\n    def m(self) -> Float64: ...\n",
        "m",
    );
}

#[test]
fn rejects_var_then_def_and_def_then_var() {
    assert_redeclaration(
        "var f: Int = 1\n\ndef f(x: Int) -> Int:\n    return x\n",
        "f",
    );
    assert_redeclaration(
        "def f(x: Int) -> Int:\n    return x\n\nvar f: Int = 1\n",
        "f",
    );
}

#[test]
fn rejects_imported_duplicate_signature_across_modules() {
    let d = TempDir::new();
    d.write("lib.mojo", "def f(x: Int) -> Int:\n    return 10\n");
    let entry = d.write(
        "main.mojo",
        "from lib import f\n\ndef f(x: Int) -> Int:\n    return 20\n\ndef main():\n    print(f(1))\n",
    );
    match check_linked(&entry) {
        Err(TypeError::Redeclaration(n)) => assert_eq!(n, "f"),
        other => panic!("expected Redeclaration(f), got: {other:?}"),
    }
}

#[test]
fn rejects_alpha_equivalent_generic_overloads() {
    // `[T: AnyType](x: T)` and `[U: AnyType](x: U)` differ only by the parameter
    // name — duplicate-equivalent in substance (today they form a set and every
    // call is ambiguous).
    assert_redeclaration(
        "def f[T: AnyType](x: T) -> Int:\n    return 0\n\ndef f[U: AnyType](x: U) -> Int:\n    return 1\n",
        "f",
    );
}

#[test]
fn rejects_def_sharing_a_struct_name() {
    // Mojo has one namespace for types and functions; today the def is accepted
    // and the constructor becomes unreachable with a confusing later error.
    match err("@fieldwise_init\nstruct P:\n    var n: Int\n\ndef P(x: Int) -> Int:\n    return x\n")
    {
        TypeError::Redeclaration(n) => assert_eq!(n, "P"),
        other => panic!("expected Redeclaration(P), got: {other:?}"),
    }
}

#[test]
fn generic_overloads_differing_only_in_bounds_get_distinct_symbols() {
    // Accepted as an overload set (bounds differ), but both candidates currently
    // mangle to `f$ov$T`, so MIR emits two functions with one name and the VM
    // runs whichever comes first. Either the symbols must encode bounds or the
    // declaration must be rejected.
    let names = lowered_names(
        "def f[T: Copyable & Movable](x: T) -> Int:\n    return 0\n\ndef f[T: AnyType](x: T) -> Int:\n    return 1\n",
    );
    let f_names: Vec<&String> = names.iter().filter(|n| n.starts_with("f$ov$")).collect();
    assert_eq!(f_names.len(), 2, "both overloads should lower: {names:?}");
    assert_ne!(
        f_names[0], f_names[1],
        "lowered overload symbols must be distinct"
    );
}

// =========================================================================
// B. Ambiguous coercion paths (and no-match rejections)
// =========================================================================

#[test]
fn rejects_ambiguous_int_literal_between_int_and_float64() {
    assert_ambiguous(
        "def f(x: Int) -> Int:\n    return 0\n\ndef f(x: Float64) -> Int:\n    return 1\n\nvar r: Int = f(1)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_int_literal_between_int_and_uint() {
    assert_ambiguous(
        "def f(x: Int) -> Int:\n    return 0\n\ndef f(x: UInt) -> Int:\n    return 1\n\nvar r: Int = f(1)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_int_literal_between_uint_and_float64() {
    assert_ambiguous(
        "def f(x: UInt) -> Int:\n    return 0\n\ndef f(x: Float64) -> Int:\n    return 1\n\nvar r: Int = f(1)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_multi_argument_literal_tie() {
    // (Int, Float64) vs (Float64, Int): two literal coercions each — tied.
    assert_ambiguous(
        "def f(a: Int, b: Float64) -> Int:\n    return 0\n\ndef f(a: Float64, b: Int) -> Int:\n    return 1\n\nvar r: Int = f(1, 2)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_partial_literal_tie() {
    // Exact first argument, literal second: (Int, Int) and (Int, Float64) both
    // score one coercion. Mojo's ranking would prefer (Int, Int); mojito's
    // conservative coercion count rejects the tie (see overload_errors.md,
    // design notes) — this test pins the conservative behavior on purpose.
    assert_ambiguous(
        "def f(a: Int, b: Int) -> Int:\n    return 0\n\ndef f(a: Int, b: Float64) -> Int:\n    return 1\n\nvar i: Int = 3\nvar r: Int = f(i, 2)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_generic_overloads_when_both_bounds_hold() {
    // Two generic candidates whose bounds are both satisfied score identically.
    assert_ambiguous(
        "def f[T: Copyable & Movable](x: T) -> Int:\n    return 0\n\ndef f[T: AnyType](x: T) -> Int:\n    return 1\n\nvar r: Int = f(1)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_keyword_literal_call() {
    // Keyword binding does not disambiguate a literal coercion tie.
    assert_ambiguous(
        "def f(x: Int) -> Int:\n    return 0\n\ndef f(x: Float64) -> Int:\n    return 1\n\nvar r: Int = f(x=1)\n",
        "f",
    );
}

#[test]
fn rejects_ambiguous_constructor_literal() {
    match err(
        "struct Box:\n    var n: Int\n    def __init__(out self, x: Int):\n        self.n = x\n    def __init__(out self, x: Float64):\n        self.n = 0\n\nvar b: Box = Box(1)\n",
    ) {
        TypeError::BadCall { func, reason } => {
            assert_eq!(func, "Box");
            assert!(reason.contains("ambiguous"), "got: {reason}");
        }
        other => panic!("expected an ambiguous constructor BadCall, got: {other:?}"),
    }
}

#[test]
fn rejects_call_matching_no_overload() {
    match err(
        "def f(x: Int) -> Int:\n    return x\n\ndef f(x: String) -> Int:\n    return 1\n\nvar r: Int = f(True)\n",
    ) {
        TypeError::BadCall { func, reason } => {
            assert_eq!(func, "f");
            assert!(reason.contains("no overload"), "got: {reason}");
        }
        other => panic!("expected a no-overload BadCall, got: {other:?}"),
    }
}

#[test]
fn rejects_arity_matching_no_overload() {
    match err(
        "def f(x: Int) -> Int:\n    return x\n\ndef f(x: String) -> Int:\n    return 1\n\nvar r: Int = f()\n",
    ) {
        TypeError::BadCall { func, reason } => {
            assert_eq!(func, "f");
            assert!(reason.contains("no overload"), "got: {reason}");
        }
        other => panic!("expected a no-overload BadCall, got: {other:?}"),
    }
}

#[test]
fn overload_set_fully_shadows_a_builtin_name() {
    // A user overload set on `len` shadows the builtin entirely: the String
    // call no longer reaches the builtin — pinned so shadowing rules stay
    // deliberate.
    match err(
        "def len(x: Int) -> Int:\n    return x\n\ndef len(x: Bool) -> Int:\n    return 0\n\nvar r: Int = len(\"abc\")\n",
    ) {
        TypeError::BadCall { func, reason } => {
            assert_eq!(func, "len");
            assert!(reason.contains("no overload"), "got: {reason}");
        }
        other => panic!("expected a no-overload BadCall, got: {other:?}"),
    }
}

#[test]
fn reports_ambiguous_method_call_as_ambiguous() {
    // The method exists; the call is ambiguous. Today: "type 'Box' has no
    // method 'm'".
    match err(
        "@fieldwise_init\nstruct Box:\n    var n: Int\n    def m(self, x: Int) -> Int:\n        return x\n    def m(self, x: Float64) -> Int:\n        return 0\n\nvar b: Box = Box(1)\nvar r: Int = b.m(1)\n",
    ) {
        TypeError::BadCall { reason, .. } => {
            assert!(reason.contains("ambiguous"), "got: {reason}")
        }
        other => panic!("expected an ambiguous method BadCall, got: {other:?}"),
    }
}

#[test]
fn reports_substitution_induced_method_ambiguity_as_ambiguous() {
    // On `Pair[String]`, `m(Self.T)` and `m(String)` substitute to the same
    // signature; the tie must be reported as ambiguity, not "no method".
    match err(
        "@fieldwise_init\nstruct Pair[T: Copyable & Movable]:\n    var a: Self.T\n    def m(self, x: Self.T) -> Int:\n        return 0\n    def m(self, x: String) -> Int:\n        return 1\n\nvar p: Pair[String] = Pair(\"hi\")\nvar r: Int = p.m(\"x\")\n",
    ) {
        TypeError::BadCall { reason, .. } => {
            assert!(reason.contains("ambiguous"), "got: {reason}")
        }
        other => panic!("expected an ambiguous method BadCall, got: {other:?}"),
    }
}

#[test]
fn reports_no_match_method_call_as_bad_call() {
    match err(
        "@fieldwise_init\nstruct Box:\n    var n: Int\n    def m(self, x: Int) -> Int:\n        return x\n    def m(self, s: String) -> Int:\n        return 1\n\nvar b: Box = Box(1)\nvar r: Int = b.m(1, 2)\n",
    ) {
        TypeError::BadCall { reason, .. } => {
            assert!(reason.contains("no overload"), "got: {reason}")
        }
        other => panic!("expected a no-overload method BadCall, got: {other:?}"),
    }
}

#[test]
fn reports_constructor_no_match_as_bad_call() {
    match err(
        "@fieldwise_init\nstruct P:\n    var n: Int\n\nstruct Box:\n    var n: Int\n    def __init__(out self, n: Int):\n        self.n = n\n    def __init__(out self, s: String):\n        self.n = 0\n\nvar p: P = P(3)\nvar b: Box = Box(p)\n",
    ) {
        TypeError::BadCall { func, reason } => {
            assert_eq!(func, "Box");
            assert!(reason.contains("no"), "got: {reason}");
        }
        other => panic!("expected a no-overload constructor BadCall, got: {other:?}"),
    }
}

#[test]
fn concrete_overload_beats_generic_on_a_literal_argument() {
    // Mojo prefers concrete candidates; today the generic's constant score 0
    // beats the concrete candidate's literal-coercion score 1 and the generic
    // body runs.
    let out = vm(
        "def f[T: AnyType](x: T) -> Int:\n    return 0\n\ndef f(x: Int) -> Int:\n    return 1\n\ndef main():\n    print(f(1))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn concrete_overload_beats_generic_on_an_exact_argument() {
    let out = vm(
        "def f[T: AnyType](x: T) -> Int:\n    return 0\n\ndef f(x: Int) -> Int:\n    return 1\n\ndef main():\n    var i: Int = 7\n    print(f(i))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn nested_def_overloads_dispatch_correctly_or_are_rejected() {
    // Both nested `g`s lift to `outer$g`, so every call runs the first body.
    // Acceptable outcomes: correct dispatch, or a clean check-time rejection —
    // never a wrong answer.
    let src = "def outer() -> Int:\n    def g(x: Int) -> Int:\n        return x + 100\n    def g(x: String) -> Int:\n        return len(x)\n    return g(7) + g(\"abcd\")\n\ndef main():\n    print(outer())\n";
    match check_source(src) {
        Err(_) => {} // clean rejection is fine
        Ok(()) => assert_eq!(vm(src), "111\n"),
    }
}

// =========================================================================
// C. Positive resolution controls (must keep passing as ranking evolves)
// =========================================================================

#[test]
fn exact_typed_argument_beats_literal_coercion() {
    let out = vm(
        "def f(x: Int) -> Int:\n    return 1\n\ndef f(x: Float64) -> Int:\n    return 2\n\ndef main():\n    var i: Int = 7\n    print(f(i))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn float_literal_resolves_to_the_float64_candidate() {
    // A float literal coerces only to Float64, so there is no tie.
    let out = vm(
        "def f(x: Int) -> Int:\n    return 1\n\ndef f(x: Float64) -> Int:\n    return 2\n\ndef main():\n    print(f(2.5))\n",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn bool_argument_resolves_exactly_and_never_coerces() {
    let out = vm(
        "def f(x: Int) -> Int:\n    return 1\n\ndef f(x: Bool) -> Int:\n    return 2\n\ndef main():\n    print(f(True))\n",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn keyword_call_with_exact_argument_resolves() {
    let out = vm(
        "def f(x: Int) -> Int:\n    return 1\n\ndef f(x: String) -> Int:\n    return 2\n\ndef main():\n    var i: Int = 3\n    print(f(x=i))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn multi_argument_resolution_filters_non_coercing_candidates() {
    // (Float64, Float64) cannot accept a concrete Int, so (Int, Int) wins even
    // though the second argument is a literal.
    let out = vm(
        "def f(a: Int, b: Int) -> Int:\n    return 1\n\ndef f(a: Float64, b: Float64) -> Int:\n    return 2\n\ndef main():\n    var i: Int = 3\n    print(f(i, 2))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn failed_trait_bound_filters_a_generic_candidate_statically() {
    // A move-only struct fails `T: Copyable`, so only the AnyType candidate
    // remains — the checker must resolve, not report ambiguity. (Running it hits
    // the O-4 symbol collision; the runtime half lives in the ignored test
    // below.)
    ok(
        "struct MoveOnly:\n    var n: Int\n    def __init__(out self, n: Int):\n        self.n = n\n    def __moveinit__(out self, deinit take: Self):\n        self.n = take.n\n\ndef f[T: Copyable & Movable](x: T) -> Int:\n    return 0\n\ndef f[T: AnyType](x: T) -> Int:\n    return 1\n\nvar m: MoveOnly = MoveOnly(3)\nvar r: Int = f(m)\n",
    );
}

#[test]
fn failed_trait_bound_resolution_runs_the_selected_body() {
    let out = vm(
        "struct MoveOnly:\n    var n: Int\n    def __init__(out self, n: Int):\n        self.n = n\n    def __moveinit__(out self, deinit take: Self):\n        self.n = take.n\n\ndef f[T: Copyable & Movable](x: T) -> Int:\n    return 0\n\ndef f[T: AnyType](x: T) -> Int:\n    return 1\n\ndef main():\n    var m: MoveOnly = MoveOnly(3)\n    print(f(m))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn explicit_parameter_arguments_select_the_generic_candidate() {
    // `f[Int](i)`: the concrete candidate takes no compile-time parameters, so
    // the explicit bracket list selects the generic unambiguously.
    let out = vm(
        "def f[T: AnyType](x: T) -> Int:\n    return 1\n\ndef f(x: Int) -> Int:\n    return 2\n\ndef main():\n    var i: Int = 2\n    print(f[Int](i))\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn importing_a_name_extends_the_local_overload_set() {
    let d = TempDir::new();
    d.write("lib.mojo", "def f(x: Int) -> Int:\n    return 10\n");
    let entry = d.write(
        "main.mojo",
        "from lib import f\n\ndef f(x: String) -> Int:\n    return 20\n\ndef main():\n    var i: Int = 1\n    print(f(i), f(\"abc\"))\n",
    );
    let program = link(&entry).expect("link error");
    check(&program).expect("type error");
    let mut backend = BackendKind::Vm.make();
    backend.run(&program).expect("runtime error");
    assert_eq!(backend.output(), "10 20\n");
}
