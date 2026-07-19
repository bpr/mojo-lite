use mojito::{BackendKind, RuntimeError, TypeError, Value, check_program, parse};

/// Run a program on the VM backend (the sole executor), returning its global
/// (top-level) bindings for value inspection. No type-checking — these tests
/// exercise evaluation semantics directly (static errors are `checker_test`'s job).
fn run(source: &str) -> Vec<(String, Value)> {
    let program = parse(source).expect("parse error");
    let checked = check_program(&program).expect("type error");
    let mut backend = BackendKind::make("vm").expect("the register VM is implemented");
    backend.run(&checked).expect("runtime error");
    backend.bindings()
}

/// Run a program that is expected to fail at runtime, returning the error.
fn run_err(source: &str) -> RuntimeError {
    let program = parse(source).expect("parse error");
    let checked = check_program(&program).expect("type error");
    let mut backend = BackendKind::make("vm").expect("the register VM is implemented");
    backend.run(&checked).expect_err("expected a runtime error")
}

/// Run a program and return its captured `print` output.
fn output(source: &str) -> String {
    let program = parse(source).expect("parse error");
    let checked = check_program(&program).expect("type error");
    let mut backend = BackendKind::make("vm").expect("the register VM is implemented");
    backend.run(&checked).expect("runtime error");
    backend.output()
}

#[test]
fn bounded_trait_dispatch_preserves_raising_effects_at_runtime() {
    assert_eq!(
        output(
            "trait Fallible:\n    def run(self) raises -> Int: ...\n\n@fieldwise_init\nstruct Failure(Fallible):\n    var code: Int\n    def run(self) raises -> Int:\n        raise \"failed\"\n        return self.code\n\ndef invoke[T: Fallible](value: T) raises -> Int:\n    return value.run()\n\ndef main():\n    try:\n        var ignored = invoke(Failure(7))\n    except error:\n        print(\"caught trait effect\")\n"
        ),
        "caught trait effect\n"
    );
}

#[test]
fn executes_function_scoped_implicit_binding_from_a_nested_block() {
    assert_eq!(
        output("def main():\n    if True:\n        value = 7\n    print(value)\n"),
        "7\n"
    );
}

fn binding(bindings: &[(String, Value)], name: &str) -> Value {
    bindings
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no binding named '{}'", name))
        .1
        .clone()
}

#[test]
fn evaluates_operator_precedence() {
    let e = run("var a: Int = 1 + 2 * 3\nvar b: Int = (1 + 2) * 3\n");
    assert_eq!(binding(&e, "a"), Value::Int(7));
    assert_eq!(binding(&e, "b"), Value::Int(9));
}

#[test]
fn evaluates_unary_and_boolean_logic() {
    let e = run("var x: Int = -5\nvar y: Bool = not False\nvar z: Bool = 1 < 2 and 2 <= 2\n");
    assert_eq!(binding(&e, "x"), Value::Int(-5));
    assert_eq!(binding(&e, "y"), Value::Bool(true));
    assert_eq!(binding(&e, "z"), Value::Bool(true));
}

#[test]
fn string_concatenation_and_equality() {
    let e = run("var s: String = \"foo\" + \"bar\"\nvar eq: Bool = s == \"foobar\"\n");
    assert_eq!(binding(&e, "s"), Value::Str("foobar".into()));
    assert_eq!(binding(&e, "eq"), Value::Bool(true));
}

#[test]
fn and_or_short_circuit() {
    // Division by zero would fail at runtime if either right-hand side ran.
    let e = run("var a: Bool = False and (1 // 0 == 0)\nvar b: Bool = True or (1 // 0 == 0)\n");
    assert_eq!(binding(&e, "a"), Value::Bool(false));
    assert_eq!(binding(&e, "b"), Value::Bool(true));
}

#[test]
fn functions_and_nested_calls() {
    let e = run(
        "def add(x: Int, y: Int) -> Int:\n    return x + y\n\ndef square(n: Int) -> Int:\n    return n * n\n\nvar r: Int = square(add(1, 2))\n",
    );
    assert_eq!(binding(&e, "r"), Value::Int(9));
}

#[test]
fn named_out_result_is_returned_without_a_caller_argument() {
    assert_eq!(
        output(
            "def doubled(value: Int, out result: Int):\n    result = value * 2\n\ndef main():\n    print(doubled(21))\n"
        ),
        "42\n"
    );
}

#[test]
fn callable_parameter_invokes_a_function_value() {
    let actual = output(
        "def increment(x: Int) -> Int:\n    return x + 1\n\ndef apply(cb: def(Int) -> Int, x: Int) -> Int:\n    return (cb)(x)\n\ndef main():\n    var callback: def(Int) -> Int = increment\n    print(apply(callback, 41))\n",
    );
    assert_eq!(actual, "42\n");
}

#[test]
fn generic_function_value_runs_through_a_monomorphic_callable_view() {
    assert_eq!(
        output(
            "def identity[T: Copyable & Movable](value: T) -> T:\n    return value\n\ndef main():\n    var callback: def(Int) -> Int = identity\n    print(callback(42))\n"
        ),
        "42\n"
    );
}

#[test]
fn overloaded_function_value_uses_its_contextual_signature() {
    assert_eq!(
        output(
            "def choose(value: Int) -> Int:\n    return value + 1\n\ndef choose(value: String) -> Int:\n    return len(value)\n\ndef main():\n    var callback: def(Int) -> Int = choose\n    print(callback(41))\n"
        ),
        "42\n"
    );
}

#[test]
fn raising_callable_value_is_caught() {
    let actual = output(
        "def boom() raises -> Int:\n    raise \"boom\"\n    return 0\n\ndef invoke(callback: def() raises -> Int) raises -> Int:\n    return callback()\n\ndef main():\n    try:\n        var ignored = invoke(boom)\n    except error:\n        print(\"caught callable\")\n",
    );
    assert_eq!(actual, "caught callable\n");
}

#[test]
fn inner_scope_shadows_outer() {
    let e = run(
        "var x: Int = 1\ndef f() -> Int:\n    var x: Int = 99\n    return x\n\nvar outer: Int = x\nvar inner: Int = f()\n",
    );
    assert_eq!(binding(&e, "outer"), Value::Int(1));
    assert_eq!(binding(&e, "inner"), Value::Int(99));
}

#[test]
fn local_reference_reads_and_writes_owner_storage() {
    assert_eq!(
        output(
            "def main():\n    var value: Int = 1\n    ref alias = value\n    alias = 4\n    print(value)\n"
        ),
        "4\n"
    );
}

#[test]
fn local_reference_index_is_evaluated_once() {
    assert_eq!(
        output(
            "@fieldwise_init\nstruct Cursor:\n    var count: Int\n    def next(mut self) -> Int:\n        var old = self.count\n        self.count += 1\n        return old\n\ndef main():\n    var values = List(10, 20)\n    var cursor = Cursor(0)\n    ref alias = values[cursor.next()]\n    print(alias)\n    print(alias)\n    print(cursor.count)\n"
        ),
        "10\n10\n1\n"
    );
}

#[test]
fn ref_self_mutation_persists() {
    assert_eq!(
        output(
            "@fieldwise_init\nstruct Counter:\n    var value: Int\n    def bump(ref self):\n        self.value += 1\n\ndef main():\n    var counter = Counter(1)\n    counter.bump()\n    print(counter.value)\n"
        ),
        "2\n"
    );
}

#[test]
fn parametric_mutability_ref_parameter_executes() {
    assert_eq!(
        output(
            "def set[origin: Origin[mut=True]](ref[origin] value: Int):\n    value = 9\n\ndef main():\n    var value = 1\n    set(value)\n    print(value)\n"
        ),
        "9\n"
    );
}

#[test]
fn returned_reference_aliases_caller_storage() {
    assert_eq!(
        output(
            "def borrow[origin: Origin[mut=True]](ref[origin] value: Int) -> ref[origin] Int:\n    return value\n\ndef main():\n    var value = 1\n    ref alias = borrow(value)\n    alias = 7\n    print(alias)\n    print(value)\n"
        ),
        "7\n7\n"
    );
}

#[test]
fn returned_reference_preserves_receiver_projection() {
    assert_eq!(
        output(
            "@fieldwise_init\nstruct Box:\n    var value: Int\n    def get(ref self) -> ref[origin_of(self.value)] Int:\n        return self.value\n\ndef main():\n    var box = Box(2)\n    ref alias = box.get()\n    alias = 8\n    print(box.value)\n"
        ),
        "8\n"
    );
}

#[test]
fn union_reference_return_keeps_dynamic_identity() {
    assert_eq!(
        output(
            "def choose(ref left: Int, ref right: Int, first: Bool) -> ref[left, right] Int:\n    if first:\n        return left\n    return right\n\ndef main():\n    var left = 1\n    var right = 2\n    ref alias = choose(left, right, False)\n    alias = 9\n    print(left, right)\n"
        ),
        "1 9\n"
    );
}

#[test]
fn returned_index_reference_captures_the_selected_element() {
    assert_eq!(
        output(
            "def element(ref values: List[Int], index: Int) -> ref[origin_of(values[index])] Int:\n    return values[index]\n\ndef main():\n    var values = List(3, 4)\n    ref alias = element(values, 1)\n    alias = 12\n    print(values[1])\n"
        ),
        "12\n"
    );
}

#[test]
fn closure_captures_enclosing_local_downward() {
    let e = run(
        "def adder(n: Int) -> Int:\n    def add_n(x: Int) unified {n} -> Int:\n        return x + n\n    return add_n(100)\n\nvar c: Int = adder(42)\n",
    );
    assert_eq!(binding(&e, "c"), Value::Int(142));
}

#[test]
fn unified_closure_value_carries_read_and_mutable_environments() {
    assert_eq!(
        output(
            "def apply_twice(callback: def(Int) -> None):\n    callback(2)\n    callback(3)\n\ndef total() -> Int:\n    var sum: Int = 0\n    def add(value: Int) unified {mut sum}:\n        sum += value\n    apply_twice(add)\n    return sum\n\ndef main():\n    print(total())\n"
        ),
        "5\n"
    );
}

// Note: closure-escape (`return helper` / `f = g`), undefined-variable, and
// argument type-mismatch are now caught statically by the checker (see
// `checker_test`), not at VM runtime, so those cases moved there.

#[test]
fn arity_mismatch_is_an_error() {
    let program =
        parse("def f(x: Int) -> Int:\n    return x\n\nvar a: Int = f(1, 2)\n").expect("parse");
    assert!(matches!(
        check_program(&program),
        Err(TypeError::ArityMismatch { .. })
    ));
}

#[test]
fn type_mismatch_in_arithmetic_is_an_error() {
    let program = parse("var a: Int = 1 + True\n").expect("parse");
    assert!(matches!(
        check_program(&program),
        Err(TypeError::BadOperator { .. })
    ));
}

// --- Structs ---

const POINT: &str = "@fieldwise_init\nstruct Point:\n    var x: Int\n    var y: Int\n\n    def sum(self) -> Int:\n        return self.x + self.y\n\n    def scaled(self, k: Int) -> Int:\n        return self.sum() * k\n\n";

#[test]
fn constructs_and_reads_fields() {
    let e = run(&format!(
        "{POINT}var p: Point = Point(3, 4)\nvar a: Int = p.x\nvar b: Int = p.y\n"
    ));
    assert_eq!(binding(&e, "a"), Value::Int(3));
    assert_eq!(binding(&e, "b"), Value::Int(4));
}

#[test]
fn handwritten_constructor_shares_default_and_keyword_call_binding() {
    assert_eq!(
        output(
            "struct Box:\n    var value: Int\n    def __init__(out self, value: Int = 3):\n        self.value = value\n\ndef main():\n    var a = Box()\n    var b = Box(value=7)\n    print(a.value, b.value)\n"
        ),
        "3 7\n"
    );
}

#[test]
fn trait_default_method_is_materialized_and_can_be_overridden() {
    let actual = output(
        "trait Named:\n    def name(self) -> String: ...\n    def describe(self) -> String:\n        return self.name() + \"!\"\n\n@fieldwise_init\nstruct Defaulted(Named):\n    var label: String\n    def name(self) -> String:\n        return self.label\n\n@fieldwise_init\nstruct Overridden(Named):\n    var label: String\n    def name(self) -> String:\n        return self.label\n    def describe(self) -> String:\n        return \"override \" + self.label\n\ndef main():\n    var a = Defaulted(\"default\")\n    var b = Overridden(\"custom\")\n    print(a.describe())\n    print(b.describe())\n",
    );
    assert_eq!(actual, "default!\noverride custom\n");
}

#[test]
fn opaque_trait_bound_dispatches_indexing() {
    let actual = output(
        "trait IntIndexer:\n    def __getitem__(self, index: Int) -> Int: ...\n\n@fieldwise_init\nstruct Pair(IntIndexer):\n    var first: Int\n    var second: Int\n    def __getitem__(self, index: Int) -> Int:\n        if index == 0:\n            return self.first\n        return self.second\n\ndef second[T: IntIndexer](value: T) -> Int:\n    return value[1]\n\ndef main():\n    print(second(Pair(3, 7)))\n",
    );
    assert_eq!(actual, "7\n");
}

#[test]
fn indexer_values_normalize_for_builtin_collections() {
    let actual = output(
        "@fieldwise_init\nstruct Offset(Indexer):\n    var value: Int\n    def __mlir_index__(self) -> Int:\n        return self.value\n\ndef main():\n    var values = [3, 7, 11]\n    print(values[Offset(1)])\n",
    );
    assert_eq!(actual, "7\n");
}

#[test]
fn writable_value_formats_through_its_string_hook() {
    let actual = output(
        "@fieldwise_init\nstruct Temperature(Writable):\n    var degrees: Int\n    def write_to(self, mut writer: Some[Writer]):\n        writer.write(self.degrees, \" degrees\")\n\ndef main():\n    print(Temperature(21))\n",
    );
    assert_eq!(actual, "21 degrees\n");
}

#[test]
fn writable_default_reflects_fields() {
    let actual = output(
        "@fieldwise_init\nstruct Point(Writable):\n    var x: Int\n    var y: Int\n\ndef main():\n    print(Point(2, 5))\n",
    );
    assert_eq!(actual, "Point(x=2, y=5)\n");
}

#[test]
fn writable_repr_and_string_format_use_writer_rendering() {
    let actual = output(
        "@fieldwise_init\nstruct Point(Writable):\n    var x: Int\n    def write_to(self, mut writer: Some[Writer]):\n        writer.write(\"point=\", self.x)\n    def write_repr_to(self, mut writer: Some[Writer]):\n        writer.write(\"Point[\", self.x, \"]\")\n\ndef main():\n    var point = Point(4)\n    print(String(point))\n    print(repr(point))\n    print(\"{} / {!r} / {0}\".format(point, point))\n    print(\"|{:>5}|{:<5}|{:.2f}\".format(3, 4, 1.5))\n",
    );
    assert_eq!(
        actual,
        "point=4\nPoint[4]\npoint=4 / Point[4] / point=4\n|    3|4    |1.50\n"
    );
}

#[test]
fn hashable_contributes_to_a_caller_provided_hasher() {
    let actual = output(
        "@fieldwise_init\nstruct Pair(Hashable):\n    var left: Int\n    var right: Int\n    def __hash__(self, mut hasher: Some[Hasher]):\n        hasher.update(self.left)\n        hasher.update(self.right)\n\ndef main():\n    print(hash(Pair(1, 2)) == hash(Pair(1, 2)))\n    print(hash(Pair(1, 2)) == hash(Pair(2, 1)))\n",
    );
    assert_eq!(actual, "True\nFalse\n");
}

#[test]
fn buffer_writer_receives_multiple_writable_values() {
    let actual = output(
        "@fieldwise_init\nstruct BufferWriter(Writer):\n    var buffer: String\n    def write_string(mut self, value: String):\n        self.buffer = self.buffer + value\n\ndef main():\n    var writer = BufferWriter(\"\")\n    writer.write(\"count=\", 3, \" ok=\", True)\n    print(writer.buffer)\n",
    );
    assert_eq!(actual, "count=3 ok=True\n");
}

#[test]
fn method_reads_self_and_calls_sibling_method() {
    let e = run(&format!(
        "{POINT}var p: Point = Point(3, 4)\nvar s: Int = p.sum()\nvar sc: Int = p.scaled(10)\n"
    ));
    assert_eq!(binding(&e, "s"), Value::Int(7));
    assert_eq!(binding(&e, "sc"), Value::Int(70)); // (3+4) * 10
}

#[test]
fn nested_struct_fields_and_chained_access() {
    let e = run(
        "@fieldwise_init\nstruct Inner:\n    var v: Int\n\n@fieldwise_init\nstruct Outer:\n    var inner: Inner\n    var tag: Int\n\nvar o: Outer = Outer(Inner(5), 9)\nvar v: Int = o.inner.v\nvar t: Int = o.tag\n",
    );
    assert_eq!(binding(&e, "v"), Value::Int(5));
    assert_eq!(binding(&e, "t"), Value::Int(9));
}

#[test]
fn struct_passed_to_a_function_is_copied() {
    // The function receives the struct by value and reads a field.
    let e = run(&format!(
        "{POINT}def first(q: Point) -> Int:\n    return q.x\n\nvar p: Point = Point(8, 9)\nvar r: Int = first(p)\n"
    ));
    assert_eq!(binding(&e, "r"), Value::Int(8));
}

// --- Numbers: Float64, UInt, conversions ---

#[test]
fn floor_division_and_modulo_floor_toward_negative_infinity() {
    let e = run(
        "var a: Int = -7 // 2\nvar b: Int = -7 % 2\nvar c: Int = 7 // -2\nvar d: Int = 7 % -2\n",
    );
    assert_eq!(binding(&e, "a"), Value::Int(-4)); // floor(-3.5)
    assert_eq!(binding(&e, "b"), Value::Int(1)); // remainder takes the divisor's sign
    assert_eq!(binding(&e, "c"), Value::Int(-4));
    assert_eq!(binding(&e, "d"), Value::Int(-1));
}

#[test]
fn power_and_true_division() {
    let e = run("var p: Int = 2 ** 10\nvar h: Float64 = 7 / 2\nvar r: Float64 = 2.0 ** 0.5\n");
    assert_eq!(binding(&e, "p"), Value::Int(1024));
    assert_eq!(binding(&e, "h"), Value::Float64(3.5)); // true division of ints -> Float64
    assert_eq!(binding(&e, "r"), Value::Float64(2.0_f64.powf(0.5)));
}

#[test]
fn integer_division_by_zero_is_a_runtime_error() {
    assert!(matches!(
        run_err("var x: Int = 1 // 0\n"),
        RuntimeError::TypeError(_)
    ));
    assert!(matches!(
        run_err("var x: Int = 1 % 0\n"),
        RuntimeError::TypeError(_)
    ));
}

#[test]
fn literals_coerce_at_runtime() {
    // 0 materializes to UInt; `u + 1` keeps u a UInt; 3 becomes Float64; 1/2 is 0.5.
    let e = run(
        "var u: UInt = 0\nu = u + 1\nu = u + 1\nvar f: Float64 = 3\nvar half: Float64 = 1 / 2\n",
    );
    assert_eq!(binding(&e, "u"), Value::UInt(2));
    assert_eq!(binding(&e, "f"), Value::Float64(3.0));
    assert_eq!(binding(&e, "half"), Value::Float64(0.5));
}

#[test]
fn uint_accumulates_with_literals_in_a_loop() {
    let e = run("var total: UInt = UInt(0)\nfor i in range(5):\n    total = total + 1\n");
    assert_eq!(binding(&e, "total"), Value::UInt(5));
}

#[test]
fn float_arithmetic_and_division() {
    let e =
        run("var a: Float64 = 1.5 + 2.0 * 3.0\nvar b: Float64 = 10.0 / 4.0\nvar c: Float64 = -b\n");
    assert_eq!(binding(&e, "a"), Value::Float64(7.5));
    assert_eq!(binding(&e, "b"), Value::Float64(2.5));
    assert_eq!(binding(&e, "c"), Value::Float64(-2.5));
}

#[test]
fn uint_arithmetic_via_conversions() {
    let e = run("var u: UInt = UInt(10)\nu = u - UInt(3)\nvar t: Bool = u == UInt(7)\n");
    assert_eq!(binding(&e, "u"), Value::UInt(7));
    assert_eq!(binding(&e, "t"), Value::Bool(true));
}

#[test]
fn numeric_conversions_follow_mojo() {
    let e = run(
        "var trunc: Int = Int(3.9)\nvar widen: Float64 = Float64(3)\nvar frombool: Int = Int(True)\nvar u: UInt = UInt(5)\n",
    );
    assert_eq!(binding(&e, "trunc"), Value::Int(3)); // truncates toward zero
    assert_eq!(binding(&e, "widen"), Value::Float64(3.0));
    assert_eq!(binding(&e, "frombool"), Value::Int(1));
    assert_eq!(binding(&e, "u"), Value::UInt(5));
}

#[test]
fn float_accumulation_in_a_loop() {
    let e = run("var total: Float64 = 0.0\nfor i in range(4):\n    total = total + 1.5\n");
    assert_eq!(binding(&e, "total"), Value::Float64(6.0));
}

// --- Assignment ---

#[test]
fn reassignment_updates_the_variable() {
    let e = run("var x: Int = 1\nx = 42\nx = x + 1\n");
    assert_eq!(binding(&e, "x"), Value::Int(43));
}

#[test]
fn for_loop_accumulates_into_an_outer_variable() {
    // total lives in the function scope; the per-iteration body scope assigns
    // through to it, so the sum survives across iterations.
    let e = run(
        "def sum_to(n: Int) -> Int:\n    var total: Int = 0\n    for i in range(n):\n        total = total + i\n    return total\n\nvar s: Int = sum_to(5)\n",
    );
    assert_eq!(binding(&e, "s"), Value::Int(10)); // 0+1+2+3+4
}

#[test]
fn while_loop_terminates_via_mutation() {
    let e = run(
        "def count() -> Int:\n    var x: Int = 0\n    while x < 5:\n        x = x + 1\n    return x\n\nvar c: Int = count()\n",
    );
    assert_eq!(binding(&e, "c"), Value::Int(5));
}

#[test]
fn assignment_in_a_branch_updates_the_enclosing_variable() {
    let e = run(
        "def f(n: Int) -> Int:\n    var r: Int = 0\n    if n > 0:\n        r = 1\n    return r\n\nvar a: Int = f(5)\nvar b: Int = f(-5)\n",
    );
    assert_eq!(binding(&e, "a"), Value::Int(1));
    assert_eq!(binding(&e, "b"), Value::Int(0));
}

#[test]
fn var_less_introduction_binds_the_variable() {
    // `x = 1` on an undeclared name (implicit declaration) works on the VM: it
    // lowers to the same binding as `var x = 1`.
    let e = run("x = 1\nx = x + 4\n");
    assert_eq!(binding(&e, "x"), Value::Int(5));
}

// --- Control flow ---

#[test]
fn if_elif_else_selects_the_right_branch() {
    let e = run(
        "def sign(n: Int) -> Int:\n    if n > 0:\n        return 1\n    elif n < 0:\n        return -1\n    else:\n        return 0\n\nvar pos: Int = sign(5)\nvar neg: Int = sign(-3)\nvar zero: Int = sign(0)\n",
    );
    assert_eq!(binding(&e, "pos"), Value::Int(1));
    assert_eq!(binding(&e, "neg"), Value::Int(-1));
    assert_eq!(binding(&e, "zero"), Value::Int(0));
}

#[test]
fn for_over_range_is_half_open() {
    // member(t, n) == 1 iff t appears in range(n) == 0,1,...,n-1
    let prog = "def member(t: Int, n: Int) -> Int:\n    for i in range(n):\n        if i == t:\n            return 1\n    return 0\n\n";
    let e = run(&format!(
        "{prog}var a: Int = member(0, 5)\nvar b: Int = member(4, 5)\nvar c: Int = member(5, 5)\nvar d: Int = member(-1, 5)\n"
    ));
    assert_eq!(binding(&e, "a"), Value::Int(1)); // 0 is in range(5)
    assert_eq!(binding(&e, "b"), Value::Int(1)); // 4 is in range(5)
    assert_eq!(binding(&e, "c"), Value::Int(0)); // 5 is NOT (upper bound exclusive)
    assert_eq!(binding(&e, "d"), Value::Int(0)); // -1 is not
}

#[test]
fn range_two_and_three_args_and_negative_step() {
    let prog = "def member(t: Int, a: Int, b: Int, s: Int) -> Int:\n    for i in range(a, b, s):\n        if i == t:\n            return 1\n    return 0\n\n";
    let e = run(&format!(
        "{prog}var asc: Int = member(4, 0, 10, 2)\nvar gap: Int = member(5, 0, 10, 2)\nvar desc: Int = member(3, 5, 0, -1)\nvar past: Int = member(0, 5, 0, -1)\n"
    ));
    assert_eq!(binding(&e, "asc"), Value::Int(1)); // 4 in 0,2,4,6,8
    assert_eq!(binding(&e, "gap"), Value::Int(0)); // 5 not in 0,2,4,6,8
    assert_eq!(binding(&e, "desc"), Value::Int(1)); // 3 in 5,4,3,2,1
    assert_eq!(binding(&e, "past"), Value::Int(0)); // 0 excluded (stop is exclusive)
}

#[test]
fn continue_skips_to_next_iteration() {
    // Without `continue`, the first iteration would `return 0`.
    let e = run(
        "def first_from(start: Int, n: Int) -> Int:\n    for i in range(n):\n        if i < start:\n            continue\n        return i\n    return -1\n\nvar r: Int = first_from(3, 10)\n",
    );
    assert_eq!(binding(&e, "r"), Value::Int(3));
}

#[test]
fn break_exits_the_loop_early() {
    // `break` at i == 2 prevents the loop from ever reaching the i == 4 return.
    let e = run(
        "def f(n: Int) -> Int:\n    for i in range(n):\n        if i == 2:\n            break\n        if i == 4:\n            return 99\n    return 7\n\nvar r: Int = f(10)\n",
    );
    assert_eq!(binding(&e, "r"), Value::Int(7));
}

#[test]
fn break_only_exits_the_innermost_loop() {
    let e = run(
        "def f(n: Int) -> Int:\n    for i in range(n):\n        for j in range(n):\n            if j == 1:\n                break\n            if j == 2:\n                return 99\n    return 7\n\nvar r: Int = f(5)\n",
    );
    assert_eq!(binding(&e, "r"), Value::Int(7));
}

#[test]
fn while_loop_runs_until_break() {
    // Without `break` this would loop forever; reaching 42 proves it terminated.
    let e =
        run("def f() -> Int:\n    while True:\n        break\n    return 42\n\nvar r: Int = f()\n");
    assert_eq!(binding(&e, "r"), Value::Int(42));
}

#[test]
fn empty_range_runs_the_body_zero_times() {
    let e = run(
        "def f() -> Int:\n    for i in range(0):\n        return 1\n    return 0\n\nvar r: Int = f()\n",
    );
    assert_eq!(binding(&e, "r"), Value::Int(0));
}

#[test]
fn loop_runs_the_body_each_iteration() {
    // A `for` accumulates across iterations (loop-var scoping is enforced by the
    // checker; the VM's flat frame is an internal detail, not observed here).
    let e = run("var total: Int = 0\nfor i in range(4):\n    total = total + i\n");
    assert_eq!(binding(&e, "total"), Value::Int(6));
}

#[test]
fn range_with_zero_step_is_a_runtime_error() {
    let err = run_err("for i in range(0, 5, 0):\n    pass\n");
    assert!(matches!(err, RuntimeError::TypeError(_)), "got {:?}", err);
}

// --- Parameterization (generics): type-erased at runtime ---

const PAIR: &str = "@fieldwise_init\nstruct Pair[T: Copyable & Movable]:\n    var left: Self.T\n    var right: Self.T\n";

#[test]
fn generic_struct_constructs_and_reads_members() {
    let e = run(&format!(
        "{PAIR}var p: Pair[Int] = Pair(3, 4)\nvar a: Int = p.left\nvar b: Int = p.right\n"
    ));
    assert_eq!(binding(&e, "a"), Value::Int(3));
    assert_eq!(binding(&e, "b"), Value::Int(4));
}

#[test]
fn generic_struct_preserves_element_runtime_type() {
    // A `Pair[Float64]` keeps Float64 fields (the float literals materialize).
    let e = run(&format!(
        "{PAIR}var p: Pair[Float64] = Pair(1.5, 2.5)\nvar a: Float64 = p.left\n"
    ));
    assert_eq!(binding(&e, "a"), Value::Float64(1.5));
}

#[test]
fn inferred_generic_methods_run_type_erased() {
    let src = "@fieldwise_init\nstruct Factory:\n    var marker: Int\n    @staticmethod\n    def make[T: Copyable](value: T) -> T:\n        return value\n    def echo[T: Copyable](self, value: T) -> T:\n        return value\n\ndef main():\n    var factory = Factory(0)\n    print(Factory.make(7), factory.echo(\"ok\"))\n";
    assert_eq!(output(src), "7 ok\n");
}

#[test]
fn generic_target_implicit_conversion_runs_selected_constructor() {
    let src = "struct Box[T: AnyType]:\n    var value: Self.T\n    @implicit\n    def __init__(out self, value: Self.T):\n        self.value = value\n\ndef take(value: Box[Int]) -> Int:\n    return value.value\n\ndef main():\n    print(take(42))\n";
    assert_eq!(output(src), "42\n");
}

#[test]
fn generic_function_identity_runs_type_erased() {
    let e = run(
        "def id[T: Copyable & Movable](x: T) -> T:\n    return x\n\nvar n: Int = id(5)\nvar s: String = id(\"hi\")\n",
    );
    assert_eq!(binding(&e, "n"), Value::Int(5));
    assert_eq!(binding(&e, "s"), Value::Str("hi".into()));
}

#[test]
fn generic_function_over_generic_struct() {
    let e = run(&format!(
        "{PAIR}def first[T: Copyable & Movable](p: Pair[T]) -> T:\n    return p.left\n\nvar p: Pair[Int] = Pair(10, 20)\nvar x: Int = first(p)\n"
    ));
    assert_eq!(binding(&e, "x"), Value::Int(10));
}

#[test]
fn generic_struct_method_dispatches() {
    let e = run(
        "@fieldwise_init\nstruct Box[T: Copyable & Movable]:\n    var val: Self.T\n\n    def get(self) -> Self.T:\n        return self.val\n\nvar b: Box[Int] = Box(7)\nvar g: Int = b.get()\n",
    );
    assert_eq!(binding(&e, "g"), Value::Int(7));
}

// --- Traits (Phase 1b): type-erased; dispatch is on the conforming struct ---

const QUACK: &str = "trait Quackable:\n    def quack(self) -> String:\n        ...\n\n@fieldwise_init\nstruct Duck(Quackable):\n    var name: String\n\n    def quack(self) -> String:\n        return \"Quack\"\n\ndef make_it_quack[T: Quackable](x: T) -> String:\n    return x.quack()\n";

#[test]
fn bounded_generic_dispatches_to_conforming_struct_method() {
    let e = run(&format!(
        "{QUACK}var s: String = make_it_quack(Duck(\"Donald\"))\n"
    ));
    assert_eq!(binding(&e, "s"), Value::Str("Quack".into()));
}

#[test]
fn trait_declaration_produces_no_binding() {
    // A trait is a pure compile-time construct — nothing at runtime.
    let e = run("trait Q:\n    def m(self) -> Int:\n        ...\n");
    assert!(
        e.iter().all(|(n, _)| n != "Q"),
        "trait leaked a runtime binding"
    );
}

#[test]
fn self_typed_trait_method_runs() {
    let e = run(
        "trait Eq2:\n    def same(self, other: Self) -> Bool:\n        ...\n\n@fieldwise_init\nstruct P(Eq2):\n    var x: Int\n\n    def same(self, other: Self) -> Bool:\n        return self.x == other.x\n\nvar r: Bool = P(1).same(P(1))\nvar q: Bool = P(1).same(P(2))\n",
    );
    assert_eq!(binding(&e, "r"), Value::Bool(true));
    assert_eq!(binding(&e, "q"), Value::Bool(false));
}

// --- Value parameters + comptime (Phase 2) ---

const FIXEDBUF: &str = "@fieldwise_init\nstruct FixedBuffer[size: Int]:\n    var tag: Int\n\n    def capacity(self) -> Int:\n        return Self.size\n";

#[test]
fn value_parameter_is_reified_and_read_via_self() {
    let e = run(&format!(
        "{FIXEDBUF}var b: FixedBuffer[8] = FixedBuffer[8](3)\nvar c: Int = b.capacity()\n"
    ));
    assert_eq!(binding(&e, "c"), Value::Int(8));
}

#[test]
fn float_compile_time_parameter_materializes_at_runtime() {
    let src = "def weight[value: Float64]() -> Float64:\n    return value\n\ndef main():\n    print(weight[1.5]())\n";
    assert_eq!(output(src), "1.5\n");
}

#[test]
fn comptime_arithmetic_argument_evaluates() {
    let e = run(&format!(
        "{FIXEDBUF}var b: FixedBuffer[2 + 3] = FixedBuffer[2 + 3](0)\nvar c: Int = b.capacity()\n"
    ));
    assert_eq!(binding(&e, "c"), Value::Int(5));
}

#[test]
fn value_parameter_function_binds_the_value() {
    let e = run("def doubled[n: Int]() -> Int:\n    return n * 2\n\nvar d: Int = doubled[21]()\n");
    assert_eq!(binding(&e, "d"), Value::Int(42));
}

#[test]
fn comptime_constant_is_a_runtime_int() {
    let e = run("comptime N = 6 * 7\nvar n: Int = N\n");
    assert_eq!(binding(&e, "n"), Value::Int(42));
}

// --- SIMD (bit-accurate) ---

#[test]
fn simd_construction_add_and_index() {
    let e = run(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar w: SIMD[DType.int32, 4] = v + v\nvar lane: Int32 = w[2]\n",
    );
    assert_eq!(binding(&e, "w").to_string(), "[2, 4, 6, 8]");
    assert_eq!(binding(&e, "lane").to_string(), "6");
}

#[test]
fn simd_splat_and_multiply() {
    let e = run(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](5)\nvar w: SIMD[DType.int32, 4] = v * v\nvar lane: Int32 = w[0]\n",
    );
    assert_eq!(binding(&e, "lane").to_string(), "25");
}

#[test]
fn int8_arithmetic_wraps_bit_accurately() {
    // 100 + 100 = 200, which wraps to -56 in signed int8.
    let e = run(
        "var v: SIMD[DType.int8, 2] = SIMD[DType.int8, 2](100)\nvar w: SIMD[DType.int8, 2] = v + v\nvar lane: Int8 = w[0]\n",
    );
    assert_eq!(binding(&e, "lane").to_string(), "-56");
}

#[test]
fn uint8_arithmetic_wraps_bit_accurately() {
    // 255 + 1 = 0 in uint8.
    let e = run(
        "var v: SIMD[DType.uint8, 2] = SIMD[DType.uint8, 2](255)\nvar w: SIMD[DType.uint8, 2] = v + SIMD[DType.uint8, 2](1)\nvar lane: UInt8 = w[0]\n",
    );
    assert_eq!(binding(&e, "lane").to_string(), "0");
}

#[test]
fn byte_alias_materializes_as_uint8() {
    let execution = run("var byte: Byte = 255\n");
    assert_eq!(binding(&execution, "byte").to_string(), "255");
}

#[test]
fn simd_comparison_yields_bool_mask() {
    let e = run(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar w: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](4, 3, 2, 1)\nvar m: SIMD[DType.bool, 4] = v < w\n",
    );
    assert_eq!(binding(&e, "m").to_string(), "[True, True, False, False]");
}

#[test]
fn float32_division() {
    let e = run(
        "var v: SIMD[DType.float32, 2] = SIMD[DType.float32, 2](3.0, 1.0)\nvar w: SIMD[DType.float32, 2] = v / SIMD[DType.float32, 2](2.0)\n",
    );
    assert_eq!(binding(&e, "w").to_string(), "[1.5, 0.5]");
}

#[test]
fn literal_splats_into_simd_operator() {
    let e = run(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar w: SIMD[DType.int32, 4] = v + 100\n",
    );
    assert_eq!(binding(&e, "w").to_string(), "[101, 102, 103, 104]");
}

#[test]
fn simd_lane_index_out_of_range_is_runtime_error() {
    let err = run_err(
        "var v: SIMD[DType.int32, 2] = SIMD[DType.int32, 2](1, 2)\nvar bad: Int32 = v[5]\n",
    );
    assert!(matches!(err, RuntimeError::TypeError(_)), "got {:?}", err);
}

// --- Exceptions ---

#[test]
fn try_catches_a_raised_error() {
    let e = run(
        "var out: String = \"none\"\ntry:\n    raise Error(\"boom\")\nexcept e:\n    out = \"caught\"\n",
    );
    assert_eq!(binding(&e, "out"), Value::Str("caught".into()));
}

#[test]
fn else_runs_only_without_error_and_finally_always() {
    let ok = run(
        "var log: String = \"\"\ntry:\n    log = log + \"T\"\nexcept e:\n    log = log + \"C\"\nelse:\n    log = log + \"E\"\nfinally:\n    log = log + \"F\"\n",
    );
    assert_eq!(binding(&ok, "log"), Value::Str("TEF".into())); // try, else, finally
    let caught = run(
        "var log: String = \"\"\ntry:\n    raise \"x\"\nexcept e:\n    log = log + \"C\"\nelse:\n    log = log + \"E\"\nfinally:\n    log = log + \"F\"\n",
    );
    assert_eq!(binding(&caught, "log"), Value::Str("CF".into())); // except, finally (no else)
}

#[test]
fn raise_propagates_across_a_function_call() {
    let e = run(
        "def boom() raises -> Int:\n    raise \"deep\"\n    return 0\n\nvar out: String = \"none\"\ntry:\n    var y: Int = boom()\nexcept e:\n    out = \"propagated\"\n",
    );
    assert_eq!(binding(&e, "out"), Value::Str("propagated".into()));
}

#[test]
fn raising_method_is_caught_at_runtime() {
    let actual = output(
        "@fieldwise_init\nstruct Bomb:\n    var code: Int\n    def explode(self) raises -> Int:\n        raise \"boom\"\n\ndef main():\n    var bomb = Bomb(7)\n    try:\n        var ignored = bomb.explode()\n    except error:\n        print(\"caught\")\n",
    );
    assert_eq!(actual, "caught\n");
}

#[test]
fn typed_error_value_is_preserved_and_bound_by_except() {
    let actual = output(
        "@fieldwise_init\nstruct ValidationError:\n    var field: String\n    var reason: String\n\ndef validate() raises ValidationError -> Int:\n    raise ValidationError(\"name\", \"empty\")\n\ndef main():\n    try:\n        var ignored = validate()\n    except error:\n        print(error.field, error.reason)\n",
    );
    assert_eq!(actual, "name empty\n");
}

#[test]
fn parametric_error_effect_erases_to_never_for_nonraising_callback() {
    let actual = output(
        "def run_action[E: AnyType](action: def() raises E -> Int) raises E -> Int:\n    return action()\n\ndef safe() -> Int:\n    return 9\n\ndef main():\n    print(run_action(safe))\n",
    );
    assert_eq!(actual, "9\n");
}

#[test]
fn reraise_with_transfer_sigil() {
    let e = run(
        "var out: String = \"none\"\ntry:\n    try:\n        raise \"inner\"\n    except e:\n        raise e^\nexcept e2:\n    out = \"reraised\"\n",
    );
    assert_eq!(binding(&e, "out"), Value::Str("reraised".into()));
}

#[test]
fn uncaught_raise_is_a_runtime_error() {
    let err = run_err("def main() raises:\n    raise \"unhandled\"\n");
    assert_eq!(err, RuntimeError::Raised(Value::Error("unhandled".into())));
}

#[test]
fn uncaught_raise_propagates_across_a_call() {
    let err = run_err(
        "def f() raises -> Int:\n    raise \"from f\"\n    return 0\n\ndef main() raises:\n    var z: Int = f()\n",
    );
    assert_eq!(err, RuntimeError::Raised(Value::Error("from f".into())));
}

// --- print ---

#[test]
fn print_writes_to_the_output_buffer() {
    let e = output("print(\"Hello, mojito!\")\n");
    assert_eq!(e, "Hello, mojito!\n");
}

#[test]
fn print_joins_multiple_args_with_spaces() {
    let e = output("var a: Int = 2\nprint(a, \"+\", 3, \"=\", a + 3)\n");
    assert_eq!(e, "2 + 3 = 5\n");
}

#[test]
fn print_accumulates_across_calls_and_loops() {
    let e = output("for i in range(3):\n    print(\"i =\", i)\n");
    assert_eq!(e, "i = 0\ni = 1\ni = 2\n");
}

#[test]
fn empty_print_writes_a_blank_line() {
    let e = output("print()\nprint(\"x\")\n");
    assert_eq!(e, "\nx\n");
}

#[test]
fn print_displays_structs_and_simd() {
    let e = output(
        "@fieldwise_init\nstruct P(Writable):\n    var x: Int\n    var y: Int\n    def write_to(self, mut writer: Some[Writer]):\n        writer.write(\"P(\", self.x, \", \", self.y, \")\")\n\nprint(P(1, 2))\nprint(SIMD[DType.int32, 4](1, 2, 3, 4))\n",
    );
    assert_eq!(e, "P(1, 2)\n[1, 2, 3, 4]\n");
}

// --- Builtins: String / abs / min / max / round / len ---

#[test]
fn string_builtin_stringifies_and_concatenates() {
    let e = run("var msg: String = \"n = \" + String(42)\nvar f: String = String(3.5)\n");
    assert_eq!(binding(&e, "msg"), Value::Str("n = 42".into()));
    assert_eq!(binding(&e, "f"), Value::Str("3.5".into()));
}

#[test]
fn abs_of_numbers() {
    let e = run("var a: Int = abs(-7)\nvar b: Float64 = abs(-2.5)\nvar c: UInt = abs(UInt(4))\n");
    assert_eq!(binding(&e, "a"), Value::Int(7));
    assert_eq!(binding(&e, "b"), Value::Float64(2.5));
    assert_eq!(binding(&e, "c"), Value::UInt(4));
}

#[test]
fn min_max_promote_and_compare() {
    let e = run("var lo: Int = min(8, 3)\nvar hi: Int = max(8, 3)\nvar f: Float64 = max(1.0, 2)\n");
    assert_eq!(binding(&e, "lo"), Value::Int(3));
    assert_eq!(binding(&e, "hi"), Value::Int(8));
    assert_eq!(binding(&e, "f"), Value::Float64(2.0));
}

#[test]
fn round_rounds_to_nearest() {
    let e = run(
        "var a: Float64 = round(3.7)\nvar b: Float64 = round(2.4)\nvar c: Float64 = round(1 / 2)\n",
    );
    assert_eq!(binding(&e, "a"), Value::Float64(4.0));
    assert_eq!(binding(&e, "b"), Value::Float64(2.0));
    assert_eq!(binding(&e, "c"), Value::Float64(1.0)); // 0.5 rounds half away from zero
}

#[test]
fn len_of_string() {
    let e = run("var n: Int = len(\"hello\")\nvar z: Int = len(\"\")\n");
    assert_eq!(binding(&e, "n"), Value::Int(5));
    assert_eq!(binding(&e, "z"), Value::Int(0));
}

// --- List (Step 1: construction / read / iterate / len) ---

#[test]
fn list_len_and_index() {
    let e = run(
        "var xs: List[Int] = [10, 20, 30]\nvar n: Int = len(xs)\nvar a: Int = xs[0]\nvar b: Int = xs[2]\n",
    );
    assert_eq!(binding(&e, "n"), Value::Int(3));
    assert_eq!(binding(&e, "a"), Value::Int(10));
    assert_eq!(binding(&e, "b"), Value::Int(30));
}

#[test]
fn list_iteration_accumulates() {
    let e = run(
        "var xs: List[Int] = [1, 2, 3, 4]\nvar sum: Int = 0\nfor x in xs:\n    sum = sum + x\n",
    );
    assert_eq!(binding(&e, "sum"), Value::Int(10));
}

#[test]
fn inferred_list_promotes_numeric_elements() {
    let e = run("var xs: List[Float64] = [1, 2.0, 3]\n");
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![
            Value::Float64(1.0),
            Value::Float64(2.0),
            Value::Float64(3.0)
        ])
    );
}

#[test]
fn list_assignment_is_a_copy() {
    // `var b = a` copies; the two are equal but independent values.
    let e = run("var a: List[Int] = [1, 2, 3]\nvar b: List[Int] = a\n");
    assert_eq!(binding(&e, "a"), binding(&e, "b"));
}

#[test]
fn list_index_out_of_range_is_a_runtime_error() {
    let err = run_err("var xs: List[Int] = [1, 2]\nvar y: Int = xs[5]\n");
    assert!(matches!(err, RuntimeError::TypeError(_)), "got {:?}", err);
}

// --- List (Steps 2 & 3: index-assign, append, pop) ---

#[test]
fn append_builds_a_list_in_a_loop() {
    let e = run(
        "var xs: List[Int] = List[Int]()\nfor i in range(5):\n    xs.append(i * i)\nvar total: Int = 0\nfor x in xs:\n    total = total + x\n",
    );
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(4),
            Value::Int(9),
            Value::Int(16)
        ])
    );
    assert_eq!(binding(&e, "total"), Value::Int(30));
}

#[test]
fn index_assignment_mutates_in_place() {
    let e = run("var xs: List[Int] = [10, 20, 30]\nxs[1] = 99\n");
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![Value::Int(10), Value::Int(99), Value::Int(30)])
    );
}

#[test]
fn pop_returns_and_shrinks() {
    let e = run("var xs: List[Int] = [1, 2, 3]\nvar last: Int = xs.pop()\n");
    assert_eq!(binding(&e, "last"), Value::Int(3));
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
}

#[test]
fn list_copy_is_independent_under_mutation() {
    // The crux of value semantics: mutating the copy must not touch the original.
    let e = run("var a: List[Int] = [1, 2, 3]\nvar b: List[Int] = a\nb.append(4)\n");
    assert_eq!(
        binding(&e, "a"),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        binding(&e, "b"),
        Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4)
        ])
    );
}

#[test]
fn pop_from_empty_list_is_a_runtime_error() {
    let err = run_err("var xs: List[Int] = List[Int]()\nvar y: Int = xs.pop()\n");
    assert!(matches!(err, RuntimeError::TypeError(_)), "got {:?}", err);
}

// --- List (more methods) ---

#[test]
fn insert_and_remove_and_pop_index() {
    let e = run(
        "var xs: List[Int] = [1, 2, 3]\nxs.insert(1, 99)\nvar mid: Int = xs.pop(2)\nxs.remove(99)\n",
    );
    assert_eq!(binding(&e, "mid"), Value::Int(2)); // [1,99,2,3], pop(2) -> 2
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![Value::Int(1), Value::Int(3)])
    );
}

#[test]
fn reverse_clear_extend() {
    let e = run(
        "var a: List[Int] = [1, 2, 3]\na.reverse()\nvar b: List[Int] = [4, 5]\na.extend(b)\nvar n: Int = len(a)\nvar last: Int = a.pop()\na.clear()\nvar empty: Int = len(a)\n",
    );
    assert_eq!(binding(&e, "n"), Value::Int(5)); // [3,2,1,4,5]
    assert_eq!(binding(&e, "last"), Value::Int(5));
    assert_eq!(binding(&e, "empty"), Value::Int(0));
}

#[test]
fn count_and_index() {
    let e = run(
        "var xs: List[Int] = [5, 7, 5, 9, 5]\nvar c: Int = xs.count(5)\nvar i: Int = xs.index(9)\n",
    );
    assert_eq!(binding(&e, "c"), Value::Int(3));
    assert_eq!(binding(&e, "i"), Value::Int(3));
}

#[test]
fn remove_coerces_the_search_value() {
    // remove(2) on a Float64 list matches 2.0.
    let e = run("var xs: List[Float64] = [1.0, 2.0, 3.0]\nxs.remove(2)\n");
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![Value::Float64(1.0), Value::Float64(3.0)])
    );
}

#[test]
fn remove_absent_value_is_a_runtime_error() {
    let err = run_err("var xs: List[Int] = [1, 2]\nxs.remove(9)\n");
    assert!(matches!(err, RuntimeError::TypeError(_)), "got {:?}", err);
}

// --- Membership: in / not in ---

#[test]
fn list_membership_and_not_in() {
    let e = run(
        "var xs: List[Int] = [1, 2, 3]\nvar a: Bool = 2 in xs\nvar b: Bool = 5 in xs\nvar c: Bool = 5 not in xs\n",
    );
    assert_eq!(binding(&e, "a"), Value::Bool(true));
    assert_eq!(binding(&e, "b"), Value::Bool(false));
    assert_eq!(binding(&e, "c"), Value::Bool(true));
}

#[test]
fn string_substring_membership() {
    let e = run(
        "var s: String = \"hello\"\nvar a: Bool = \"ell\" in s\nvar b: Bool = \"z\" not in s\n",
    );
    assert_eq!(binding(&e, "a"), Value::Bool(true));
    assert_eq!(binding(&e, "b"), Value::Bool(true));
}

#[test]
fn membership_coerces_numeric_search_value() {
    let e = run("var xs: List[Float64] = [1.0, 2.0, 3.0]\nvar a: Bool = 2 in xs\n");
    assert_eq!(binding(&e, "a"), Value::Bool(true));
}

#[test]
fn not_in_drives_a_dedup_loop() {
    let e = run(
        "var xs: List[Int] = [3, 1, 4, 1, 5, 9, 4]\nvar seen: List[Int] = List[Int]()\nfor x in xs:\n    if x not in seen:\n        seen.append(x)\n",
    );
    assert_eq!(
        binding(&e, "seen"),
        Value::List(vec![
            Value::Int(3),
            Value::Int(1),
            Value::Int(4),
            Value::Int(5),
            Value::Int(9)
        ])
    );
}

// --- Member-write: place assignment + mut self ---

const EPT: &str = "@fieldwise_init\nstruct Point:\n    var x: Int\n    var y: Int\n    def __copyinit__(out self, existing: Point):\n        self.x = existing.x\n        self.y = existing.y\n\n";

#[test]
fn field_write_mutates_in_place() {
    let e = run(&format!(
        "{EPT}var p: Point = Point(1, 2)\np.x = 10\np.y = p.x + 5\n"
    ));
    assert_eq!(
        binding(&e, "p"),
        Value::Struct {
            name: "Point".into(),
            fields: vec![("x".into(), Value::Int(10)), ("y".into(), Value::Int(15))],
            value_params: vec![],
        }
    );
}

#[test]
fn field_write_is_independent_across_copies() {
    // Value semantics: mutating a copy leaves the original unchanged.
    let e = run(&format!(
        "{EPT}var p: Point = Point(1, 2)\nvar q: Point = p\nq.x = 100\n"
    ));
    let px = match binding(&e, "p") {
        Value::Struct { fields, .. } => fields[0].1.clone(),
        _ => panic!(),
    };
    let qx = match binding(&e, "q") {
        Value::Struct { fields, .. } => fields[0].1.clone(),
        _ => panic!(),
    };
    assert_eq!(px, Value::Int(1));
    assert_eq!(qx, Value::Int(100));
}

#[test]
fn write_to_a_field_of_a_list_element() {
    let e = run(&format!(
        "{EPT}var ps: List[Point] = [Point(1, 1), Point(2, 2)]\nps[1].x = 99\n"
    ));
    let x = match binding(&e, "ps") {
        Value::List(items) => match &items[1] {
            Value::Struct { fields, .. } => fields[0].1.clone(),
            _ => panic!(),
        },
        _ => panic!(),
    };
    assert_eq!(x, Value::Int(99));
}

#[test]
fn mut_self_method_persists_mutation() {
    let e = run(
        "@fieldwise_init\nstruct Counter:\n    var n: Int\n\n    def inc(mut self):\n        self.n = self.n + 1\n\n    def add(mut self, k: Int):\n        self.n = self.n + k\n\nvar c: Counter = Counter(0)\nc.inc()\nc.inc()\nc.add(10)\nvar total: Int = c.n\n",
    );
    assert_eq!(binding(&e, "total"), Value::Int(12));
}

#[test]
fn method_mut_param_writes_back_to_caller() {
    // A method's ordinary `mut` parameter mutates the caller's argument in place
    // (reference semantics), like a free-function `mut` parameter.
    let e = run(
        "@fieldwise_init\nstruct C:\n    var n: Int\n\n    def add_into(self, mut dest: C):\n        dest.n = dest.n + self.n\n\nvar a: C = C(3)\nvar b: C = C(10)\na.add_into(b)\na.add_into(b)\nvar total: Int = b.n\n",
    );
    assert_eq!(binding(&e, "total"), Value::Int(16));
}

#[test]
fn list_method_through_a_field() {
    let e = run(
        "@fieldwise_init\nstruct Bag:\n    var items: List[Int]\n\nvar b: Bag = Bag([1, 2])\nb.items.append(3)\nb.items[0] = 9\nvar n: Int = len(b.items)\n",
    );
    assert_eq!(binding(&e, "n"), Value::Int(3));
    let items = match binding(&e, "b") {
        Value::Struct { fields, .. } => fields[0].1.clone(),
        _ => panic!(),
    };
    assert_eq!(
        items,
        Value::List(vec![Value::Int(9), Value::Int(2), Value::Int(3)])
    );
}

// --- Augmented assignment ---

#[test]
fn augmented_assignment_arithmetic() {
    let e = run("var i: Int = 10\ni += 5\ni -= 2\ni *= 3\ni //= 4\ni %= 7\ni **= 2\n");
    assert_eq!(binding(&e, "i"), Value::Int(4));
    let s = run("var s: String = \"a\"\ns += \"bc\"\n");
    assert_eq!(binding(&s, "s"), Value::Str("abc".into()));
}

#[test]
fn augmented_assignment_on_field_index_and_mut_self() {
    let e = run(
        "@fieldwise_init\nstruct Counter:\n    var n: Int\n\n    def bump(mut self, k: Int):\n        self.n += k\n\nvar c: Counter = Counter(0)\nc.n += 100\nc.bump(5)\nvar xs: List[Int] = [1, 2, 3]\nxs[1] += 10\n",
    );
    let n = match binding(&e, "c") {
        Value::Struct { fields, .. } => fields[0].1.clone(),
        _ => panic!(),
    };
    assert_eq!(n, Value::Int(105));
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![Value::Int(1), Value::Int(12), Value::Int(3)])
    );
}

#[test]
fn augmented_assignment_evaluates_the_place_once() {
    // `xs[idx(1)] += 5` must call `idx` exactly once (single read-modify-write) —
    // observed via `idx`'s single `print`.
    let out = output(
        "def idx(i: Int) -> Int:\n    print(\"idx\", i)\n    return i\n\ndef main():\n    var xs: List[Int] = [10, 20, 30]\n    xs[idx(1)] += 5\n    print(xs[0], xs[1], xs[2])\n",
    );
    assert_eq!(out, "idx 1\n10 25 30\n");
}

// --- SIMD lane writes ---

fn lane(v: &Value, i: usize) -> i128 {
    match v {
        Value::Simd {
            lanes: mojito::runtime::SimdLanes::Int(l),
            ..
        } => l[i],
        _ => panic!("not an int SIMD"),
    }
}

#[test]
fn simd_lane_write_scalar_and_splat_and_aug() {
    let e = run(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nv[0] = 10\nv[1] = Int32(20)\nv[2] += 100\n",
    );
    let v = binding(&e, "v");
    assert_eq!(lane(&v, 0), 10);
    assert_eq!(lane(&v, 1), 20);
    assert_eq!(lane(&v, 2), 103);
    assert_eq!(lane(&v, 3), 4);
}

#[test]
fn simd_lane_write_wraps_to_element_width() {
    // int8: 200 wraps to -56.
    let e = run("var b: SIMD[DType.int8, 2] = SIMD[DType.int8, 2](0, 0)\nb[0] = 200\n");
    assert_eq!(lane(&binding(&e, "b"), 0), -56);
}

#[test]
fn simd_lane_write_through_a_struct_field() {
    let e = run(
        "@fieldwise_init\nstruct V:\n    var data: SIMD[DType.int32, 4]\n\nvar s: V = V(SIMD[DType.int32, 4](5, 6, 7, 8))\ns.data[3] = 99\n",
    );
    let data = match binding(&e, "s") {
        Value::Struct { fields, .. } => fields[0].1.clone(),
        _ => panic!(),
    };
    assert_eq!(lane(&data, 3), 99);
}

#[test]
fn simd_lane_write_out_of_range_is_a_runtime_error() {
    let err = run_err("var v: SIMD[DType.int32, 2] = SIMD[DType.int32, 2](1, 2)\nv[5] = 0\n");
    assert!(matches!(err, RuntimeError::TypeError(_)), "got {:?}", err);
}

// --- Float64 / SIMD unification ---

#[test]
fn width1_float64_simd_is_a_native_float64() {
    // Constructing SIMD[DType.float64, 1] yields a Value::Float64 (canonicalized).
    let e =
        run("var a: SIMD[DType.float64, 1] = SIMD[DType.float64, 1](3.5)\nvar b: Float64 = a\n");
    assert_eq!(binding(&e, "a"), Value::Float64(3.5));
    assert_eq!(binding(&e, "b"), Value::Float64(3.5));
}

#[test]
fn float64_vector_arithmetic_and_lane_read() {
    let e = run(
        "var v: SIMD[DType.float64, 4] = SIMD[DType.float64, 4](1.0, 2.0, 3.0, 4.0)\nvar d: SIMD[DType.float64, 4] = v + v\nvar lane: Float64 = d[3]\n",
    );
    assert_eq!(binding(&e, "lane"), Value::Float64(8.0)); // (4+4)
}

#[test]
fn float64_lane_write_and_aug_and_splat() {
    let e = run(
        "var a: Float64 = 100.0\nvar v: SIMD[DType.float64, 4] = SIMD[DType.float64, 4](1.0, 2.0, 3.0, 4.0)\nv[0] = a\nv[1] += 10.0\nvar lane0: Float64 = v[0]\nvar lane1: Float64 = v[1]\n",
    );
    assert_eq!(binding(&e, "lane0"), Value::Float64(100.0));
    assert_eq!(binding(&e, "lane1"), Value::Float64(12.0));
}

#[test]
fn float64_keeps_full_precision_unlike_float32() {
    // float64 does NOT round to single precision; float32 does.
    let e = run(
        "var a: Float64 = 0.1\nvar big: SIMD[DType.float64, 1] = SIMD[DType.float64, 1](0.1)\nvar eq: Bool = big == a\n",
    );
    assert_eq!(binding(&e, "eq"), Value::Bool(true));
    let f32 = run("var s: SIMD[DType.float32, 1] = SIMD[DType.float32, 1](0.1)\n");
    // The float32 lane is rounded, so it differs from the exact f64 0.1.
    assert_ne!(
        binding(&f32, "s"),
        Value::Simd {
            dtype: mojito::Dtype::Float32,
            lanes: mojito::runtime::SimdLanes::Float(vec![0.1])
        }
    );
}

// --- Walrus ---

#[test]
fn walrus_binds_and_produces_its_value() {
    assert_eq!(
        output("def main():\n    var y: Int = (n := 5)\n    print(y, n)\n"),
        "5 5\n"
    );
}

#[test]
fn unsafe_pointer_provenance_arithmetic_and_deallocation() {
    assert_eq!(
        output(
            "def main():\n    var base = UnsafePointer[Int].alloc_aligned(3, 16)\n    base[1] = 42\n    var next = base + 1\n    print(next[0], next - base, next == base + 1)\n    base.free()\n"
        ),
        "42 1 True\n"
    );
    assert!(run_err("def main():\n    var p = UnsafePointer[Int].alloc(1)\n    var q = p\n    p.free()\n    print(q[0])\n").to_string().contains("use after"));
}

// --- Inferred `var` ---

#[test]
fn inferred_var_takes_the_values_natural_type() {
    let e = run("var i = 5\nvar f = 3.5\nvar s = \"hi\"\nvar b = True\nvar g = 1 + 2\n");
    assert_eq!(binding(&e, "i"), Value::Int(5));
    assert_eq!(binding(&e, "f"), Value::Float64(3.5));
    assert_eq!(binding(&e, "s"), Value::Str("hi".into()));
    assert_eq!(binding(&e, "b"), Value::Bool(true));
    assert_eq!(binding(&e, "g"), Value::Int(3));
}

#[test]
fn inferred_var_list_is_mutable_and_reassignable() {
    let e = run("var xs = [1, 2]\nxs.append(3)\nvar total = 0\nfor x in xs:\n    total += x\n");
    assert_eq!(
        binding(&e, "xs"),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
    assert_eq!(binding(&e, "total"), Value::Int(6));
}

// --- Tuples ---

#[test]
fn tuple_construction_indexing_and_value_semantics() {
    let e = run(
        "var t: Tuple[Int, Float64, String] = (1, 2.5, \"hi\")\nvar a: Int = t[0]\nvar b: Float64 = t[1]\nvar c: String = t[2]\n",
    );
    assert_eq!(binding(&e, "a"), Value::Int(1));
    assert_eq!(binding(&e, "b"), Value::Float64(2.5));
    assert_eq!(binding(&e, "c"), Value::Str("hi".into()));
    assert_eq!(
        binding(&e, "t"),
        Value::Tuple(vec![
            Value::Int(1),
            Value::Float64(2.5),
            Value::Str("hi".into())
        ])
    );
}

#[test]
fn tuple_element_coercion_at_runtime() {
    // `(1, 2)` into `Tuple[Float64, Float64]` materializes each element to Float64.
    let e = run("var t: Tuple[Float64, Float64] = (1, 2)\n");
    assert_eq!(
        binding(&e, "t"),
        Value::Tuple(vec![Value::Float64(1.0), Value::Float64(2.0)])
    );
}

#[test]
fn function_returns_a_tuple() {
    let e = run(
        "def stats() -> Tuple[Int, Int]:\n    return (512, 4)\n\nvar s = stats()\nvar points: Int = s[0]\nvar scans: Int = s[1]\n",
    );
    assert_eq!(binding(&e, "points"), Value::Int(512));
    assert_eq!(binding(&e, "scans"), Value::Int(4));
}

#[test]
fn default_argument_values_fill_missing_trailing_args() {
    // Omitted trailing arg uses the default; provided arg overrides it.
    let e = output(
        "def p(b: Int, e: Int = 2) -> Int:\n    return b ** e\n\ndef main():\n    print(p(3))\n    print(p(3, 3))\n",
    );
    assert_eq!(e, "9\n27\n");
}

#[test]
fn main_is_called_as_entry_point() {
    let e = output("def main():\n    print(\"hi\")\n");
    assert_eq!(e, "hi\n");
}

#[test]
fn keyword_arguments_bind_by_name() {
    // Keyword args match by name regardless of order; mix with positional + default.
    let e = output(
        "def sub(a: Int, b: Int, c: Int = 100) -> Int:\n    return a - b + c\n\ndef main():\n    print(sub(10, 3))\n    print(sub(b=3, a=10))\n    print(sub(10, c=0, b=3))\n",
    );
    assert_eq!(e, "107\n107\n7\n");
}

#[test]
fn variadic_args_collects_extra_positional_args() {
    let e = output(
        "def sum(*values: Int) -> Int:\n    var t: Int = 0\n    for v in values:\n        t = t + v\n    return t\n\ndef main():\n    print(sum())\n    print(sum(1, 2, 3))\n    print(sum(10, 20, 30, 40))\n",
    );
    assert_eq!(e, "0\n6\n100\n");
}

#[test]
fn heterogeneous_variadic_pack_preserves_all_runtime_values() {
    assert_eq!(
        output(
            "def count[*ArgTypes: AnyType](*args: *ArgTypes) -> Int:\n    return len(args)\n\ndef main():\n    print(count(1, \"two\", True))\n"
        ),
        "3\n"
    );
}

#[test]
fn variadic_args_after_regular_params() {
    let e = output(
        "def tag(label: String, *nums: Int) -> Int:\n    return len(nums)\n\ndef main():\n    print(tag(\"a\"))\n    print(tag(\"a\", 1, 2, 3))\n",
    );
    assert_eq!(e, "0\n3\n");
}

#[test]
fn mut_param_writes_back_to_caller() {
    // A `mut` reference parameter mutates the caller's variable.
    let ev = output(
        "def incr(mut x: Int):\n    x = x + 1\n\ndef main():\n    var n: Int = 5\n    incr(n)\n    print(n)\n",
    );
    assert_eq!(ev, "6\n");
}

#[test]
fn mut_param_mutates_a_struct_field() {
    let ev = output(
        "@fieldwise_init\nstruct Counter:\n    var n: Int\n\ndef bump(mut c: Counter, k: Int):\n    c.n = c.n + k\n\ndef main():\n    var c: Counter = Counter(0)\n    bump(c, 5)\n    bump(c, 3)\n    print(c.n)\n",
    );
    assert_eq!(ev, "8\n");
}

#[test]
fn ref_param_also_writes_back() {
    // `ref` (a reference) is modeled like `mut` for write-back.
    let ev = output(
        "def set_to(ref x: Int, v: Int):\n    x = v\n\ndef main():\n    var n: Int = 0\n    set_to(n, 42)\n    print(n)\n",
    );
    assert_eq!(ev, "42\n");
}
