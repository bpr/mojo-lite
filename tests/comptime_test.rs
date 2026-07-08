//! Compile-time elaboration (`comptime if` / `comptime for`). Each test runs the
//! real pipeline stage order — parse → **elaborate** → check → VM — so it exercises
//! the phase-distinction semantics: unselected branches are dropped before checking,
//! and `comptime for` unrolls with the loop variable substituted as a literal.

use mojo_lite::{BackendKind, CtValue, Ty, check, elaborate, parse};

fn run(src: &str) -> Result<String, String> {
    let program = parse(src).map_err(|e| format!("parse: {e}"))?;
    let program = elaborate(program).map_err(|e| format!("comptime: {e}"))?;
    check(&program).map_err(|e| format!("check: {e:?}"))?;
    let mut backend = BackendKind::Vm.make();
    backend.run(&program).map_err(|e| format!("run: {e:?}"))?;
    Ok(backend.output())
}

#[test]
fn ct_value_can_carry_a_type_without_runtime_materialization() {
    let ty = Ty::List(Box::new(Ty::Int));
    let value = CtValue::Type(Box::new(ty));

    assert_eq!(value.to_string(), "List[Int]");
    assert!(value.materialize((0, 0)).is_none());
}

#[test]
fn comptime_if_selects_a_branch() {
    let src = "comptime N = 8\n\ndef main():\n    comptime if N > 4:\n        print(\"big\")\n    elif N > 0:\n        print(\"small\")\n    else:\n        print(\"zero\")\n";
    assert_eq!(run(src).unwrap(), "big\n");
}

#[test]
fn comptime_if_drops_unselected_branch_before_checking() {
    // The `else` branch has a type error, but it is dropped by elaboration, so the
    // program still type-checks and runs — the key metaprogramming property.
    let src = "comptime FLAG = 1\n\ndef main():\n    comptime if FLAG == 1:\n        print(\"ok\")\n    else:\n        var bad: Int = \"not an int\"\n        print(bad)\n";
    assert_eq!(run(src).unwrap(), "ok\n");
}

#[test]
fn comptime_for_unrolls_with_substitution() {
    // `i` becomes a literal in each unrolled copy (0², 1², 2², 3²).
    let src = "def main():\n    comptime for i in range(4):\n        print(i, i * i)\n";
    assert_eq!(run(src).unwrap(), "0 0\n1 1\n2 4\n3 9\n");
}

#[test]
fn comptime_for_over_a_const_with_nested_comptime_if() {
    let src = "comptime COUNT = 5\n\ndef main():\n    comptime for i in range(COUNT):\n        comptime if i % 2 == 0:\n            print(i, \"even\")\n        else:\n            print(i, \"odd\")\n";
    assert_eq!(run(src).unwrap(), "0 even\n1 odd\n2 even\n3 odd\n4 even\n");
}

#[test]
fn comptime_for_range_variants_and_reverse() {
    let src = "def main():\n    comptime for i in range(2, 8, 2):\n        print(i)\n    comptime for j in range(3, 0, -1):\n        print(j)\n";
    assert_eq!(run(src).unwrap(), "2\n4\n6\n3\n2\n1\n");
}

#[test]
fn comptime_for_quota_rejects_a_huge_unroll() {
    let err =
        run("def main():\n    comptime for i in range(1000000):\n        print(i)\n").unwrap_err();
    assert!(err.contains("quota"), "got {err}");
}

#[test]
fn comptime_for_iterates_a_heterogeneous_tuple() {
    // The payoff: `t[i]` needs a compile-time-constant index (tuple elements are
    // heterogeneous), which a runtime `for` can't provide — but `comptime for`
    // substitutes `i` with a literal, so each `t[i]` type-checks.
    let src = "def main():\n    var t: Tuple[Int, String, Bool] = (42, \"hi\", True)\n    comptime for i in range(3):\n        print(t[i])\n";
    assert_eq!(run(src).unwrap(), "42\nhi\ntrue\n");
}

#[test]
fn comptime_for_over_a_tuple_of_strings() {
    // The codex-direction milestone: iterate a compile-time tuple of strings.
    let src = "comptime states = (\"empty\", \"occupied\", \"deleted\")\n\ndef main():\n    comptime for state in states:\n        print(state)\n";
    assert_eq!(run(src).unwrap(), "empty\noccupied\ndeleted\n");
}

#[test]
fn comptime_for_over_a_list_and_string_concat() {
    // A compile-time list of ints, and compile-time string concatenation (used to
    // pick a branch, so the concatenated value is consumed at compile time).
    let src = "comptime sizes = [1, 2, 4, 8]\n\ndef main():\n    comptime for n in sizes:\n        print(n)\n    comptime if \"a\" + \"b\" == \"ab\":\n        print(\"concat-ok\")\n";
    assert_eq!(run(src).unwrap(), "1\n2\n4\n8\nconcat-ok\n");
}

#[test]
fn comptime_for_enables_compile_time_tuple_indexing() {
    // Substituting the loop var with a literal makes `t[i]` a compile-time-constant
    // index — so a heterogeneous tuple can be walked (a runtime `for` can't).
    let src = "def main():\n    var t: Tuple[Int, String, Bool] = (1, \"two\", True)\n    comptime for i in range(3):\n        print(t[i])\n";
    assert_eq!(run(src).unwrap(), "1\ntwo\ntrue\n");
}

#[test]
fn non_comptime_binding_is_rejected_by_elaboration() {
    // `comptime NAME = <runtime value>` is rejected at compile-time elaboration.
    let program = parse("var x: Int = 3\ncomptime N = x\n").unwrap();
    assert!(elaborate(program).is_err());
}

#[test]
fn ctfe_runs_a_pure_function_at_compile_time() {
    // A pure top-level function (loops + locals) executes at compile time.
    let src = "def next_pow2(n: Int) -> Int:\n    var p: Int = 1\n    while p < n:\n        p = p * 2\n    return p\n\ncomptime CAP = next_pow2(17)\n\ndef main():\n    comptime for i in range(CAP):\n        pass\n    print(CAP)\n";
    assert_eq!(run(src).unwrap(), "32\n");
}

#[test]
fn ctfe_supports_recursion() {
    let src = "def fact(n: Int) -> Int:\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\n\ncomptime F = fact(5)\n\ndef main():\n    print(F)\n";
    assert_eq!(run(src).unwrap(), "120\n");
}

#[test]
fn ctfe_is_fuel_bounded() {
    let err = run("def spin(n: Int) -> Int:\n    while True:\n        n = n + 1\n    return n\ncomptime X = spin(0)\n\ndef main():\n    print(X)\n").unwrap_err();
    assert!(err.contains("quota"), "got {err}");
}

#[test]
fn module_comptime_constants_materialize_into_functions() {
    // A top-level comptime constant is usable inside a function (materialized as a
    // literal, closing the module-global-in-function gap): as a value returned from
    // a function, and as a value-parameter argument (`Box[N]`).
    let src = "comptime GREETING = \"hi\"\ncomptime N = 8\n\ndef greet() -> String:\n    return GREETING\n\n@fieldwise_init\nstruct Box[size: Int]:\n    var v: Int\n    def cap(self) -> Int:\n        return Self.size\n\ndef main():\n    print(greet())\n    var b: Box[N] = Box[N](0)\n    print(b.cap())\n";
    assert_eq!(run(src).unwrap(), "hi\n8\n");
}

#[test]
fn ctfe_computed_value_parameter_argument() {
    // Phase 1 regression (comptime.md): a CTFE-computed comptime constant flows
    // into a value-parameter argument through the shared compile-time value model —
    // `pow2(3)` runs at compile time to `8`, materializes into `scale[N]`, and the
    // checker resolves `scale`'s value parameter `n` from it.
    let src = "def scale[n: Int](x: Int) -> Int:\n    return x * n\n\ndef pow2(k: Int) -> Int:\n    var x: Int = 1\n    for i in range(k):\n        x = x * 2\n    return x\n\ncomptime N = pow2(3)\n\ndef main():\n    print(scale[N](5))\n";
    assert_eq!(run(src).unwrap(), "40\n");
}
