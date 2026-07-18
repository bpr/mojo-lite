//! Register-VM execution tests.
//!
//! Each test asserts the VM's exact `print` output for a program. `vm`/`parity`
//! return that output (`parity` is a historical helper name); `run` returns a
//! `Result` for error cases.
//!
//! Coverage tracks `backend/vm.rs`: scalars/operators, literal coercion,
//! short-circuit `and`/`or`, `if`/`while`, `for`/`range` (iterator protocol) and
//! `for` over lists, variables, user `def` calls (default/keyword/variadic ABI,
//! `mut`/`ref` reference-param write-back) + recursion, `return`, structs
//! (fieldwise construction, field read, `mut self`), `List`/`Tuple`, SIMD
//! construction + lane read, destructor (`__del__`) calls, `try`/`except`/`else`/
//! `finally` with exceptional-edge cleanup, and `print`/`String`/`len`. Remaining
//! gaps — a `return`/`break`/`continue` crossing a `try` boundary, and methods with
//! `mut`/`ref` ordinary params — are covered by `vm_reports_unsupported_features_cleanly`
//! and `vm_refuses_mut_ref_via_non_place_argument`: the VM must error cleanly, never
//! diverge.

use mojito::{BackendKind, check, elaborate, link, parse};

/// Run `src` through the VM backend (the sole executor) and return its captured
/// output, or a stage error string.
fn run(src: &str) -> Result<String, String> {
    let program = parse(src).map_err(|e| format!("parse error: {e:?}"))?;
    let program = elaborate(program).map_err(|e| format!("comptime error: {e}"))?;
    let checked = mojito::check_program(&program).map_err(|e| format!("type error: {e:?}"))?;
    let mut backend = BackendKind::Vm.make();
    backend
        .run(&checked)
        .map_err(|e| format!("runtime error: {e:?}"))?;
    Ok(backend.output())
}

/// Whether `src` is a statically valid program (parses + type-checks) — used to
/// show a program is well-formed but exercises a VM coverage gap.
fn checks_ok(src: &str) -> bool {
    parse(src).is_ok_and(|p| check(&p).is_ok())
}

/// The VM's output for a program that must succeed (exact-output assertions).
fn vm(src: &str) -> String {
    run(src).expect("vm backend failed")
}

/// Alias retained by the exact-output tests.
fn parity(src: &str) -> String {
    vm(src)
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(
        parity("print(1 + 2 * 3)\nprint((1 + 2) * 3)\nprint(2 ** 10)\nprint(-7 // 2)\n"),
        "7\n9\n1024\n-4\n"
    );
}

#[test]
fn float_arithmetic_same_type() {
    // Both operands are float literals, so no contextual int→float coercion is needed.
    assert_eq!(vm("print(1.0 / 2.0)\nprint(3.5 + 1.5)\n"), "0.5\n5.0\n");
    parity("print(1.0 / 2.0)\nprint(3.5 + 1.5)\n");
}

#[test]
fn boolean_and_comparison() {
    parity("print(1 < 2 and not False)\nprint(3 == 3)\nprint(2 > 5 or 1 == 1)\n");
}

#[test]
fn collection_displays_and_comprehensions_execute_in_source_order() {
    let output = vm(include_str!(
        "../conformance/fixtures/collection_comprehensions.mojo"
    ));
    assert_eq!(
        output,
        "3 True False\n2 9 True\n0\n[0, 4, 16]\n[0, 1, 2, 10, 11, 12]\n{0, 1, 2}\n{0: 0, 1: 1, 2: 4, 3: 9}\n"
    );
}

#[test]
fn comprehension_binders_do_not_overwrite_outer_or_shadowed_bindings() {
    assert_eq!(
        vm("def main():\n    var x = 100\n    var values = [x for x in range(3)]\n    var nested = [x for x in range(2) for x in range(x + 1)]\n    print(x, values)\n    print(nested)\n"),
        "100 [0, 1, 2]\n[0, 0, 1]\n"
    );
}

#[test]
fn collection_displays_materialize_contextual_element_types() {
    assert_eq!(
        vm("def empty() -> Set[Float64]:\n    return {}\n\ndef numbers() -> Set[Float64]:\n    return {1, 2}\n\ndef show(values: Set[Float64]):\n    print(values)\n\ndef main():\n    show({})\n    show({1, 2})\n    print(empty())\n    print(numbers())\n"),
        "{}\n{1.0, 2.0}\n{}\n{1.0, 2.0}\n"
    );
}

#[test]
fn discarded_set_elements_and_replaced_dictionary_values_are_destroyed() {
    let output = vm("struct Token:\n    var id: Int\n    def __init__(out self, id: Int):\n        self.id = id\n    def __del__(deinit self):\n        print(\"drop\", self.id)\n    def __hash__(self) -> UInt:\n        return UInt(self.id)\n\ndef main():\n    var dictionary = {0: Token(1), 0: Token(2)}\n    print(\"built dict\")\n    var values = {Token(3), Token(3)}\n    print(\"built set\")\n");
    assert_eq!(
        output,
        "drop 1\nbuilt dict\ndrop 3\nbuilt set\ndrop 3\ndrop 2\n"
    );
}

#[test]
fn owned_iteration_moves_elements_and_drops_the_residual_on_break() {
    let output = vm(include_str!("../conformance/fixtures/owned_iteration.mojo"));
    assert!(output.contains("take 1\n"));
    assert!(output.contains("take 2\n"));
    assert!(!output.contains("take 3\n"));
    assert!(output.contains("drop 1\n"));
    assert!(output.contains("drop 2\n"));
    assert!(output.contains("drop 3\n"));
    assert!(output.ends_with("done\n"));
}

#[test]
fn string_concat_and_builtins() {
    assert_eq!(
        parity("var s: String = \"ab\" + \"cd\"\nprint(s)\nprint(len(s))\nprint(String(42))\n"),
        "abcd\n4\n42\n"
    );
}

#[test]
fn if_elif_else_via_function() {
    let src = "def sign(n: Int) -> Int:\n    if n > 0:\n        return 1\n    elif n < 0:\n        return -1\n    else:\n        return 0\n\ndef main():\n    print(sign(7))\n    print(sign(-4))\n    print(sign(0))\n";
    assert_eq!(parity(src), "1\n-1\n0\n");
}

#[test]
fn while_loop_accumulates() {
    let src = "def main():\n    var i: Int = 0\n    var total: Int = 0\n    while i < 5:\n        total = total + i\n        i = i + 1\n    print(total)\n";
    assert_eq!(parity(src), "10\n");
}

#[test]
fn function_calls_and_recursion() {
    let src = "def fib(n: Int) -> Int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\n\ndef main():\n    print(fib(10))\n";
    assert_eq!(parity(src), "55\n");
}

#[test]
fn deep_free_function_recursion_uses_vm_frames() {
    assert_eq!(
        run("def countdown(n: Int) -> Int:\n    if n == 0:\n        return 0\n    return countdown(n - 1)\n\ndef main():\n    print(countdown(10000))\n")
            .unwrap(),
        "0\n"
    );
}

#[test]
fn nested_calls_evaluate_in_order() {
    let src = "def add(a: Int, b: Int) -> Int:\n    return a + b\n\ndef sq(n: Int) -> Int:\n    return n * n\n\ndef main():\n    print(sq(add(1, 2)))\n";
    assert_eq!(parity(src), "9\n");
}

#[test]
fn top_level_then_main_entry() {
    // Top-level statements run first, then the synthesized `main()` entry.
    let src = "print(1)\n\ndef main():\n    print(2)\n";
    assert_eq!(parity(src), "1\n2\n");
}

#[test]
fn boolean_short_circuit_skips_rhs() {
    // The MIR lowers `and`/`or` to CFG blocks, so a side-effecting right operand is
    // NOT evaluated when the left settles the result.
    // `loud()` prints; its absence from the output proves the skip.
    let src = "def loud() -> Bool:\n    print(\"called\")\n    return True\n\ndef main():\n    var a: Bool = False and loud()\n    print(a)\n    var b: Bool = True or loud()\n    print(b)\n    if False and loud():\n        print(\"nope\")\n    print(\"done\")\n";
    assert_eq!(parity(src), "False\nTrue\ndone\n");

    // When the left operand does NOT settle it, the right IS evaluated.
    let src2 = "def loud() -> Bool:\n    print(\"called\")\n    return True\n\ndef main():\n    var a: Bool = True and loud()\n    print(a)\n";
    assert_eq!(parity(src2), "called\nTrue\n");

    // Nested short-circuits compose.
    parity("print(True or (False and False))\nprint((1 < 2) and (2 < 3) and (3 < 4))\n");
}

#[test]
fn literal_coercion_at_binding_sites() {
    // An int literal materializes to the annotated/parameter type — the one place
    // the untyped MIR used to diverge. Now the MIR carries the annotation and the
    // VM applies the checked binding coercion.
    assert_eq!(parity("var f: Float64 = 3\nprint(f)\n"), "3.0\n");
    assert_eq!(parity("var u: UInt = 0\nu = u + 1\nprint(u)\n"), "1\n");
    // Int literal into a Float64 parameter, then float arithmetic.
    let src = "def scale(x: Float64) -> Float64:\n    return x * 2.0\n\ndef main():\n    print(scale(5))\n";
    assert_eq!(parity(src), "10.0\n");
    // Inferred `var` keeps the literal's natural kind (int stays Int).
    assert_eq!(parity("var n = 7\nprint(n)\nprint(n + 1)\n"), "7\n8\n");
}

#[test]
fn for_range_iterator_protocol() {
    // `for`/`range` lowers to the iterator protocol (HasNext/Next over a Range),
    // covering step direction, break/continue, and nesting.
    assert_eq!(
        parity("var t: Int = 0\nfor i in range(5):\n    t = t + i\nprint(t)\n"),
        "10\n"
    );
    assert_eq!(
        parity("for j in range(2, 8, 2):\n    print(j)\n"),
        "2\n4\n6\n"
    );
    assert_eq!(
        parity("for k in range(3, 0, -1):\n    print(k)\n"),
        "3\n2\n1\n"
    );
    // An empty range runs the body zero times.
    assert_eq!(
        parity("for x in range(0):\n    print(x)\nprint(99)\n"),
        "99\n"
    );
    // break/continue.
    let bc = "def main():\n    for m in range(10):\n        if m == 3:\n            break\n        if m == 1:\n            continue\n        print(m)\n";
    assert_eq!(parity(bc), "0\n2\n");
    // Nested loops.
    let nested = "for a in range(2):\n    for b in range(2):\n        print(a * 10 + b)\n";
    assert_eq!(parity(nested), "0\n1\n10\n11\n");
}

#[test]
fn return_crossing_try_runs_with_finally() {
    // A `return` inside a `try` crosses the boundary and runs the `finally` on the
    // way out through the VM's Flow-based region execution. A
    // `finally` that itself returns overrides (Python/Mojo semantics).
    let f = "def f() -> Int:\n    try:\n        return 1\n    finally:\n        print(\"fin\")\n\ndef main():\n    print(f())\n";
    assert_eq!(parity(f), "fin\n1\n");
    let override_ = "def g() -> Int:\n    try:\n        return 1\n    finally:\n        return 2\n\ndef main():\n    print(g())\n";
    assert_eq!(parity(override_), "2\n");
    let caught = "def h() -> Int:\n    try:\n        raise \"x\"\n    except e:\n        return 5\n    finally:\n        print(\"h-fin\")\n\ndef main():\n    print(h())\n";
    assert_eq!(parity(caught), "h-fin\n5\n");
}

#[test]
fn break_continue_crossing_try_runs_with_finally() {
    // `break`/`continue` inside a `try` that target an outer loop cross the boundary
    // and run each `finally` on the way out.
    let brk = "def main():\n    for i in range(5):\n        try:\n            if i == 3:\n                break\n            if i % 2 == 0:\n                continue\n            print(\"odd\", i)\n        finally:\n            print(\"fin\", i)\n    print(\"done\")\n";
    assert_eq!(parity(brk), "fin 0\nodd 1\nfin 1\nfin 2\nfin 3\ndone\n");
    // `break` in an `except`; a `finally` that itself `break`s overrides a body
    // `continue`; nested try/finally both run before the jump reaches the loop.
    let exc = "def main():\n    for i in range(4):\n        try:\n            raise \"x\"\n        except e:\n            break\n        finally:\n            print(\"fin\", i)\n    print(\"done\")\n";
    assert_eq!(parity(exc), "fin 0\ndone\n");
    let fin = "def main():\n    for i in range(3):\n        try:\n            continue\n        finally:\n            print(\"f\", i)\n            break\n    print(\"done\")\n";
    assert_eq!(parity(fin), "f 0\ndone\n");
    let nested = "def main():\n    for i in range(3):\n        try:\n            try:\n                break\n            finally:\n                print(\"in\", i)\n        finally:\n            print(\"out\", i)\n    print(\"done\")\n";
    assert_eq!(parity(nested), "in 0\nout 0\ndone\n");
}

#[test]
fn vm_reports_unsupported_features_cleanly() {
    // A remaining coverage gap must surface as a clean error, not a wrong answer or
    // a panic. `break`/`continue` crossing a `try` now works when the target loop is
    // function-level; the still-refused case is a loop declared *inside* a `try`,
    // broken by a nested `try` (a region-local target the mini-CFG can't name) — a
    // statically valid program the VM must reject cleanly, not diverge.
    let program = "def main():\n    try:\n        for i in range(3):\n            try:\n                break\n            finally:\n                print(\"fin\", i)\n    finally:\n        print(\"outer\")\n";
    assert!(checks_ok(program), "the program is statically valid");
    assert!(
        run(program).is_err(),
        "a break targeting a loop declared inside an enclosing try is not supported — must error"
    );
}

#[test]
fn vm_runs_every_ok_fixture() {
    // The VM is the sole executor: every `assets/ok/*.mojo` fixture must run without
    // error. (Exact-output correctness is asserted by the targeted tests above.)
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ok");
    let mut ran = 0;
    for entry in std::fs::read_dir(dir).expect("assets/ok exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mojo") {
            continue;
        }
        // This fixture intentionally reads stdin; an integration test inherits
        // Cargo's terminal and would wait for user input forever.
        if path.file_name().and_then(|n| n.to_str()) == Some("input.mojo") {
            continue;
        }
        let program = link(&path)
            .unwrap_or_else(|e| panic!("link failed on ok fixture {}: {e}", path.display()));
        let program = elaborate(program)
            .unwrap_or_else(|e| panic!("comptime failed on ok fixture {}: {e}", path.display()));
        let checked = mojito::check_program(&program)
            .unwrap_or_else(|e| panic!("check failed on ok fixture {}: {e:?}", path.display()));
        let mut backend = BackendKind::Vm.make();
        backend
            .run(&checked)
            .unwrap_or_else(|e| panic!("vm failed on ok fixture {}: {e:?}", path.display()));
        ran += 1;
    }
    assert!(ran > 0, "expected some ok fixtures");
}

#[test]
fn structs_construction_fields_and_mut_self() {
    // Construction, field read, a read-only method, and a `mut self` method whose
    // mutation persists (written back through the receiver place).
    let src = "@fieldwise_init\nstruct Counter:\n    var n: Int\n\n    def get(self) -> Int:\n        return self.n\n\n    def bump(mut self, k: Int):\n        self.n += k\n\ndef main():\n    var c: Counter = Counter(10)\n    print(c.get())\n    c.bump(5)\n    c.bump(2)\n    print(c.n)\n";
    assert_eq!(parity(src), "10\n17\n");
}

#[test]
fn lists_tuples_and_indexing() {
    // List literal + index + mutation + membership; tuple return + const index.
    assert_eq!(
        parity("var xs = [1, 2, 3]\nxs.append(4)\nprint(xs[0])\nprint(len(xs))\nprint(3 in xs)\n"),
        "1\n4\nTrue\n"
    );
    let tup = "def pair() -> Tuple[Int, Int]:\n    return (7, 9)\n\ndef main():\n    var t = pair()\n    print(t[0])\n    print(t[1])\n";
    assert_eq!(parity(tup), "7\n9\n");
}

#[test]
fn argument_matching_default_keyword_variadic() {
    assert_eq!(
        parity(
            "def p(b: Int, e: Int = 2) -> Int:\n    return b ** e\n\ndef main():\n    print(p(3))\n    print(p(3, 3))\n    print(p(e=4, b=2))\n"
        ),
        "9\n27\n16\n"
    );
    let variadic = "def total(*xs: Int) -> Int:\n    var s: Int = 0\n    for x in xs:\n        s = s + x\n    return s\n\ndef main():\n    print(total())\n    print(total(1, 2, 3))\n";
    assert_eq!(parity(variadic), "0\n6\n");
}

#[test]
fn user_static_methods_use_the_shared_call_abi() {
    assert_eq!(
        parity(
            "struct S:\n    @staticmethod\n    def add(a: Int, b: Int = 2) -> Int:\n        return a + b\n\ndef main():\n    print(S.add(3), S.add(b=4, a=3))\n"
        ),
        "5 7\n"
    );
}

#[test]
fn argument_markers_positional_only_keyword_only_and_variadic_tail() {
    let src = "def first(a: Int, b: Int, /) -> Int:\n    return a\n\ndef scale(a: Int, *, by: Int) -> Int:\n    return a * by\n\ndef total(*xs: Int, scale: Int) -> Int:\n    var s: Int = 0\n    for x in xs:\n        s = s + x\n    return s * scale\n\ndef main():\n    print(first(8, 9))\n    print(scale(6, by=7))\n    print(total(1, 2, 3, scale=10))\n";
    assert_eq!(parity(src), "8\n42\n60\n");
}

#[test]
fn simd_construction_elementwise_and_lane() {
    let src = "var v: SIMD[DType.float64, 4] = SIMD[DType.float64, 4](1.0, 2.0, 3.0, 4.0)\nvar scaled = v * 2.0\nprint(scaled[3])\n";
    assert_eq!(parity(src), "8.0\n");
}

#[test]
fn vm_mut_ref_params_write_back() {
    // A `mut`/`ref` reference parameter mutates the caller's variable — the VM
    // writes each one's final value back to the caller's argument place after the
    // call.
    assert_eq!(
        parity(
            "def incr(mut x: Int):\n    x = x + 1\n\ndef main():\n    var n: Int = 5\n    incr(n)\n    incr(n)\n    print(n)\n"
        ),
        "7\n"
    );
    // `ref` writes back too; write-back through a struct field place persists.
    assert_eq!(
        parity(
            "def set_to(ref x: Int, v: Int):\n    x = v\n\ndef main():\n    var n: Int = 0\n    set_to(n, 42)\n    print(n)\n"
        ),
        "42\n"
    );
    let field = "@fieldwise_init\nstruct Counter:\n    var n: Int\n\ndef bump(mut c: Counter, k: Int):\n    c.n = c.n + k\n\ndef main():\n    var c: Counter = Counter(0)\n    bump(c, 5)\n    bump(c, 3)\n    print(c.n)\n";
    assert_eq!(parity(field), "8\n");
}

#[test]
fn method_mut_ref_param_writeback_parity() {
    // A method with a `mut` *ordinary* parameter writes the mutated argument back
    // to the caller's place through the VM call ABI.
    let src = "@fieldwise_init\nstruct C:\n    var n: Int\n    def combine(self, mut other: C):\n        other.n = other.n + self.n\n\ndef main():\n    var a: C = C(1)\n    var b: C = C(2)\n    a.combine(b)\n    print(b.n)\n";
    assert_eq!(parity(src), "3\n");
}

#[test]
fn method_argument_binding_matches_free_functions() {
    let src = "@fieldwise_init\nstruct Acc:\n    var total: Int\n    def add(mut self, x: Int, /, y: Int = 2, *rest: Int, scale: Int = 1) -> Int:\n        var amount: Int = x + y\n        for value in rest:\n            amount = amount + value\n        self.total = self.total + amount * scale\n        return self.total\n    def bump_arg(self, mut value: Int, delta: Int = 1):\n        value = value + delta\n\ndef main():\n    var acc: Acc = Acc(0)\n    print(acc.add(3, y=4, scale=2))\n    print(acc.add(1, 5, 6, 7, scale=3))\n    var n: Int = 10\n    acc.bump_arg(n, delta=4)\n    print(n)\n";
    assert_eq!(parity(src), "14\n71\n14\n");
}

#[test]
fn generic_argument_binding_matches_free_functions() {
    let src = "def collect[T: AnyType](head: T, /, extra: Int = 2, *rest: Int, scale: Int = 1) -> Int:\n    return (extra + len(rest)) * scale\n\ndef replace[T: Copyable & Movable](mut value: T, replacement: T):\n    value = replacement\n\ndef main():\n    print(collect(\"x\", extra=3, scale=4))\n    print(collect(1, 2, 8, 9, scale=3))\n    var n: Int = 5\n    replace(n, replacement=9)\n    print(n)\n";
    assert_eq!(parity(src), "12\n12\n9\n");
}

#[test]
fn try_except_else_finally() {
    // Full structured exceptions cover a
    // caught raise, the `else` on normal completion, and `finally` on every path.
    let caught = "def main():\n    try:\n        print(\"body\")\n        raise \"x\"\n        print(\"unreached\")\n    except e:\n        print(\"caught\")\n    finally:\n        print(\"fin\")\n    print(\"after\")\n";
    assert_eq!(parity(caught), "body\ncaught\nfin\nafter\n");

    let no_raise = "def main():\n    try:\n        print(\"body\")\n    except e:\n        print(\"caught\")\n    else:\n        print(\"elseran\")\n    finally:\n        print(\"fin\")\n";
    assert_eq!(parity(no_raise), "body\nelseran\nfin\n");
}

#[test]
fn partial_move_field_read_parity() {
    // A partial move `p.a^` followed by reads of the moved value and the retained
    // sibling runs identically on both backends: the field read now lowers to a
    // `LoadPlace`, and `^` on a field to a `MovePlace`, preserving the moved value.
    let src = "@fieldwise_init\nstruct Inner:\n    var id: Int\n\n@fieldwise_init\nstruct Pair:\n    var a: Inner\n    var b: Inner\n\ndef main():\n    var p: Pair = Pair(Inner(1), Inner(2))\n    var x: Inner = p.a^\n    print(x.id)\n    print(p.b.id)\n    p.a = Inner(9)\n    print(p.a.id)\n";
    assert_eq!(parity(src), "1\n2\n9\n");
}

#[test]
fn utility_builtins_parity() {
    // abs/min/max/round + Int/UInt/Float64 conversions use shared runtime helpers.
    let src = "def main():\n    print(abs(-5))\n    print(abs(-3.5))\n    print(min(3, 7), max(3, 7))\n    print(round(2.5), round(2.4))\n    print(Int(3.9), UInt(42), Float64(7))\n";
    assert_eq!(parity(src), "5\n3.5\n3 7\n3.0 2.0\n3 42 7.0\n");
}

#[test]
fn simd_lane_write_parity() {
    // `v[i] = e` / `v[i] += e`, both bare and through a struct field, now update a
    // SIMD lane through `store_place`/`set_simd_lane`.
    let src = "@fieldwise_init\nstruct Vec4:\n    var data: SIMD[DType.int32, 4]\n\ndef main():\n    var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\n    v[0] = 10\n    v[2] += 5\n    print(v[0], v[1], v[2], v[3])\n    var w: Vec4 = Vec4(SIMD[DType.int32, 4](0, 0, 0, 0))\n    w.data[1] = 42\n    print(w.data[1])\n";
    assert_eq!(parity(src), "10 2 8 4\n42\n");
}

#[test]
fn value_parameterized_generics_parity() {
    // A value-parameterized struct reifies its value parameter (read via
    // `Self.size`), and a value-parameterized function binds it as a local — both
    // execute through the same VM frame representation.
    let src = "@fieldwise_init\nstruct FixedBuffer[size: Int]:\n    var tag: Int\n    def capacity(self) -> Int:\n        return Self.size\n\ndef scaled[factor: Int](x: Int) -> Int:\n    return x * factor\n\ndef main():\n    var b: FixedBuffer[8] = FixedBuffer[8](3)\n    print(b.capacity(), b.tag)\n    print(scaled[10](4))\n";
    assert_eq!(parity(src), "8 3\n40\n");
}

#[test]
fn nested_def_closures_parity() {
    // Nested `def`s cover a
    // read-capture, a write-capture (reference semantics), and self-recursion.
    let read = "def adder(n: Int) -> Int:\n    def add_n(x: Int) unified {n} -> Int:\n        return x + n\n    return add_n(100)\n\ndef main():\n    print(adder(42))\n";
    assert_eq!(parity(read), "142\n");
    let write = "def counter() -> Int:\n    var total: Int = 0\n    def add(x: Int) unified {mut total}:\n        total = total + x\n    add(5)\n    add(3)\n    return total\n\ndef main():\n    print(counter())\n";
    assert_eq!(parity(write), "8\n");
    let rec = "def factorial(base: Int) -> Int:\n    def fact(n: Int) unified {base} -> Int:\n        if n <= 1:\n            return base\n        return n * fact(n - 1)\n    return fact(5)\n\ndef main():\n    print(factorial(1))\n";
    assert_eq!(parity(rec), "120\n");
}

#[test]
fn nested_def_calling_sibling_forwards_its_closure_environment() {
    let src = "def outer() -> Int:\n    var b: Int = 10\n    def helper(x: Int) unified {b} -> Int:\n        return x + b\n    def caller(y: Int) unified {helper} -> Int:\n        return helper(y) + 1\n    return caller(5)\n\ndef main():\n    print(outer())\n";
    assert_eq!(parity(src), "16\n");
}

#[test]
fn generic_nested_def_is_type_erased_after_checker_inference() {
    let src = "def outer() -> Int:\n    def identity[T: Copyable & Movable](value: T) unified {} -> T:\n        return value\n    return identity(42)\n\ndef main():\n    print(outer())\n";
    assert_eq!(parity(src), "42\n");
}

#[test]
fn nominal_callable_struct_requires_and_executes_call_contract() {
    let src = "@fieldwise_init\nstruct Scale(def(Int) -> Int):\n    var factor: Int\n    def __call__(self, value: Int) -> Int:\n        return value * self.factor\n\ndef apply(callback: def(Int) -> Int, value: Int) -> Int:\n    return callback(value)\n\ndef main():\n    var scale = Scale(3)\n    print(apply(scale, 14))\n";
    assert_eq!(parity(src), "42\n");
}

#[test]
fn overloaded_callable_values_execute_contextual_targets() {
    let src = include_str!("../conformance/fixtures/overloaded_callable_values.mojo");
    assert_eq!(parity(src), "42\ncaught\n");
}

#[test]
fn overload_symbols_distinguish_stropped_type_names() {
    let src = "@fieldwise_init\nstruct `A-B`:\n    var x: Int\n\n@fieldwise_init\nstruct `A_B`:\n    var x: Int\n\ndef choose(x: `A-B`) -> Int:\n    return 1\n\ndef choose(x: `A_B`) -> Int:\n    return 2\n\ndef main():\n    print(choose(`A-B`(0)))\n    print(choose(`A_B`(0)))\n";
    assert_eq!(vm(src), "1\n2\n");
}

#[test]
fn overload_symbols_fold_comptime_value_arguments() {
    let src = "@fieldwise_init\nstruct FixedBuffer[size: Int]:\n    var value: Int\n\ncomptime N = 2 + 6\n\ndef choose(x: FixedBuffer[N]) -> Int:\n    return 1\n\ndef choose(x: Int) -> Int:\n    return 2\n\ndef main():\n    print(choose(FixedBuffer[8](7)))\n";
    assert!(vm(src).lines().any(|line| line == "1"));
}

#[test]
fn dunder_operator_and_builtin_dispatch() {
    // Operators + `len`/`String`/subscript/`in` on a user struct dispatch to its
    // dunder methods (operator overloading), running on the VM.
    let src = "@fieldwise_init\nstruct Vec2(Writable):\n    var x: Int\n    var y: Int\n    def __add__(self, o: Vec2) -> Vec2:\n        return Vec2(self.x + o.x, self.y + o.y)\n    def __eq__(self, o: Vec2) -> Bool:\n        return self.x == o.x and self.y == o.y\n    def write_to(self, mut writer: Some[Writer]):\n        writer.write(\"V(\", self.x, \",\", self.y, \")\")\n    def __len__(self) -> Int:\n        return 2\n    def __getitem__(self, i: Int) -> Int:\n        if i == 0:\n            return self.x\n        return self.y\n    def __contains__(self, v: Int) -> Bool:\n        return self.x == v or self.y == v\n\ndef main():\n    var a: Vec2 = Vec2(1, 2)\n    print(String(a + Vec2(3, 4)))\n    print(a == Vec2(1, 2))\n    print(len(a), a[0], a[1])\n    print(2 in a, 9 not in a)\n";
    assert_eq!(parity(src), "V(4,6)\nTrue\n2 1 2\nTrue True\n");
}

#[test]
fn dunder_augmented_assignment_uses_add() {
    // `c += d` expands to `c = c + d`, which dispatches to `__add__`.
    let src = "@fieldwise_init\nstruct Acc(Writable):\n    var n: Int\n    def __add__(self, o: Acc) -> Acc:\n        return Acc(self.n + o.n)\n    def write_to(self, mut writer: Some[Writer]):\n        writer.write(self.n)\n\ndef main():\n    var c: Acc = Acc(1)\n    c += Acc(10)\n    c += Acc(100)\n    print(String(c))\n";
    assert_eq!(parity(src), "111\n");
}

#[test]
fn dunder_setitem_writes_back() {
    // `c[i] = e` dispatches to `__setitem__(mut self, …)` and the mutation persists;
    // `c[i] += e` reads via `__getitem__` and writes via `__setitem__`; a nested
    // place (`h.p[i] = e`) writes back through the outer struct.
    let src = "@fieldwise_init\nstruct Pair:\n    var a: Int\n    var b: Int\n    def __getitem__(self, i: Int) -> Int:\n        if i == 0:\n            return self.a\n        return self.b\n    def __setitem__(mut self, i: Int, v: Int):\n        if i == 0:\n            self.a = v\n        else:\n            self.b = v\n\n@fieldwise_init\nstruct Holder:\n    var p: Pair\n\ndef main():\n    var p: Pair = Pair(1, 2)\n    p[0] = 10\n    p[1] = 20\n    p[0] += 5\n    print(p[0], p[1])\n    var h: Holder = Holder(Pair(5, 6))\n    h.p[1] = 99\n    print(h.p[0], h.p[1])\n";
    assert_eq!(parity(src), "15 20\n5 99\n");
}

#[test]
fn hand_written_init_constructs_and_coerces() {
    // A `def __init__(out self, …)` builds the struct: fields are set in the body,
    // and arguments are coerced to the parameter types (Int literal → Float64).
    let src = "struct Point:\n    var x: Int\n    var y: Int\n    def __init__(out self, x: Int, y: Int):\n        self.x = x\n        self.y = y\n    def sum(self) -> Int:\n        return self.x + self.y\n\nstruct Scaled:\n    var v: Float64\n    def __init__(out self, n: Float64):\n        self.v = n * 2.0\n\ndef main():\n    var p: Point = Point(3, 4)\n    print(p.x, p.y, p.sum())\n    var s: Scaled = Scaled(5)\n    print(s.v)\n";
    assert_eq!(parity(src), "3 4 7\n10.0\n");
}

#[test]
fn user_iterator_protocol() {
    // `for x in c` on a user type dispatches `c.__iter__()` → loop while
    // `len(iter) > 0` binding `x = iter.__next__()`; break/continue compose.
    let src = "@fieldwise_init\nstruct It:\n    var cur: Int\n    var stop: Int\n    def __len__(self) -> Int:\n        return self.stop - self.cur\n    def __next__(mut self) -> Int:\n        var v: Int = self.cur\n        self.cur = self.cur + 1\n        return v\n\n@fieldwise_init\nstruct Nums:\n    var n: Int\n    def __iter__(self) -> It:\n        return It(0, self.n)\n\ndef main():\n    for x in Nums(6):\n        if x == 4:\n            break\n        if x == 1:\n            continue\n        print(x)\n";
    assert_eq!(parity(src), "0\n2\n3\n");
}

#[test]
fn unsafe_pointer_alloc_load_store_alias() {
    // `UnsafePointer[T].alloc`/`ptr[i]` load+store, `ptr[i] += e`, and aliasing (a
    // copied pointer shares storage), running over the VM heap arena.
    let src = "def main():\n    var p: UnsafePointer[Int] = UnsafePointer[Int].alloc(3)\n    p[0] = 10\n    p[1] = 20\n    p[1] += 5\n    var q: UnsafePointer[Int] = p\n    q[0] = 99\n    print(p[0], p[1])\n";
    assert_eq!(parity(src), "99 25\n");
}

#[test]
fn self_hosted_vec_over_unsafe_pointer() {
    // A heap-owning container written in mojito: `push` mutates storage through
    // the pointer (aliased across the value-type copy); the size is written back.
    let src = "struct IntVec:\n    var data: UnsafePointer[Int]\n    var size: Int\n    def __init__(out self, cap: Int):\n        self.data = UnsafePointer[Int].alloc(cap)\n        self.size = 0\n    def push(mut self, v: Int):\n        self.data[self.size] = v\n        self.size = self.size + 1\n    def get(self, i: Int) -> Int:\n        return self.data[i]\n\ndef main():\n    var xs: IntVec = IntVec(8)\n    xs.push(7)\n    xs.push(8)\n    xs.push(9)\n    print(xs.size, xs.get(0), xs.get(2))\n";
    assert_eq!(parity(src), "3 7 9\n");
}

#[test]
fn copyinit_gives_value_semantics() {
    // A pointer-owning struct with `__copyinit__` deep-copies on `var b = a` and on
    // pass-by-value, so writes through one don't affect the other. `__moveinit__`
    // relocates on `^`.
    let src = "struct Buf:\n    var data: UnsafePointer[Int]\n    var n: Int\n    def __init__(out self, n: Int):\n        self.data = UnsafePointer[Int].alloc(n)\n        self.n = n\n    def __copyinit__(out self, e: Buf):\n        self.n = e.n\n        self.data = UnsafePointer[Int].alloc(e.n)\n        var i: Int = 0\n        while i < e.n:\n            self.data[i] = e.data[i]\n            i = i + 1\n    def __moveinit__(out self, deinit e: Buf):\n        self.n = e.n\n        self.data = e.data\n    def set(mut self, i: Int, v: Int):\n        self.data[i] = v\n    def get(self, i: Int) -> Int:\n        return self.data[i]\n\ndef main():\n    var a: Buf = Buf(2)\n    a.set(0, 5)\n    var b: Buf = a\n    b.set(0, 9)\n    print(a.get(0), b.get(0))\n    var c: Buf = b^\n    print(c.get(0))\n";
    assert_eq!(parity(src), "5 9\n9\n");
}

#[test]
fn mojo_copy_constructor_gives_value_semantics() {
    let src = "struct Buf(Copyable):\n    var data: UnsafePointer[Int]\n    var n: Int\n    def __init__(out self, n: Int):\n        self.data = UnsafePointer[Int].alloc(n)\n        self.n = n\n    def __init__(out self, *, copy: Self):\n        self.n = copy.n\n        self.data = UnsafePointer[Int].alloc(copy.n)\n        var i: Int = 0\n        while i < copy.n:\n            self.data[i] = copy.data[i]\n            i = i + 1\n    def set(mut self, i: Int, v: Int):\n        self.data[i] = v\n    def get(self, i: Int) -> Int:\n        return self.data[i]\n\ndef main():\n    var a: Buf = Buf(2)\n    a.set(0, 5)\n    var b: Buf = Buf(copy: a)\n    b.set(0, 9)\n    print(a.get(0), b.get(0))\n    var c: Buf = a\n    c.set(0, 11)\n    print(a.get(0), c.get(0))\n";
    assert_eq!(parity(src), "5 9\n5 11\n");
}

#[test]
fn ternary_and_chained_comparison_run() {
    // Ternary picks a branch; chained comparison evaluates each operand once and
    // short-circuits (a middle False → the rest is not evaluated).
    let src = "def loud(n: Int) -> Int:\n    print(\"e\", n)\n    return n\n\ndef main():\n    var x: Int = 5\n    var m: Int = 10 if x > 0 else 20\n    print(m)\n    print(0 <= x < 10)\n    print(0 <= x < 3)\n    print(1 < 0 < loud(99))\n";
    // loud(99) must NOT run (1 < 0 is False), so no "e 99" line.
    assert_eq!(parity(src), "10\nTrue\nFalse\nFalse\n");
}

#[test]
fn tuple_unpacking_runs() {
    // Unpack into names; swap through a temporary tuple (RHS built once).
    let src = "def main():\n    var t: Tuple[Int, Int, Int] = (1, 2, 3)\n    a, b, c = t\n    print(a, b, c)\n    var x: Int = 10\n    var y: Int = 20\n    x, y = (y, x)\n    print(x, y)\n";
    assert_eq!(parity(src), "1 2 3\n20 10\n");
}

#[test]
fn current_tuple_core_runs() {
    let src = "def main():\n    var bare = 4, \"four\"\n    var left, right = bare\n    print(bare)\n    print(left, right)\n    print(Tuple())\n    print(Tuple(7))\n    print(Tuple[Float64, String](2, \"two\"))\n    print(Tuple[Float64](2)[0])\n    print(len(bare), 4 in bare, 9 not in bare)\n    print(Tuple(1, 2) == Tuple(1, 2))\n    print(Tuple(1, 2) != Tuple(1, 3))\n    print(Tuple(1, 2) < Tuple(1, 3), Tuple(2, 0) >= Tuple(1, 9))\n    print(bare.reverse())\n    print(bare.concat(Tuple(True)))\n";
    assert_eq!(
        parity(src),
        "(4, four)\n4 four\n()\n(7,)\n(2.0, two)\n2.0\n2 True True\nTrue\nTrue\nTrue True\n(four, 4)\n(4, four, True)\n"
    );
}

#[test]
fn slice_subscript_runs() {
    // List + String slicing with optional bounds, steps, negative indices, reversal.
    let src = "def main():\n    var xs: List[Int] = [0, 1, 2, 3, 4, 5]\n    print(xs[1:4])\n    print(xs[::2])\n    print(xs[::-1])\n    print(xs[-2:])\n    var s: String = \"hello\"\n    print(s[1:4])\n    print(s[::-1])\n";
    assert_eq!(
        parity(src),
        "[1, 2, 3]\n[0, 2, 4]\n[5, 4, 3, 2, 1, 0]\n[4, 5]\nell\nolleh\n"
    );
}

#[test]
fn reference_valued_aggregate_preserves_and_writes_through_handle() {
    let src = "@fieldwise_init\nstruct RefBox[origin: Origin[mut=True]]:\n    var value: ref[origin] Int\n\ndef main():\n    var value = 40\n    ref alias = value\n    var box = RefBox(alias)\n    box.value += 2\n    print(value)\n";
    assert_eq!(parity(src), "42\n");
}

#[test]
fn handwritten_initializer_stores_reference_field_handle() {
    let src = "struct RefBox[origin: Origin[mut=True]]:\n    var value: ref[origin] Int\n    def __init__(out self, ref[origin] value: Int):\n        self.value = value\n\ndef main():\n    var value = 40\n    var box = RefBox(value)\n    box.value += 2\n    print(value)\n";
    assert_eq!(parity(src), "42\n");
}

#[test]
fn nested_reference_aggregate_preserves_handles() {
    // Mojito-only executable-ref-field proof; current Mojo uses origin-bearing
    // pointer aggregates for stored provenance.
    let tuple = "@fieldwise_init\nstruct RefTuple[origin: Origin[mut=True]]:\n    var values: Tuple[ref[origin] Int, ref[origin] Int]\n\ndef main():\n    var left = 4\n    var right = 8\n    ref a = left\n    ref b = right\n    var pair = RefTuple((a, b))\n    print(pair.values[0], pair.values[1])\n";
    assert_eq!(vm(tuple), "4 8\n");

    let list = "@fieldwise_init\nstruct RefList[origin: Origin[mut=True]]:\n    var values: List[ref[origin] Int]\n\ndef main():\n    var left = 4\n    var right = 8\n    ref a = left\n    ref b = right\n    var pair = RefList([a, b])\n    pair.values[1] += 2\n    print(left, right)\n";
    assert_eq!(vm(list), "4 10\n");
}

#[test]
fn variant_projection_is_a_tag_checked_place() {
    let src = "struct Variant:\n    pass\n\ndef main():\n    var value = Variant[Int, String](7)\n    value[Int] += 5\n    print(value[Int])\n";
    assert_eq!(vm(src), "12\n");

    // Mojito's executable local-ref extension exercises the same place as a
    // persistent frame/slot handle, rather than a cloned VariantGet payload.
    let src = "struct Variant:\n    pass\n\ndef main():\n    var value = Variant[Int, String](7)\n    ref payload = value[Int]\n    payload += 5\n    print(value[Int])\n";
    assert_eq!(vm(src), "12\n");
}

#[test]
fn user_slice_dispatches_through_checked_getitem() {
    let src = "@fieldwise_init\nstruct Window:\n    var size: Int\n\n    def __getitem__(self, part: Slice) -> Int:\n        var normalized = part.indices(self.size)\n        return normalized[0] + normalized[1] + normalized[2]\n\n@fieldwise_init\nstruct Grid:\n    def __getitem__(self, row: Int, columns: Slice) -> Int:\n        var normalized = columns.indices(10)\n        return row * 100 + normalized[0] + normalized[1] + normalized[2]\n\ndef main():\n    var window = Window(10)\n    print(window[:5])\n    print(window[::-1])\n    var grid = Grid()\n    print(grid[3, 1:8:2])\n";
    assert_eq!(parity(src), "6\n7\n311\n");
}

#[test]
fn slice_descriptor_overloads_are_selected_statically() {
    let src = "@fieldwise_init\nstruct Probe:\n    def __getitem__(self, part: ContiguousSlice) -> Int:\n        return 1\n    def __getitem__(self, part: StridedSlice) -> Int:\n        return 2\n\ndef main():\n    var probe = Probe()\n    print(probe[1:5], probe[1:5:2], probe[::])\n";
    assert_eq!(parity(src), "1 2 2\n");
}

#[test]
fn multi_index_dispatch_supports_variadic_getitem() {
    let src = "@fieldwise_init\nstruct Cube:\n    def __getitem__(self, *indices: Int) -> Int:\n        return indices[0] * 100 + indices[1] * 10 + indices[2]\n\ndef main():\n    var cube = Cube()\n    print(cube[1, 2, 3])\n";
    assert_eq!(parity(src), "123\n");
}

#[test]
fn mixed_slice_assignment_dispatches_to_fixed_setitem() {
    let src = "@fieldwise_init\nstruct Grid:\n    var value: Int\n    def __setitem__(mut self, row: Int, columns: Slice, value: Int):\n        var normalized = columns.indices(10)\n        self.value = row * 100 + normalized[0] + normalized[1] + normalized[2] + value\n\ndef main():\n    var grid = Grid(0)\n    grid[3, 1:8:2] = 9\n    print(grid.value)\n";
    assert_eq!(parity(src), "320\n");
}

#[test]
fn multidimensional_assignment_supports_variadic_setitem() {
    let src = "@fieldwise_init\nstruct Cube:\n    var value: Int\n    def __setitem__(mut self, *indices: Int, *, value: Int):\n        self.value = indices[0] * 1000 + indices[1] * 100 + indices[2] * 10 + value\n\ndef main():\n    var cube = Cube(0)\n    cube[1, 2, 3] = 4\n    print(cube.value)\n";
    assert_eq!(parity(src), "1234\n");
}

#[test]
fn explicit_slice_values_expose_optional_fields_and_indices() {
    let src = "def main():\n    var span = Slice(None, None, -1)\n    print(span.start.is_some(), span.end.or_else(9), span.step.or_else(1))\n    print(span.indices(4))\n    print(slice(3).indices(10))\n";
    assert_eq!(parity(src), "False 9 -1\n(3, -1, -1)\n(0, 3, 1)\n");
}
