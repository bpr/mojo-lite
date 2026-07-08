use mojo_lite::{Lexer, Parser, TypeError, check};

/// Parse `source` and run the type checker, returning its result.
fn check_source(source: &str) -> Result<(), TypeError> {
    let program = Parser::new(Lexer::new(source))
        .parse_program()
        .expect("parse error");
    check(&program)
}

/// Type-check a program that is expected to pass.
fn ok(source: &str) {
    check_source(source).expect("expected the program to type-check");
}

/// Type-check a program that is expected to fail, returning the error.
fn err(source: &str) -> TypeError {
    check_source(source).expect_err("expected a type error")
}

// --- Programs that should type-check ---

#[test]
fn accepts_well_typed_declarations() {
    ok(
        "var a: Int = 1 + 2 * 3\nvar ok: Bool = 1 < 2 and not False\nvar s: String = \"a\" + \"b\"\n",
    );
}

#[test]
fn accepts_functions_and_calls() {
    ok("def add(x: Int, y: Int) -> Int:\n    return x + y\n\nvar s: Int = add(3, 4)\n");
}

#[test]
fn accepts_recursion() {
    ok("def f(n: Int) -> Int:\n    return f(n)\n\nvar x: Int = f(0)\n");
}

#[test]
fn accepts_lexical_capture_downward_funarg() {
    ok(
        "def adder(n: Int) -> Int:\n    def add_n(x: Int) -> Int:\n        return x + n\n    return add_n(100)\n\nvar c: Int = adder(42)\n",
    );
}

#[test]
fn accepts_shadowing_across_scopes() {
    ok("var x: Int = 1\ndef shadow() -> Int:\n    var x: Int = 99\n    return x\n");
}

#[test]
fn accepts_omitted_return_type_as_none() {
    ok("def noop(x: Int):\n    return\n\nnoop(1)\n");
}

// --- Annotation enforcement ---

#[test]
fn rejects_var_init_type_mismatch() {
    let e = err("var x: Int = True\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_wrong_argument_type() {
    let e = err("def f(x: Int) -> Int:\n    return x\n\nvar y: Int = f(True)\n");
    match e {
        TypeError::TypeMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, "Int");
            assert_eq!(found, "Bool");
        }
        other => panic!("expected a type mismatch, got {:?}", other),
    }
}

#[test]
fn rejects_wrong_return_type() {
    let e = err("def f() -> Int:\n    return True\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_arity_mismatch() {
    let e = err("def f(x: Int) -> Int:\n    return x\n\nvar y: Int = f(1, 2)\n");
    assert_eq!(
        e,
        TypeError::ArityMismatch {
            name: "f".into(),
            expected: 1,
            got: 2
        }
    );
}

// --- Errors that used to surface only at runtime ---

#[test]
fn rejects_operand_type_mismatch() {
    let e = err("var x: Int = 1 + True\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_ordering_on_strings() {
    let e = err("var x: Bool = \"a\" < \"b\"\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_cross_type_equality() {
    let e = err("var x: Bool = 1 == True\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_undefined_variable() {
    let e = err("var x: Int = y\n");
    assert_eq!(e, TypeError::UndefinedVariable("y".into()));
}

#[test]
fn rejects_calling_a_non_function() {
    let e = err("var x: Int = 1\nx(2)\n");
    assert!(matches!(e, TypeError::NotCallable { .. }), "got {:?}", e);
}

#[test]
fn rejects_escaping_closure() {
    let e =
        err("def make() -> Int:\n    def helper() -> Int:\n        return 1\n    return helper\n");
    assert_eq!(e, TypeError::ClosureEscape);
}

// --- Rules Mojo enforces statically but the evaluator did not ---

#[test]
fn rejects_same_scope_redeclaration() {
    let e = err("var x: Int = 1\nvar x: Int = 2\n");
    assert_eq!(e, TypeError::Redeclaration("x".into()));
}

#[test]
fn rejects_var_redeclaring_param() {
    let e = err("def f(x: Int) -> Int:\n    var x: Int = 1\n    return x\n");
    assert_eq!(e, TypeError::Redeclaration("x".into()));
}

#[test]
fn rejects_return_outside_function() {
    let e = err("return 1\n");
    assert_eq!(e, TypeError::ReturnOutsideFunction);
}

// --- Structs ---

const POINT: &str = "@fieldwise_init\nstruct Point:\n    var x: Int\n    var y: Int\n\n    def sum(self) -> Int:\n        return self.x + self.y\n\n";

#[test]
fn accepts_struct_construction_field_and_method() {
    ok(&format!(
        "{POINT}var p: Point = Point(1, 2)\nvar s: Int = p.sum()\nvar a: Int = p.x\n"
    ));
}

#[test]
fn accepts_struct_typed_field_and_chained_access() {
    ok(
        "@fieldwise_init\nstruct Inner:\n    var v: Int\n\n@fieldwise_init\nstruct Outer:\n    var inner: Inner\n    var tag: Int\n\nvar o: Outer = Outer(Inner(5), 9)\nvar v: Int = o.inner.v\n",
    );
}

#[test]
fn rejects_unknown_field() {
    let e = err(&format!(
        "{POINT}var p: Point = Point(1, 2)\nvar z: Int = p.z\n"
    ));
    assert!(matches!(e, TypeError::NoSuchField { .. }), "got {:?}", e);
}

#[test]
fn rejects_unknown_method() {
    let e = err(&format!(
        "{POINT}var p: Point = Point(1, 2)\nvar z: Int = p.nope()\n"
    ));
    assert!(matches!(e, TypeError::NoSuchMethod { .. }), "got {:?}", e);
}

#[test]
fn rejects_construction_arity_and_type() {
    assert!(matches!(
        err(&format!("{POINT}var p: Point = Point(1)\n")),
        TypeError::ArityMismatch { .. }
    ));
    assert!(matches!(
        err(&format!("{POINT}var p: Point = Point(1, True)\n")),
        TypeError::TypeMismatch { .. }
    ));
}

#[test]
fn rejects_struct_without_constructor() {
    let e = err("struct P:\n    var x: Int\n\nvar p: P = P(1)\n");
    assert_eq!(e, TypeError::NoConstructor("P".into()));
}

#[test]
fn rejects_unknown_type_annotation() {
    let e = err("var p: Bogus = 1\n");
    assert_eq!(e, TypeError::UnknownType("Bogus".into()));
}

#[test]
fn rejects_member_access_on_non_struct() {
    let e = err("var x: Int = 1\nvar y: Int = x.field\n");
    assert!(matches!(e, TypeError::NoSuchField { .. }), "got {:?}", e);
}

#[test]
fn checks_method_body_against_field_types() {
    // `get` is declared to return Int but returns a Bool field.
    let e = err(
        "@fieldwise_init\nstruct P:\n    var b: Bool\n\n    def get(self) -> Int:\n        return self.b\n",
    );
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

// --- Numbers: Float64, UInt, conversions ---

#[test]
fn accepts_float64_arithmetic_and_division() {
    ok("var x: Float64 = 1.5 + 2.0 * 3.0\nvar y: Float64 = 10.0 / 4.0\nvar ok: Bool = x < y\n");
}

#[test]
fn accepts_uint_via_conversion() {
    ok("var u: UInt = UInt(0)\nu = u + UInt(1)\nvar ok: Bool = u < UInt(10)\n");
}

#[test]
fn accepts_numeric_conversions() {
    ok("var i: Int = Int(3.9)\nvar f: Float64 = Float64(3)\nvar u: UInt = UInt(True)\n");
}

#[test]
fn int_literal_coerces_to_uint_and_float() {
    // Literal coercion: bare literals materialize to the annotated type.
    ok("var u: UInt = 0\nvar f: Float64 = 3\nvar g: Float64 = 1 + 2\nu = u + 1\n");
}

#[test]
fn literal_arithmetic_mixes_but_concrete_types_do_not() {
    // Mixing two literals is fine (`1.0 + 2` is Float64)...
    ok("var x: Float64 = 1.0 + 2\n");
    // ...but mixing two *concrete* numeric types is rejected.
    let e = err("var i: Int = 1\nvar u: UInt = UInt(2)\nvar bad: Int = i + u\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn true_division_yields_float64() {
    // `/` returns Float64 even on integers, so `7 / 2` is 3.5 (not an Int).
    ok("var x: Float64 = 7 / 2\n");
    let e = err("var x: Int = 7 / 2\n");
    match e {
        TypeError::TypeMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, "Int");
            assert_eq!(found, "Float64");
        }
        other => panic!("expected a type mismatch, got {:?}", other),
    }
}

#[test]
fn floor_div_mod_pow_preserve_type() {
    ok(
        "var a: Int = 7 // 2\nvar b: Int = 7 % 2\nvar c: Int = 2 ** 10\nvar d: Float64 = 7.0 // 2.0\n",
    );
}

#[test]
fn rejects_unary_minus_on_uint() {
    let e = err("var u: UInt = UInt(1)\nvar n: UInt = -u\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_cross_numeric_equality() {
    let e = err("var ok: Bool = Int(1) == UInt(1)\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_conversion_of_string() {
    let e = err("var i: Int = Int(\"5\")\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

// --- Assignment ---

#[test]
fn accepts_assignment_keeping_the_declared_type() {
    ok("var x: Int = 1\nx = 2\nx = x + 40\n");
}

#[test]
fn accepts_assignment_to_an_outer_variable_from_a_loop() {
    ok("var total: Int = 0\nfor i in range(10):\n    total = total + i\n");
}

#[test]
fn var_less_introduction_type_checks_as_implicit_declaration() {
    // `x = 1` on an undeclared name is a var-less introduction: parsed and
    // type-checked (binding the implicit var), then flagged unsupported at eval.
    ok("x = 1\nvar y: Int = x + 1\n");
}

#[test]
fn rejects_assignment_of_wrong_type() {
    let e = err("var x: Int = 1\nx = True\n");
    match e {
        TypeError::TypeMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, "Int");
            assert_eq!(found, "Bool");
        }
        other => panic!("expected a type mismatch, got {:?}", other),
    }
}

#[test]
fn rejects_assigning_a_closure() {
    let e = err("def f() -> Int:\n    return 1\n\ndef g() -> Int:\n    return 2\n\nf = g\n");
    assert_eq!(e, TypeError::ClosureEscape);
}

// --- Control flow: if / while / for / break / continue ---

#[test]
fn accepts_if_elif_else() {
    ok(
        "var x: Int = 3\nif x > 0:\n    var a: Int = 1\nelif x == 0:\n    var b: Int = 2\nelse:\n    var c: Int = 3\n",
    );
}

#[test]
fn accepts_while_with_bool_condition() {
    ok("if True:\n    while False:\n        break\n");
}

#[test]
fn accepts_for_over_range_with_int_loop_var() {
    // The loop variable is Int and is in scope in the body.
    ok("for i in range(10):\n    var x: Int = i + 1\n");
}

#[test]
fn accepts_range_with_one_two_or_three_args() {
    ok("for i in range(0, 10, 2):\n    pass\n");
}

#[test]
fn accepts_break_and_continue_inside_loop() {
    ok("for i in range(10):\n    if i == 5:\n        break\n    continue\n");
}

#[test]
fn rejects_non_bool_if_condition() {
    let e = err("if 1:\n    pass\n");
    match e {
        TypeError::TypeMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, "Bool");
            assert_eq!(found, "Int");
        }
        other => panic!("expected a type mismatch, got {:?}", other),
    }
}

#[test]
fn rejects_non_bool_while_condition() {
    let e = err("while 1:\n    pass\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_for_over_non_range() {
    let e = err("for i in 5:\n    pass\n");
    match e {
        TypeError::TypeMismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, "range, List, or a type with __iter__");
            assert_eq!(found, "Int");
        }
        other => panic!("expected a type mismatch, got {:?}", other),
    }
}

#[test]
fn rejects_range_with_non_int_argument() {
    let e = err("for i in range(True):\n    pass\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_range_with_no_arguments() {
    let e = err("for i in range():\n    pass\n");
    assert!(matches!(e, TypeError::ArityMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_break_outside_loop() {
    let e = err("break\n");
    assert_eq!(e, TypeError::BreakOutsideLoop);
}

#[test]
fn rejects_continue_outside_loop() {
    let e = err("continue\n");
    assert_eq!(e, TypeError::ContinueOutsideLoop);
}

#[test]
fn rejects_break_in_nested_def_inside_loop() {
    // A function boundary resets the loop context.
    let e = err("while True:\n    def g() -> Int:\n        break\n        return 1\n");
    assert_eq!(e, TypeError::BreakOutsideLoop);
}

#[test]
fn loop_variable_does_not_leak_after_for() {
    // `i` is scoped to the loop body, so it is undefined afterwards.
    let e = err("for i in range(3):\n    pass\nvar x: Int = i\n");
    assert_eq!(e, TypeError::UndefinedVariable("i".into()));
}

// --- Parameterization (generics) ---

/// A generic `Pair[T]` struct used by several tests below.
const PAIR: &str = "@fieldwise_init\nstruct Pair[T: Copyable & Movable]:\n    var left: Self.T\n    var right: Self.T\n";

#[test]
fn accepts_generic_struct_construction_and_member() {
    ok(&format!(
        "{PAIR}var p: Pair[Int] = Pair(3, 4)\nvar a: Int = p.left\nvar b: Int = p.right\n"
    ));
}

#[test]
fn infers_struct_type_argument_from_construction() {
    // No explicit `Pair[Int]` annotation: T is inferred from the arguments.
    ok(&format!(
        "{PAIR}var p: Pair[Float64] = Pair(1.5, 2.5)\nvar f: Float64 = p.left\n"
    ));
}

#[test]
fn accepts_generic_function_identity() {
    ok(
        "def id[T: Copyable & Movable](x: T) -> T:\n    return x\n\nvar n: Int = id(5)\nvar s: String = id(\"hi\")\n",
    );
}

#[test]
fn accepts_generic_function_over_generic_struct() {
    ok(&format!(
        "{PAIR}def first[T: Copyable & Movable](p: Pair[T]) -> T:\n    return p.left\n\nvar p: Pair[Int] = Pair(1, 2)\nvar x: Int = first(p)\n"
    ));
}

#[test]
fn accepts_generic_struct_method_returning_self_param() {
    ok(
        "@fieldwise_init\nstruct Box[T: Copyable & Movable]:\n    var val: Self.T\n\n    def get(self) -> Self.T:\n        return self.val\n\nvar b: Box[Int] = Box(7)\nvar g: Int = b.get()\n",
    );
}

#[test]
fn rejects_operator_on_opaque_type_parameter() {
    // An unconstrained `T` supports no operators.
    let e = err("def bad[T: Copyable & Movable](x: T) -> T:\n    return x + x\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_wrong_struct_type_argument() {
    let e = err(&format!("{PAIR}var p: Pair[Int] = Pair(1.5, 2.5)\n"));
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_conflicting_type_parameter_solutions() {
    // A concrete Float64 parameter and an Int-literal argument do not unify:
    // the literal is defaulted to Int, so `Pair(1.0, 2)` is a conflict, not
    // `Pair[Float64]` (keeps the type-erased evaluator sound).
    let e = err(&format!("{PAIR}var p: Pair[Float64] = Pair(1.0, 2)\n"));
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_unknown_trait_bound() {
    let e = err("def bad[T: Frobnicate](x: T) -> T:\n    return x\n");
    assert_eq!(e, TypeError::UnknownTrait("Frobnicate".into()));
}

#[test]
fn rejects_bare_type_parameter_as_struct_field() {
    // Inside a struct a parameter must be written `Self.T`, not bare `T`.
    let e = err("@fieldwise_init\nstruct Bad[T: Copyable & Movable]:\n    var v: T\n");
    assert_eq!(e, TypeError::UnknownType("T".into()));
}

#[test]
fn rejects_wrong_type_argument_count() {
    let e = err(
        "@fieldwise_init\nstruct Box[T: Copyable & Movable]:\n    var v: Self.T\n\nvar b: Box[Int, Int] = Box(1)\n",
    );
    assert!(
        matches!(
            e,
            TypeError::WrongTypeArgCount {
                expected: 1,
                got: 2,
                ..
            }
        ),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_type_arguments_on_non_generic_struct() {
    let e = err("@fieldwise_init\nstruct Point:\n    var x: Int\n\nvar p: Point[Int] = Point(1)\n");
    assert!(
        matches!(
            e,
            TypeError::WrongTypeArgCount {
                expected: 0,
                got: 1,
                ..
            }
        ),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_self_param_naming_unknown_parameter() {
    let e = err("@fieldwise_init\nstruct Box[T: Copyable & Movable]:\n    var v: Self.U\n");
    assert_eq!(e, TypeError::UnknownSelfParam("U".into()));
}

#[test]
fn rejects_uninferable_type_parameter() {
    // `T` is not mentioned by any field, so a construction can't solve it (there
    // is no explicit `Phantom[Int](...)` syntax to supply it).
    let e = err(
        "@fieldwise_init\nstruct Phantom[T: AnyType]:\n    var x: Int\n\nvar p: Phantom[Int] = Phantom(1)\n",
    );
    assert!(
        matches!(&e, TypeError::CannotInferTypeParam { param, .. } if param == "T"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_duplicate_type_parameter_names() {
    let e = err("def f[T: AnyType, T: AnyType](x: T) -> T:\n    return x\n");
    assert_eq!(e, TypeError::Redeclaration("T".into()));
}

// --- Traits (Phase 1b) ---

/// A `Quackable` trait, a conforming `Duck`, and a bounded generic function.
const QUACK: &str = "trait Quackable:\n    def quack(self) -> String:\n        ...\n\n@fieldwise_init\nstruct Duck(Quackable):\n    var name: String\n\n    def quack(self) -> String:\n        return \"Quack\"\n\ndef make_it_quack[T: Quackable](x: T) -> String:\n    return x.quack()\n";

#[test]
fn accepts_trait_conformance_and_bounded_call() {
    ok(&format!(
        "{QUACK}var d: Duck = Duck(\"Donald\")\nvar s: String = make_it_quack(d)\n"
    ));
}

#[test]
fn accepts_self_type_in_trait_requirement() {
    ok(
        "trait Eq2:\n    def same(self, other: Self) -> Bool:\n        ...\n\n@fieldwise_init\nstruct P(Eq2):\n    var x: Int\n\n    def same(self, other: Self) -> Bool:\n        return self.x == other.x\n\nvar a: P = P(1)\nvar r: Bool = a.same(P(2))\n",
    );
}

#[test]
fn accepts_calling_bound_trait_method_on_type_parameter() {
    // Inside `make_it_quack`, `x: T` with `T: Quackable` can call `quack()`.
    ok(QUACK);
}

#[test]
fn rejects_argument_not_conforming_to_bound() {
    let e = err(&format!(
        "{QUACK}@fieldwise_init\nstruct Cat:\n    var n: Int\n\nvar s: String = make_it_quack(Cat(1))\n"
    ));
    assert!(
        matches!(&e, TypeError::TraitNotSatisfied { trait_name, .. } if trait_name == "Quackable"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_struct_missing_a_required_trait_method() {
    let e = err(
        "trait Quackable:\n    def quack(self) -> String:\n        ...\n\n@fieldwise_init\nstruct Duck(Quackable):\n    var name: String\n",
    );
    assert!(
        matches!(&e, TypeError::MissingTraitMethod { method, .. } if method == "quack"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_struct_with_mismatched_trait_method_signature() {
    // `quack` returns Int, but the trait requires it to return String.
    let e = err(
        "trait Quackable:\n    def quack(self) -> String:\n        ...\n\n@fieldwise_init\nstruct Duck(Quackable):\n    var name: String\n\n    def quack(self) -> Int:\n        return 1\n",
    );
    assert!(
        matches!(e, TypeError::TraitMethodMismatch { .. }),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_method_not_required_by_any_bound() {
    // `waddle` is not required by `Quackable`, so it can't be called on a `T`.
    let e = err(
        "trait Quackable:\n    def quack(self) -> String:\n        ...\n\ndef f[T: Quackable](x: T) -> String:\n    return x.waddle()\n",
    );
    assert!(matches!(e, TypeError::NoSuchMethod { .. }), "got {:?}", e);
}

#[test]
fn rejects_unknown_trait_in_conformance_list() {
    let e = err("@fieldwise_init\nstruct D(Bogus):\n    var n: Int\n");
    assert_eq!(e, TypeError::UnknownTrait("Bogus".into()));
}

#[test]
fn rejects_bounded_type_parameter_forwarded_to_stronger_bound() {
    // `T: AnyType` cannot be passed where `Quackable` is required.
    let e = err(
        "trait Quackable:\n    def quack(self) -> String:\n        ...\n\ndef needs[U: Quackable](x: U) -> String:\n    return x.quack()\n\ndef weak[T: AnyType](x: T) -> String:\n    return needs(x)\n",
    );
    assert!(
        matches!(e, TypeError::TraitNotSatisfied { .. }),
        "got {:?}",
        e
    );
}

#[test]
fn accepts_forwarding_a_bounded_type_parameter() {
    // `T: Quackable` may be forwarded to a `[U: Quackable]` parameter.
    ok(
        "trait Quackable:\n    def quack(self) -> String:\n        ...\n\ndef needs[U: Quackable](x: U) -> String:\n    return x.quack()\n\ndef fwd[T: Quackable](x: T) -> String:\n    return needs(x)\n",
    );
}

#[test]
fn rejects_duplicate_trait_declaration() {
    let e = err(
        "trait Q:\n    def m(self) -> Int:\n        ...\n\ntrait Q:\n    def n(self) -> Int:\n        ...\n",
    );
    assert_eq!(e, TypeError::Redeclaration("Q".into()));
}

// --- Value parameters + comptime (Phase 2) ---

const FIXEDBUF: &str = "@fieldwise_init\nstruct FixedBuffer[size: Int]:\n    var tag: Int\n\n    def capacity(self) -> Int:\n        return Self.size\n";

#[test]
fn accepts_value_parameter_struct_and_self_read() {
    ok(&format!(
        "{FIXEDBUF}var b: FixedBuffer[8] = FixedBuffer[8](0)\nvar c: Int = b.capacity()\n"
    ));
}

#[test]
fn accepts_comptime_arithmetic_argument() {
    ok(&format!(
        "{FIXEDBUF}var b: FixedBuffer[2 + 3 * 2] = FixedBuffer[8](0)\n"
    ));
}

#[test]
fn accepts_value_parameter_function() {
    ok("def doubled[n: Int]() -> Int:\n    return n * 2\n\nvar d: Int = doubled[21]()\n");
}

#[test]
fn accepts_comptime_constant_as_value_argument() {
    ok(&format!(
        "comptime N = 4 + 4\n{FIXEDBUF}var b: FixedBuffer[N] = FixedBuffer[N](0)\nvar n: Int = N\n"
    ));
}

#[test]
fn accepts_explicit_type_argument() {
    ok(
        "@fieldwise_init\nstruct Box[T: Copyable & Movable]:\n    var v: Self.T\n\nvar b: Box[Int] = Box[Int](5)\n",
    );
}

#[test]
fn distinct_value_arguments_are_distinct_types() {
    // `FixedBuffer[5]` and `FixedBuffer[6]` are different types.
    let e = err(&format!(
        "{FIXEDBUF}var b: FixedBuffer[5] = FixedBuffer[6](0)\n"
    ));
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_uninferable_value_parameter() {
    // A value parameter cannot be inferred; it must be supplied explicitly.
    let e = err(&format!(
        "{FIXEDBUF}var b: FixedBuffer[4] = FixedBuffer(0)\n"
    ));
    assert!(
        matches!(&e, TypeError::CannotInferTypeParam { param, .. } if param == "size"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_non_int_value_parameter_type() {
    let e = err("@fieldwise_init\nstruct Bad[n: Float64]:\n    var tag: Int\n");
    assert!(
        matches!(&e, TypeError::BadValueParamType { name, .. } if name == "n"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_non_comptime_value_argument() {
    // A runtime variable is not a compile-time constant.
    let e = err(&format!(
        "{FIXEDBUF}var x: Int = 3\nvar b: FixedBuffer[x] = FixedBuffer[x](0)\n"
    ));
    assert!(matches!(e, TypeError::NotComptime(_)), "got {:?}", e);
}

#[test]
fn rejects_type_argument_for_value_parameter() {
    let e = err(&format!(
        "{FIXEDBUF}var b: FixedBuffer[Int] = FixedBuffer[Int](0)\n"
    ));
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_wrong_parameter_count() {
    let e = err(&format!(
        "{FIXEDBUF}var b: FixedBuffer[8, 9] = FixedBuffer[8, 9](0)\n"
    ));
    assert!(
        matches!(e, TypeError::WrongTypeArgCount { .. }),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_non_comptime_constant_definition() {
    // A `comptime NAME = <runtime value>` is now rejected by the compile-time
    // elaborator (the comptime validator), not the checker directly.
    let program = mojo_lite::parse("var x: Int = 1\ncomptime N = x\n").unwrap();
    assert!(mojo_lite::elaborate(program).is_err());
}

#[test]
fn rejects_explicit_params_on_non_generic_function() {
    let e = err("def f(x: Int) -> Int:\n    return x\n\nvar y: Int = f[Int](1)\n");
    assert!(
        matches!(e, TypeError::WrongTypeArgCount { expected: 0, .. }),
        "got {:?}",
        e
    );
}

// --- SIMD ---

#[test]
fn accepts_simd_construction_ops_and_index() {
    ok(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar w: SIMD[DType.int32, 4] = v + v * v\nvar lane: Int32 = w[0]\n",
    );
}

#[test]
fn accepts_simd_splat_construction() {
    ok("var v: SIMD[DType.uint8, 8] = SIMD[DType.uint8, 8](7)\n");
}

#[test]
fn accepts_simd_comparison_mask() {
    ok(
        "var v: SIMD[DType.float32, 4] = SIMD[DType.float32, 4](1.0, 2.0, 3.0, 4.0)\nvar m: SIMD[DType.bool, 4] = v < v\n",
    );
}

#[test]
fn accepts_literal_splat_in_simd_operator() {
    ok(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar w: SIMD[DType.int32, 4] = v + 100\n",
    );
}

#[test]
fn accepts_scalar_alias_types_and_construction() {
    ok("var a: Int8 = Int8(5)\nvar b: UInt32 = UInt32(9)\nvar c: Float32 = Float32(1.5)\n");
}

#[test]
fn accepts_comptime_simd_width() {
    ok("comptime W = 2 * 2\nvar v: SIMD[DType.int32, W] = SIMD[DType.int32, W](1, 2, 3, 4)\n");
}

#[test]
fn rejects_non_power_of_two_width() {
    let e = err("var v: SIMD[DType.int32, 3] = SIMD[DType.int32, 3](1, 2, 3)\n");
    assert!(matches!(e, TypeError::BadSimdWidth(_)), "got {:?}", e);
}

#[test]
fn rejects_unknown_dtype() {
    let e = err("var v: SIMD[DType.foo, 4] = SIMD[DType.foo, 4](1)\n");
    assert!(matches!(e, TypeError::BadDtype(_)), "got {:?}", e);
}

#[test]
fn rejects_wrong_simd_element_count() {
    let e = err("var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2)\n");
    assert!(
        matches!(e, TypeError::SimdArity { width: 4, got: 2 }),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_mixed_dtype_operands() {
    let e = err(
        "var a: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar b: SIMD[DType.int64, 4] = SIMD[DType.int64, 4](1, 2, 3, 4)\nvar c: SIMD[DType.int32, 4] = a + b\n",
    );
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_division_on_integer_simd() {
    let e = err(
        "var a: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar b: SIMD[DType.int32, 4] = a / a\n",
    );
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_indexing_a_non_simd() {
    let e = err("var x: Int = 5\nvar y: Int = x[0]\n");
    assert!(matches!(e, TypeError::NotIndexable(_)), "got {:?}", e);
}

#[test]
fn rejects_float_literal_for_integer_dtype() {
    let e = err("var v: SIMD[DType.int32, 2] = SIMD[DType.int32, 2](1.5, 2.5)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn distinct_simd_widths_are_distinct_types() {
    let e = err("var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 2](1, 2)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

// --- Path-sensitive return checking ---

#[test]
fn accepts_return_on_all_if_paths() {
    // Every arm (including else) returns.
    ok(
        "def sign(n: Int) -> Int:\n    if n > 0:\n        return 1\n    elif n < 0:\n        return -1\n    else:\n        return 0\n",
    );
}

#[test]
fn accepts_trailing_return_after_a_loop() {
    ok("def f(n: Int) -> Int:\n    for i in range(n):\n        return i\n    return -1\n");
}

#[test]
fn accepts_no_return_when_return_type_is_none() {
    // A function without a declared return type may fall off the end (yields None).
    ok("def noop(x: Int):\n    pass\n");
}

#[test]
fn rejects_falling_off_the_end() {
    let e = err("def f() -> Int:\n    var x: Int = 1\n");
    assert_eq!(e, TypeError::MissingReturn("f".into()));
}

#[test]
fn rejects_if_without_else() {
    // No `else`, so a path falls through without returning.
    let e = err("def f(n: Int) -> Int:\n    if n > 0:\n        return 1\n");
    assert_eq!(e, TypeError::MissingReturn("f".into()));
}

#[test]
fn rejects_return_only_inside_a_loop() {
    // Conservative: a loop may run zero times, so it doesn't guarantee a return.
    let e = err("def f(n: Int) -> Int:\n    for i in range(n):\n        return i\n");
    assert_eq!(e, TypeError::MissingReturn("f".into()));
}

#[test]
fn rejects_method_missing_return() {
    let e = err(
        "@fieldwise_init\nstruct P:\n    var x: Int\n\n    def get(self) -> Int:\n        pass\n",
    );
    assert_eq!(e, TypeError::MissingReturn("get".into()));
}

// --- Exceptions ---

#[test]
fn accepts_raise_and_try_except() {
    ok("try:\n    raise Error(\"boom\")\nexcept e:\n    var x: Int = 1\n");
}

#[test]
fn accepts_string_shorthand_raise() {
    ok("raise \"oops\"\n");
}

#[test]
fn accepts_try_else_finally_and_bound_error() {
    // `except e:` binds `e: Error`, usable (e.g. re-raised).
    ok("try:\n    raise \"x\"\nexcept e:\n    raise e^\nelse:\n    pass\nfinally:\n    pass\n");
}

#[test]
fn accepts_raises_effect_but_does_not_enforce_it() {
    // A `raises` function that raises, called without a surrounding try — the
    // effect is deliberately not analyzed.
    ok("def boom() raises -> Int:\n    raise \"deep\"\n    return 0\n\nvar y: Int = boom()\n");
}

#[test]
fn accepts_error_typed_variable() {
    ok("var e: Error = Error(\"msg\")\n");
}

#[test]
fn accepts_transfer_sigil_as_identity() {
    ok("var x: Int = 1\nvar y: Int = x^\n");
}

#[test]
fn rejects_raising_a_non_error() {
    let e = err("raise 5\n");
    assert!(
        matches!(&e, TypeError::TypeMismatch { context, .. } if context == "raise"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_error_constructor_with_non_string() {
    let e = err("var e: Error = Error(5)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn except_binding_is_scoped_to_its_clause() {
    // `e` from `except e:` does not leak past the try statement.
    let e = err("try:\n    raise \"x\"\nexcept e:\n    pass\nvar leaked: Error = e\n");
    assert_eq!(e, TypeError::UndefinedVariable("e".into()));
}

// --- Imports (parsed, not resolved) ---

#[test]
fn import_statements_are_accepted_as_no_ops() {
    // Imports type-check (they are no-ops); real code alongside still runs.
    ok("import mypackage.mymodule as mm\nfrom other import a, b as c\nvar x: Int = 1 + 2\n");
}

#[test]
fn imported_names_are_not_made_available() {
    // Since imports are not resolved, an imported name is still undefined.
    let e = err("from mymodule import foo\nvar x: Int = foo\n");
    assert_eq!(e, TypeError::UndefinedVariable("foo".into()));
}

// --- print ---

#[test]
fn accepts_print_of_various_values() {
    ok(
        "@fieldwise_init\nstruct P:\n    var x: Int\n\nprint(1, \"a\", True, 3.5, None, P(5))\nprint()\n",
    );
}

#[test]
fn rejects_printing_a_function() {
    let e = err("def f() -> Int:\n    return 1\n\nprint(f)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

// --- Builtins: String / abs / min / max / round / len ---

#[test]
fn accepts_string_and_numeric_builtins() {
    ok(
        "var s: String = \"n=\" + String(42)\nvar a: Int = abs(-7)\nvar f: Float64 = abs(-2.5)\nvar lo: Int = min(3, 8)\nvar hi: Float64 = max(1.0, 2.0)\nvar r: Float64 = round(3.7)\nvar n: Int = len(\"hello\")\n",
    );
}

#[test]
fn abs_preserves_the_numeric_type() {
    // abs of a Float64 stays Float64 (would fail to bind to Int if it returned Int).
    ok("var x: Float64 = 1.5\nvar y: Float64 = abs(x)\n");
    let e = err("var x: Float64 = 1.5\nvar y: Int = abs(x)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_stringify_of_non_stringable() {
    let e = err("def f() -> Int:\n    return 1\n\nvar s: String = String(f)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_len_of_non_string() {
    let e = err("var n: Int = len(5)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_min_mixing_concrete_types() {
    let e = err("var i: Int = 1\nvar u: UInt = UInt(2)\nvar m: Int = min(i, u)\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_round_of_an_int() {
    let e = err("var r: Float64 = round(5)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_builtin_arity_mismatch() {
    assert!(matches!(
        err("var a: Int = abs(1, 2)\n"),
        TypeError::ArityMismatch { .. }
    ));
    assert!(matches!(
        err("var a: Int = min(1)\n"),
        TypeError::ArityMismatch { .. }
    ));
}

// --- List (Step 1: construction / read / iterate / len) ---

#[test]
fn accepts_list_construction_forms() {
    ok(
        "var a: List[Int] = [1, 2, 3]\nvar b: List[Int] = List[Int](1, 2)\nvar c: List[Int] = List(4, 5)\nvar d: List[Int] = List[Int]()\n",
    );
}

#[test]
fn infers_list_element_type_with_widening() {
    ok("var xs: List[Float64] = [1, 2.0, 3]\nvar ys: List[String] = [\"a\", \"b\"]\n");
}

#[test]
fn accepts_len_index_and_iteration() {
    ok(
        "var xs: List[Int] = [10, 20, 30]\nvar n: Int = len(xs)\nvar first: Int = xs[0]\nvar sum: Int = 0\nfor x in xs:\n    sum = sum + x\n",
    );
}

#[test]
fn accepts_nested_and_struct_lists() {
    ok("var m: List[List[Int]] = [[1, 2], [3, 4]]\nvar v: Int = m[1][0]\n");
    ok(
        "@fieldwise_init\nstruct P:\n    var x: Int\n\nvar ps: List[P] = [P(1), P(2)]\nvar fx: Int = ps[0].x\n",
    );
}

#[test]
fn rejects_heterogeneous_list_literal() {
    let e = err("var xs: List[Int] = [1, \"a\"]\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_wrong_list_element_type() {
    let e = err("var xs: List[Int] = List[Int](1, True)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_empty_inferred_list() {
    let e = err("var xs: List[Int] = List()\n");
    assert!(
        matches!(e, TypeError::CannotInferTypeParam { .. }),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_non_int_list_index() {
    let e = err("var xs: List[Int] = [1, 2]\nvar y: Int = xs[True]\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

// --- List (Steps 2 & 3: index-assign, append, pop) ---

#[test]
fn accepts_list_mutation() {
    ok(
        "var xs: List[Int] = List[Int]()\nxs.append(1)\nxs.append(2)\nxs[0] = 10\nvar last: Int = xs.pop()\n",
    );
}

#[test]
fn rejects_index_assign_wrong_type() {
    let e = err("var xs: List[Int] = [1]\nxs[0] = True\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_append_wrong_type() {
    let e = err("var xs: List[Int] = [1]\nxs.append(True)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_mutating_a_non_variable_list() {
    let e = err("List[Int](1, 2).append(3)\n");
    assert_eq!(e, TypeError::MutationRequiresVariable("append".into()));
}

#[test]
fn accepts_simd_lane_write() {
    // A SIMD lane write `v[i] = e` takes a splatting literal or a same-dtype scalar.
    ok(
        "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nv[0] = 9\nv[1] = Int32(5)\nv[2] += 100\n",
    );
    // ...but not a value of the wrong element kind, nor a whole vector.
    assert!(matches!(
        err("var v: SIMD[DType.int32, 2] = SIMD[DType.int32, 2](1, 2)\nv[0] = True\n"),
        TypeError::TypeMismatch { .. }
    ));
    assert!(matches!(
        err(
            "var v: SIMD[DType.int32, 2] = SIMD[DType.int32, 2](1, 2)\nv[0] = SIMD[DType.int32, 2](1, 2)\n"
        ),
        TypeError::TypeMismatch { .. }
    ));
}

#[test]
fn rejects_unknown_list_method() {
    let e = err("var xs: List[Int] = [1]\nxs.frobnicate()\n");
    assert!(matches!(e, TypeError::NoSuchMethod { .. }), "got {:?}", e);
}

#[test]
fn pop_returns_the_element_type() {
    ok("var xs: List[String] = [\"a\", \"b\"]\nvar s: String = xs.pop()\n");
}

// --- List (more methods: insert/remove/pop(i)/clear/reverse/extend/count/index) ---

#[test]
fn accepts_new_list_methods() {
    ok(
        "var xs: List[Int] = [1, 2, 3]\nxs.insert(1, 99)\nxs.remove(2)\nvar m: Int = xs.pop(0)\nxs.reverse()\nvar b: List[Int] = [4, 5]\nxs.extend(b)\nvar c: Int = xs.count(99)\nvar i: Int = xs.index(4)\nxs.clear()\n",
    );
}

#[test]
fn count_and_index_work_on_a_temporary() {
    ok("var c: Int = [1, 1, 2].count(1)\nvar i: Int = [3, 4, 5].index(4)\n");
}

#[test]
fn rejects_remove_on_non_equatable_elements() {
    let e = err(
        "@fieldwise_init\nstruct P:\n    var x: Int\n\nvar ps: List[P] = [P(1)]\nps.remove(P(1))\n",
    );
    assert!(
        matches!(&e, TypeError::TypeMismatch { context, .. } if context == "'remove'"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_extend_with_wrong_element_type() {
    let e = err("var a: List[Int] = [1]\nvar b: List[String] = [\"x\"]\na.extend(b)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_non_int_insert_index() {
    let e = err("var xs: List[Int] = [1]\nxs.insert(True, 2)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_count_on_a_non_variable_when_temporary_is_not_equatable() {
    // count needs equatable elements even for a temporary.
    let e = err("@fieldwise_init\nstruct P:\n    var x: Int\n\nvar c: Int = [P(1)].count(P(1))\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

// --- Membership: in / not in ---

#[test]
fn accepts_membership_on_list_and_string() {
    ok(
        "var xs: List[Int] = [1, 2, 3]\nvar a: Bool = 2 in xs\nvar b: Bool = 5 not in xs\nvar s: String = \"hello\"\nvar c: Bool = \"ell\" in s\n",
    );
}

#[test]
fn membership_returns_bool() {
    let e = err("var xs: List[Int] = [1]\nvar n: Int = 1 in xs\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_membership_on_non_container() {
    let e = err("var a: Bool = 1 in 5\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_membership_element_type_mismatch() {
    let e = err("var xs: List[Int] = [1, 2]\nvar a: Bool = True in xs\n");
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

#[test]
fn rejects_membership_on_non_equatable_list() {
    let e = err(
        "@fieldwise_init\nstruct P:\n    var x: Int\n\nvar ps: List[P] = [P(1)]\nvar a: Bool = P(1) in ps\n",
    );
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

// --- Member-write: place assignment + mut self ---

const MUTPT: &str = "@fieldwise_init\nstruct Point:\n    var x: Int\n    var y: Int\n\n";

#[test]
fn accepts_field_and_nested_place_writes() {
    ok(&format!(
        "{MUTPT}var p: Point = Point(1, 2)\np.x = 10\np.y = p.x + 1\n"
    ));
    ok(
        "@fieldwise_init\nstruct In:\n    var v: Int\n\n@fieldwise_init\nstruct Out:\n    var inner: In\n\nvar o: Out = Out(In(5))\no.inner.v = 100\n",
    );
    ok(&format!(
        "{MUTPT}var ps: List[Point] = [Point(1, 1)]\nps[0].x = 9\n"
    ));
}

#[test]
fn accepts_mut_self_method_and_mutation() {
    ok(
        "@fieldwise_init\nstruct Counter:\n    var n: Int\n\n    def inc(mut self):\n        self.n = self.n + 1\n\nvar c: Counter = Counter(0)\nc.inc()\n",
    );
}

#[test]
fn accepts_list_method_through_a_field() {
    ok(
        "@fieldwise_init\nstruct Bag:\n    var items: List[Int]\n\nvar b: Bag = Bag([1])\nb.items.append(2)\nb.items[0] = 9\n",
    );
}

#[test]
fn rejects_field_write_on_read_only_self() {
    let e = err(
        "@fieldwise_init\nstruct S:\n    var x: Int\n\n    def bad(self):\n        self.x = 9\n",
    );
    assert_eq!(e, TypeError::ImmutableSelf);
}

#[test]
fn rejects_mut_self_call_on_a_temporary() {
    let e = err(
        "@fieldwise_init\nstruct C:\n    var n: Int\n\n    def inc(mut self):\n        self.n = self.n + 1\n\nC(0).inc()\n",
    );
    assert!(
        matches!(e, TypeError::InvalidAssignTarget(_)),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_unknown_field_write() {
    let e = err(&format!("{MUTPT}var p: Point = Point(1, 2)\np.z = 3\n"));
    assert!(matches!(e, TypeError::NoSuchField { .. }), "got {:?}", e);
}

#[test]
fn rejects_wrong_type_field_write() {
    let e = err(&format!("{MUTPT}var p: Point = Point(1, 2)\np.x = True\n"));
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_write_to_a_call_result() {
    let e = err(&format!(
        "{MUTPT}def mk() -> Point:\n    return Point(0, 0)\n\nmk().x = 5\n"
    ));
    assert!(
        matches!(e, TypeError::InvalidAssignTarget(_)),
        "got {:?}",
        e
    );
}

// --- Augmented assignment ---

#[test]
fn accepts_augmented_assignment_forms() {
    ok("var i: Int = 0\ni += 1\ni -= 1\ni *= 2\ni //= 2\ni %= 3\ni **= 2\n");
    ok("var s: String = \"a\"\ns += \"b\"\n");
    ok(
        "@fieldwise_init\nstruct C:\n    var n: Int\n\n    def bump(mut self):\n        self.n += 1\n\nvar c: C = C(0)\nc.n += 5\n",
    );
    ok("var xs: List[Int] = [1, 2]\nxs[0] += 10\n");
}

#[test]
fn rejects_augmented_assignment_that_changes_type() {
    // `/` yields Float64, which does not fit an Int target.
    let e = err("var i: Int = 10\ni /= 2\n");
    assert!(
        matches!(&e, TypeError::TypeMismatch { context, .. } if context == "augmented assignment"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_augmented_assignment_bad_operands() {
    assert!(matches!(
        err("var i: Int = 1\ni += True\n"),
        TypeError::BadOperator { .. }
    ));
    assert_eq!(err("x += 1\n"), TypeError::UndefinedVariable("x".into()));
}

#[test]
fn rejects_augmented_assignment_on_read_only_self() {
    let e = err(
        "@fieldwise_init\nstruct S:\n    var x: Int\n\n    def bad(self):\n        self.x += 1\n",
    );
    assert_eq!(e, TypeError::ImmutableSelf);
}

// --- Float64 / SIMD unification ---

#[test]
fn float64_and_simd_float64_1_are_the_same_type() {
    // Interchangeable both ways, since Float64 == SIMD[DType.float64, 1].
    ok("var a: Float64 = 3.0\nvar b: SIMD[DType.float64, 1] = a\nvar c: Float64 = b\n");
}

#[test]
fn accepts_float64_vectors_and_lane_ops() {
    ok(
        "var v: SIMD[DType.float64, 4] = SIMD[DType.float64, 4](1.0, 2.0, 3.0, 4.0)\nvar d: SIMD[DType.float64, 4] = v + v\nvar q: SIMD[DType.float64, 4] = v / SIMD[DType.float64, 4](2.0)\nvar lane: Float64 = v[2]\nv[0] = 9.0\nv[1] += 1.0\nvar m: SIMD[DType.bool, 4] = v < d\n",
    );
}

#[test]
fn float64_scalar_splats_into_a_float64_vector() {
    ok(
        "var a: Float64 = 2.0\nvar v: SIMD[DType.float64, 2] = SIMD[DType.float64, 2](1.0, 2.0)\nvar w: SIMD[DType.float64, 2] = v + a\nv[0] = a\n",
    );
}

#[test]
fn rejects_float64_vector_dtype_mismatch() {
    // A float32 scalar does not splat into a float64 vector (distinct dtypes).
    let e = err(
        "var v: SIMD[DType.float64, 2] = SIMD[DType.float64, 2](1.0, 2.0)\nvar w: SIMD[DType.float64, 2] = v + Float32(1.0)\n",
    );
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {:?}", e);
}

// --- Inferred `var` (type from the initializer) ---

#[test]
fn accepts_inferred_var_and_uses_its_type() {
    // The inferred type flows to later uses (Int arithmetic, String concat, List).
    ok("var n = 40\nvar m: Int = n + 2\n");
    ok("var s = \"hi\"\nvar t: String = s + \"!\"\n");
    ok("var xs = [1, 2, 3]\nxs.append(4)\nvar k: Int = xs[0]\n");
    ok("var f = 3.5\nvar g: Float64 = f * 2.0\n");
}

#[test]
fn inferred_var_rejects_wrong_later_use() {
    // `n` is inferred `Int`, so using it as a `Bool` condition is a type error.
    let e = err("var n = 5\nif n:\n    pass\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_inferred_var_of_a_closure() {
    let e = err("def outer():\n    def inner() -> Int:\n        return 1\n    var f = inner\n");
    assert_eq!(e, TypeError::ClosureEscape);
}

#[test]
fn rejects_inferred_var_of_range() {
    let e = err("var r = range(5)\n");
    assert!(
        matches!(&e, TypeError::TypeMismatch { found, .. } if found == "range"),
        "got {:?}",
        e
    );
}

// --- Tuples ---

#[test]
fn accepts_tuple_annotated_inferred_and_indexed() {
    ok(
        "var t: Tuple[Int, Float64, String] = (1, 2.5, \"hi\")\nvar a: Int = t[0]\nvar b: Float64 = t[1]\nvar c: String = t[2]\n",
    );
    ok("var u = (10, 20)\nvar first: Int = u[0]\n");
    ok(
        "def stats() -> Tuple[Int, Int]:\n    return (512, 4)\n\nvar s = stats()\nvar p: Int = s[0]\n",
    );
}

#[test]
fn tuple_element_coercion_materializes_literals() {
    // `(1, 2)` fits `Tuple[Float64, Float64]` element-wise.
    ok("var t: Tuple[Float64, Float64] = (1, 2)\n");
}

#[test]
fn rejects_tuple_wrong_element_type() {
    let e = err("var t: Tuple[Int, Int] = (1, True)\n");
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {:?}", e);
}

#[test]
fn rejects_runtime_tuple_index() {
    let e = err("var t: Tuple[Int, String] = (1, \"x\")\nvar i: Int = 0\nvar y = t[i]\n");
    assert!(
        matches!(&e, TypeError::TypeMismatch { context, .. } if context == "tuple index"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_out_of_range_tuple_index() {
    let e = err("var t: Tuple[Int, Int] = (1, 2)\nvar y = t[5]\n");
    assert!(
        matches!(&e, TypeError::TypeMismatch { context, .. } if context == "tuple index"),
        "got {:?}",
        e
    );
}

#[test]
fn rejects_tuple_element_write() {
    // Tuples are immutable — no element assignment.
    let e = err("var t: Tuple[Int, Int] = (1, 2)\nt[0] = 9\n");
    assert!(matches!(e, TypeError::NotIndexable(_)), "got {:?}", e);
}

// --- Function-argument forms: parsed, flagged unsupported by the checker ---

#[test]
fn flags_advanced_parameter_forms_as_unsupported() {
    for src in [
        "def f(**opts: Int):\n    pass\n",                   // **kwargs
        "def f(a: Int, /) -> Int:\n    return a\n",          // positional-only marker
        "def f(a: Int, *, d: Bool) -> Int:\n    return a\n", // keyword-only marker
        "def f(out x: Int):\n    pass\n",                    // `out` convention (still deferred)
        "def f(*a: Int, x: Int) -> Int:\n    return x\n",    // param after *args (keyword-only)
    ] {
        assert!(
            matches!(err(src), TypeError::Unsupported(_)),
            "expected Unsupported for: {src}"
        );
    }
}

#[test]
fn accepts_variadic_args() {
    // A `*args` parameter is a `List[T]` in the body; a call collects extras.
    ok(
        "def sum(*values: Int) -> Int:\n    var t: Int = 0\n    for v in values:\n        t = t + v\n    return t\n\nvar a: Int = sum()\nvar b: Int = sum(1, 2, 3)\n",
    );
    // Regular params before the variadic.
    ok(
        "def tag(label: String, *nums: Int) -> Int:\n    return len(nums)\n\nvar a: Int = tag(\"x\", 1, 2)\n",
    );
    // Each overflow argument must match the element type.
    assert!(matches!(
        err("def sum(*values: Int) -> Int:\n    return 0\n\nvar z: Int = sum(1, \"x\")\n"),
        TypeError::TypeMismatch { .. }
    ));
    // A required regular parameter is still enforced.
    assert!(matches!(
        err("def tag(label: String, *nums: Int) -> Int:\n    return 0\n\nvar z: Int = tag()\n"),
        TypeError::BadCall { .. }
    ));
    // `*args` on a generic function is deferred.
    assert!(matches!(
        err("def f[T: AnyType](*a: Int) -> Int:\n    return 0\n"),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn accepts_default_argument_values() {
    // A default lets a call omit the trailing argument.
    ok(
        "def my_pow(base: Int, exp: Int = 2) -> Int:\n    return base ** exp\n\nvar a: Int = my_pow(3)\nvar b: Int = my_pow(3, 3)\n",
    );
}

#[test]
fn rejects_bad_default_values_and_arity() {
    // Default value must fit the parameter type.
    assert!(matches!(
        err("def f(a: Int, b: Int = \"x\") -> Int:\n    return a\n"),
        TypeError::TypeMismatch { .. }
    ));
    // A required parameter cannot follow a defaulted one.
    assert!(matches!(
        err("def f(a: Int = 1, b: Int) -> Int:\n    return a\n"),
        TypeError::Unsupported(_)
    ));
    // Fewer than the required count leaves a required argument unbound (BadCall);
    // more than the total is a too-many-positional arity error.
    assert!(matches!(
        err("def f(a: Int, b: Int = 2) -> Int:\n    return a\n\nvar z: Int = f()\n"),
        TypeError::BadCall { .. }
    ));
    assert!(matches!(
        err("def f(a: Int, b: Int = 2) -> Int:\n    return a\n\nvar z: Int = f(1, 2, 3)\n"),
        TypeError::ArityMismatch { .. }
    ));
    // Defaults on generic functions are still deferred.
    assert!(matches!(
        err("def f[T: AnyType](a: T, b: Int = 2) -> T:\n    return a\n"),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn accepts_keyword_arguments_and_reports_mismatches() {
    // Keyword args to a free function type-check (any order, mixed with positional).
    ok(
        "def f(a: Int, b: Int) -> Int:\n    return a - b\n\nvar z: Int = f(b=1, a=2)\nvar w: Int = f(2, b=1)\n",
    );
    // Unknown keyword, an argument bound twice, or a missing required argument.
    for src in [
        "def f(a: Int) -> Int:\n    return a\n\nvar z: Int = f(x=1)\n",
        "def f(a: Int, b: Int) -> Int:\n    return a\n\nvar z: Int = f(1, a=2)\n",
        "def f(a: Int, b: Int) -> Int:\n    return a\n\nvar z: Int = f(b=2)\n",
    ] {
        assert!(
            matches!(err(src), TypeError::BadCall { .. }),
            "expected BadCall for: {src}"
        );
    }
    // A keyword-argument type mismatch is still a TypeMismatch.
    assert!(matches!(
        err("def f(a: Int) -> Int:\n    return a\n\nvar z: Int = f(a=\"x\")\n"),
        TypeError::TypeMismatch { .. }
    ));
    // Keyword args to a built-in are rejected.
    assert!(matches!(
        err("def main():\n    print(len(x=\"hi\"))\n"),
        TypeError::BadCall { .. }
    ));
    // Keyword args to a method are still deferred.
    assert!(matches!(
        err(
            "@fieldwise_init\nstruct C:\n    var n: Int\n\n    def g(self, k: Int) -> Int:\n        return k\n\nvar z: Int = C(1).g(k=2)\n"
        ),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn flags_advanced_forms_on_methods_and_traits() {
    assert!(matches!(
        err(
            "@fieldwise_init\nstruct C:\n    var n: Int\n\n    def f(self, k: Int = 1):\n        pass\n"
        ),
        TypeError::Unsupported(_)
    ));
    assert!(matches!(
        err("trait T:\n    def m(self, *args: Int) -> Int:\n        ...\n"),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn plain_signatures_are_unaffected() {
    ok("def add(a: Int, b: Int) -> Int:\n    return a + b\n\nvar z: Int = add(1, 2)\n");
    // A parameter literally named after a convention word still works.
    ok("def g(read: Int) -> Int:\n    return read\n\nvar z: Int = g(5)\n");
}

// --- Expression syntax parsed but semantics deferred (syntax-first phase) ---

#[test]
fn flags_deferred_expression_syntax_as_unsupported() {
    // t-strings and the walrus `:=` still parse but defer their semantics.
    assert!(matches!(
        err("var s: String = t\"x{1}\"\n"),
        TypeError::Unsupported(_)
    ));
    // Ternary, chained comparison, and slices are now implemented (were deferred).
    ok("var m: Int = 1 if True else 2\nprint(m)\n");
    ok("var t: Bool = 0 < 1 < 2\nprint(t)\n");
    ok("var xs: List[Int] = [1, 2, 3]\nvar ys: List[Int] = xs[0:2]\nprint(ys)\n");
    // A lone comparison is unaffected (still type-checks).
    ok("var b: Bool = 1 < 2\n");
}

#[test]
fn ternary_chained_comparison_and_unpacking_type_check() {
    // Ternary branches must unify; the condition must be Bool.
    ok("def f(n: Int) -> Int:\n    return 1 if n > 0 else -1\n");
    assert!(matches!(
        err("var m: Int = 1 if 5 else 2\n"),
        TypeError::TypeMismatch { .. }
    ));
    // Chained comparison → Bool.
    ok("def g(i: Int, n: Int) -> Bool:\n    return 0 <= i < n\n");
    // Tuple unpacking: arity + element types.
    ok("var t: Tuple[Int, String] = (1, \"a\")\nvar x: Int = 0\nvar s: String = \"\"\nx, s = t\n");
    assert!(matches!(
        err(
            "var t: Tuple[Int, Int] = (1, 2)\nvar a: Int = 0\nvar b: Int = 0\nvar c: Int = 0\na, b, c = t\n"
        ),
        TypeError::TypeMismatch { .. }
    ));
}

// --- Decorators + dunder / receiver conventions (parse-only; semantics deferred) ---

#[test]
fn accepts_decorators_and_plain_dunders() {
    // Unmodeled decorators on a def/struct are ignored by the checker.
    ok("@always_inline\ndef f(x: Int) -> Int:\n    return x\n\nvar z: Int = f(1)\n");
    ok("@value\n@fieldwise_init\nstruct P:\n    var x: Int\n\nvar p: P = P(1)\n");
    // A dunder method with a plain `self` type-checks like any method.
    ok(
        "@fieldwise_init\nstruct V:\n    var x: Int\n\n    def __eq__(self, o: V) -> Bool:\n        return self.x == o.x\n",
    );
}

#[test]
fn flags_out_self_and_static_methods_as_unsupported() {
    // `out self` on a non-__init__ method is still unsupported (only `__init__`
    // may initialize the receiver).
    assert!(matches!(
        err("struct W:\n    var x: Int\n\n    def reset(out self):\n        self.x = 0\n"),
        TypeError::Unsupported(_)
    ));
    // A @staticmethod (no self).
    assert!(matches!(
        err(
            "struct S:\n    var n: Int\n\n    @staticmethod\n    def make(x: Int) -> Int:\n        return x\n"
        ),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn hand_written_init_out_self() {
    // `def __init__(out self, …)` constructs a struct without `@fieldwise_init`.
    ok(
        "struct P:\n    var x: Int\n    var y: Int\n    def __init__(out self, x: Int, y: Int):\n        self.x = x\n        self.y = y\n\ndef main():\n    var p: P = P(1, 2)\n    print(p.x)\n",
    );
    // Definite initialization: every field must be assigned in the body.
    assert!(matches!(
        err(
            "struct P:\n    var x: Int\n    var y: Int\n    def __init__(out self, x: Int):\n        self.x = x\n"
        ),
        TypeError::UninitializedField { .. }
    ));
    // A struct cannot have both `@fieldwise_init` and a hand-written `__init__`.
    assert!(matches!(
        err(
            "@fieldwise_init\nstruct W:\n    var x: Int\n    def __init__(out self, x: Int):\n        self.x = x\n"
        ),
        TypeError::ConflictingConstructor(_)
    ));
    // Neither a constructor nor `@fieldwise_init` → no constructor.
    assert!(matches!(
        err(
            "struct P:\n    var x: Int\n    def get(self) -> Int:\n        return self.x\n\ndef main():\n    var p: P = P(1)\n"
        ),
        TypeError::NoConstructor(_)
    ));
}

#[test]
fn flags_function_type_annotations_as_unsupported() {
    // A function-typed variable or parameter parses but is not modeled yet.
    assert!(matches!(
        err("def g(x: Int) -> Int:\n    return x\n\nvar f: def(Int) -> Int = g\n"),
        TypeError::Unsupported(_)
    ));
    assert!(matches!(
        err("def apply(cb: def(Int) -> Int, x: Int) -> Int:\n    return x\n"),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn accepts_numeric_bases_and_string_forms_but_flags_tstrings() {
    // Based integers, digit separators, and single/triple-quoted strings are fully
    // supported (they are ordinary Int/String values).
    ok(
        "var a: Int = 0xFF\nvar b: Int = 1_000_000\nvar c: String = 'single'\nvar d: String = \"\"\"triple\"\"\"\n",
    );
    // A t-string parses but its semantics are deferred.
    assert!(matches!(
        err("var s: String = t\"x={y}\"\n"),
        TypeError::Unsupported(_)
    ));
}

#[test]
fn owned_self_and_owned_params_are_accepted() {
    // `owned self` (Mojo's `__del__` receiver) and `owned`/`read` parameter
    // conventions bind by value and now type-check (their ownership meaning is
    // handled by the ownership analysis / ASAP drops).
    assert!(
        check_source("@fieldwise_init\nstruct R:\n    var x: Int\n    def __del__(owned self):\n        print(self.x)\n").is_ok()
    );
    assert!(check_source("def f(owned a: Int, read b: Int) -> Int:\n    return a + b\n").is_ok());
}

#[test]
fn out_and_ref_self_conventions_still_rejected() {
    // `mut`/`ref` params are now modeled (write-back); `out` params and `out self`/
    // `ref self` receivers still need init/reference semantics we don't model.
    assert!(matches!(
        check_source("def f(out a: Int):\n    pass\n"),
        Err(TypeError::Unsupported(_))
    ));
    assert!(matches!(
        check_source(
            "@fieldwise_init\nstruct R:\n    var x: Int\n    def m(ref self):\n        pass\n"
        ),
        Err(TypeError::Unsupported(_))
    ));
}

#[test]
fn structs_are_non_copyable_by_default() {
    // Move-only: copying a struct value (binding it to a new variable) is rejected;
    // a `^` transfer, or making the struct `Copyable`, is fine. Scalars copy freely.
    let nc = "@fieldwise_init\nstruct T:\n    var x: Int\n\ndef main():\n    var a: T = T(1)\n    var b: T = a\n    print(b.x)\n";
    assert!(matches!(
        check_source(nc),
        Err(TypeError::NonCopyable { .. })
    ));

    let moved = "@fieldwise_init\nstruct T:\n    var x: Int\n\ndef main():\n    var a: T = T(1)\n    var b: T = a^\n    print(b.x)\n";
    assert!(check_source(moved).is_ok());

    let copyable = "@fieldwise_init\nstruct T(Copyable):\n    var x: Int\n\ndef main():\n    var a: T = T(1)\n    var b: T = a\n    print(b.x)\n";
    assert!(check_source(copyable).is_ok());

    // Scalars are Copyable.
    assert!(
        check_source("def main():\n    var a: Int = 1\n    var b: Int = a\n    print(a + b)\n")
            .is_ok()
    );
}

#[test]
fn owned_arg_consumes_but_read_and_mut_borrow() {
    let common = "@fieldwise_init\nstruct T:\n    var x: Int\n\n";
    // `owned` consumes → copying a non-Copyable value into it is rejected.
    let owned = format!(
        "{common}def take(owned t: T) -> Int:\n    return t.x\n\ndef main():\n    var a: T = T(1)\n    print(take(a))\n"
    );
    assert!(matches!(
        check_source(&owned),
        Err(TypeError::NonCopyable { .. })
    ));
    // `owned` with `^` is a move → fine.
    let moved = format!(
        "{common}def take(owned t: T) -> Int:\n    return t.x\n\ndef main():\n    var a: T = T(1)\n    print(take(a^))\n"
    );
    assert!(check_source(&moved).is_ok());
    // `read` (default) and `mut` borrow → no copy, fine.
    let read = format!(
        "{common}def peek(t: T) -> Int:\n    return t.x\n\ndef main():\n    var a: T = T(1)\n    print(peek(a))\n"
    );
    assert!(check_source(&read).is_ok());
    let mutp = format!(
        "{common}def bump(mut t: T):\n    t.x = t.x + 1\n\ndef main():\n    var a: T = T(1)\n    bump(a)\n    print(a.x)\n"
    );
    assert!(check_source(&mutp).is_ok());
}

#[test]
fn borrow_check_rejects_mutable_aliasing() {
    // Mutable-XOR-shared, root-sensitive: a `mut` borrow of a variable must be
    // exclusive for the call.
    let two_mut = "def f(mut a: Int, mut b: Int):\n    a = b\n\ndef main():\n    var x: Int = 5\n    f(x, x)\n";
    assert!(matches!(
        check_source(two_mut),
        Err(TypeError::AliasingViolation { .. })
    ));

    let mut_and_shared = "def f(mut a: Int, b: Int):\n    a = a + b\n\ndef main():\n    var x: Int = 5\n    f(x, x)\n";
    assert!(matches!(
        check_source(mut_and_shared),
        Err(TypeError::AliasingViolation { .. })
    ));

    // Distinct variables, or two shared borrows, are fine.
    let distinct = "def f(mut a: Int, mut b: Int):\n    a = b\n\ndef main():\n    var x: Int = 5\n    var y: Int = 6\n    f(x, y)\n";
    assert!(check_source(distinct).is_ok());
    let two_shared = "def f(a: Int, b: Int) -> Int:\n    return a + b\n\ndef main():\n    var x: Int = 5\n    print(f(x, x))\n";
    assert!(check_source(two_shared).is_ok());
}

#[test]
fn borrow_check_rejects_move_while_borrowed() {
    // Moving a variable (`^`) while it is also borrowed in the same call is a
    // conflict (can't move an aliased value).
    let common = "@fieldwise_init\nstruct T:\n    var x: Int\n\n";
    let mut_and_move = format!(
        "{common}def f(mut a: T, owned b: T):\n    a.x = b.x\n\ndef main():\n    var p: T = T(1)\n    f(p, p^)\n"
    );
    assert!(matches!(
        check_source(&mut_and_move),
        Err(TypeError::AliasingViolation { .. })
    ));
}

#[test]
fn deinit_self_is_the_current_destructor_convention() {
    // Current Mojo spells the destructor receiver `deinit self`; the older
    // `owned self` is also accepted. Both type-check.
    let deinit = "@fieldwise_init\nstruct R:\n    var id: Int\n    def __del__(deinit self):\n        print(self.id)\n\ndef main():\n    var a: R = R(1)\n    print(a.id)\n";
    assert!(check_source(deinit).is_ok());
    let owned = "@fieldwise_init\nstruct R:\n    var id: Int\n    def __del__(owned self):\n        print(self.id)\n\ndef main():\n    var a: R = R(1)\n    print(a.id)\n";
    assert!(check_source(owned).is_ok());
}

#[test]
fn borrow_check_is_place_sensitive() {
    // Field-aware: two `mut` borrows of *disjoint* fields of the same variable are
    // fine, but overlapping places (a field vs itself, or the whole vs a field) are
    // rejected.
    let common = "@fieldwise_init\nstruct P(Copyable):\n    var a: Int\n    var b: Int\n\n";
    let disjoint = format!(
        "{common}def f(mut x: Int, mut y: Int):\n    x = y\n\ndef main():\n    var p: P = P(1, 2)\n    f(p.a, p.b)\n    print(p.a)\n"
    );
    assert!(
        check_source(&disjoint).is_ok(),
        "disjoint fields must be allowed"
    );

    let same_field = format!(
        "{common}def f(mut x: Int, y: Int):\n    x = y\n\ndef main():\n    var p: P = P(1, 2)\n    f(p.a, p.a)\n    print(p.a)\n"
    );
    assert!(matches!(
        check_source(&same_field),
        Err(TypeError::AliasingViolation { .. })
    ));

    let whole_vs_field = format!(
        "{common}def g(mut x: P, y: Int):\n    y = y\n\ndef main():\n    var p: P = P(1, 2)\n    g(p, p.a)\n    print(p.a)\n"
    );
    assert!(matches!(
        check_source(&whole_vs_field),
        Err(TypeError::AliasingViolation { .. })
    ));
}

// --- Operator overloading (dunder dispatch) ---

const VEC2: &str = "@fieldwise_init\nstruct Vec2:\n    var x: Int\n    def __add__(self, o: Vec2) -> Vec2:\n        return Vec2(self.x + o.x)\n    def __eq__(self, o: Vec2) -> Bool:\n        return self.x == o.x\n    def __getitem__(self, i: Int) -> Int:\n        return self.x\n    def __len__(self) -> Int:\n        return 1\n    def __str__(self) -> String:\n        return String(self.x)\n    def __contains__(self, v: Int) -> Bool:\n        return self.x == v\n\n";

#[test]
fn dunder_dispatch_type_checks_operators_and_builtins() {
    // A struct with the right dunders participates in `+`, `==`, subscript, `len`,
    // `String`, and `in` — each typed by the dunder's signature.
    ok(&format!(
        "{VEC2}def main():\n    var a: Vec2 = Vec2(1)\n    var b: Vec2 = a + a\n    var e: Bool = a == b\n    var i: Int = a[0]\n    var n: Int = len(a)\n    var s: String = String(a)\n    var m: Bool = 3 in a\n"
    ));
}

#[test]
fn operator_without_dunder_is_rejected() {
    // A struct that doesn't define the operator's dunder still fails to type-check.
    let e = err(
        "@fieldwise_init\nstruct P:\n    var x: Int\n\ndef main():\n    var a: P = P(1)\n    var b: P = a + a\n",
    );
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {e:?}");
    // `!=` is NOT auto-derived from `__eq__` (strict subset — Mojo requires `__ne__`).
    let e = err(
        "@fieldwise_init\nstruct Q:\n    var x: Int\n    def __eq__(self, o: Q) -> Bool:\n        return self.x == o.x\n\ndef main():\n    var m: Bool = Q(1) != Q(2)\n",
    );
    assert!(matches!(e, TypeError::BadOperator { .. }), "got {e:?}");
}

#[test]
fn len_dunder_must_return_int() {
    // `len(x)` requires `__len__ -> Int`; a wrong return type is a type error.
    let e = err(
        "@fieldwise_init\nstruct Bad:\n    var x: Int\n    def __len__(self) -> String:\n        return \"nope\"\n\ndef main():\n    var n: Int = len(Bad(1))\n",
    );
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {e:?}");
}

#[test]
fn setitem_dunder_typing_and_errors() {
    // `c[i] = e` needs `__setitem__`; a struct with only `__getitem__` can't be
    // assigned into.
    let e = err(
        "@fieldwise_init\nstruct P:\n    var a: Int\n    def __getitem__(self, i: Int) -> Int:\n        return self.a\n\ndef main():\n    var p: P = P(1)\n    p[0] = 9\n",
    );
    assert!(matches!(e, TypeError::NotIndexable(_)), "got {e:?}");
    // `__setitem__` must take `mut self` (else the write couldn't persist).
    let e = err(
        "@fieldwise_init\nstruct P:\n    var a: Int\n    def __setitem__(self, i: Int, v: Int):\n        pass\n\ndef main():\n    var p: P = P(1)\n    p[0] = 9\n",
    );
    assert!(matches!(e, TypeError::TypeMismatch { .. }), "got {e:?}");
    // A well-formed `__setitem__` type-checks (value coerces to the 2nd parameter).
    ok(
        "@fieldwise_init\nstruct P:\n    var a: Int\n    def __setitem__(mut self, i: Int, v: Int):\n        self.a = v\n\ndef main():\n    var p: P = P(1)\n    p[0] = 9\n",
    );
}

#[test]
fn user_iterator_protocol_typing() {
    // A struct with a valid `__iter__` → iterator (`__next__`/`__len__`) is iterable.
    ok(
        "@fieldwise_init\nstruct I:\n    var c: Int\n    var s: Int\n    def __len__(self) -> Int:\n        return self.s - self.c\n    def __next__(mut self) -> Int:\n        var v: Int = self.c\n        self.c = self.c + 1\n        return v\n\n@fieldwise_init\nstruct C:\n    var n: Int\n    def __iter__(self) -> I:\n        return I(0, self.n)\n\ndef main():\n    for x in C(3):\n        print(x)\n",
    );
    // No `__iter__` → not iterable.
    assert!(matches!(
        err(
            "@fieldwise_init\nstruct P:\n    var x: Int\n\ndef main():\n    for i in P(1):\n        print(i)\n"
        ),
        TypeError::NoSuchMethod { .. }
    ));
    // The iterator's `__next__` must be `mut self` (it advances).
    assert!(matches!(
        err(
            "@fieldwise_init\nstruct I:\n    var c: Int\n    def __len__(self) -> Int:\n        return 0\n    def __next__(self) -> Int:\n        return 0\n@fieldwise_init\nstruct C:\n    var n: Int\n    def __iter__(self) -> I:\n        return I(0)\n\ndef main():\n    for x in C(1):\n        print(x)\n"
        ),
        TypeError::TypeMismatch { .. }
    ));
}

#[test]
fn unsafe_pointer_typing() {
    // alloc + index read/write + free type-check.
    ok(
        "def main():\n    var p: UnsafePointer[Int] = UnsafePointer[Int].alloc(4)\n    p[0] = 1\n    var x: Int = p[0]\n    p.free()\n",
    );
    // `alloc` needs an Int count.
    assert!(matches!(
        err("def main():\n    var p: UnsafePointer[Int] = UnsafePointer[Int].alloc(\"x\")\n"),
        TypeError::TypeMismatch { .. }
    ));
    // A bare parameterized type is not a value.
    assert!(matches!(
        err("def main():\n    var x = UnsafePointer[Int]\n    print(1)\n"),
        TypeError::TypeMismatch { .. }
    ));
    // A store must match the pointee type.
    assert!(matches!(
        err(
            "def main():\n    var p: UnsafePointer[Int] = UnsafePointer[Int].alloc(1)\n    p[0] = \"no\"\n"
        ),
        TypeError::TypeMismatch { .. }
    ));
}

#[test]
fn copyinit_makes_type_copyable_and_checks_di() {
    // Defining `__copyinit__` makes a struct Copyable, so `var q = p` is allowed.
    ok(
        "struct P:\n    var a: Int\n    def __init__(out self):\n        self.a = 1\n    def __copyinit__(out self, e: P):\n        self.a = e.a\n\ndef main():\n    var p: P = P()\n    var q: P = p\n    print(q.a)\n",
    );
    // A struct without `__copyinit__`/Copyable is move-only: `var q = p` is rejected.
    assert!(matches!(
        err(
            "struct P:\n    var a: Int\n    def __init__(out self):\n        self.a = 1\n\ndef main():\n    var p: P = P()\n    var q: P = p\n"
        ),
        TypeError::NonCopyable { .. }
    ));
    // Definite-init applies to `__copyinit__` too (must set every field).
    assert!(matches!(
        err(
            "struct P:\n    var a: Int\n    var b: Int\n    def __init__(out self):\n        self.a = 0\n        self.b = 0\n    def __copyinit__(out self, e: P):\n        self.a = e.a\n\ndef main():\n    var p: P = P()\n    var q: P = p\n"
        ),
        TypeError::UninitializedField { .. }
    ));
}

#[test]
fn generic_hand_written_init() {
    // A hand-written `__init__` on a *generic* struct: the type parameter is solved
    // by unifying the constructor's parameters against the arguments (inferred), or
    // supplied explicitly (`Box[Int](5)`).
    ok(
        "struct Box[T: Copyable & Movable]:\n    var v: Self.T\n    def __init__(out self, v: Self.T):\n        self.v = v\n    def get(self) -> Self.T:\n        return self.v\n\ndef main():\n    var a: Box[Int] = Box(5)\n    var b: Box[Int] = Box[Int](6)\n    print(a.get(), b.get())\n",
    );
    // A UnsafePointer field of the type parameter, allocated with `Self.T`.
    ok(
        "struct Buf[T: Copyable & Movable]:\n    var data: UnsafePointer[Self.T]\n    def __init__(out self):\n        self.data = UnsafePointer[Self.T].alloc(4)\n    def set0(mut self, v: Self.T):\n        self.data[0] = v\n    def get0(self) -> Self.T:\n        return self.data[0]\n\ndef main():\n    var b: Buf[Int] = Buf[Int]()\n    b.set0(9)\n    print(b.get0())\n",
    );
    // A wrong-typed constructor argument is still rejected (solved T = Int here).
    assert!(matches!(
        err(
            "struct Box[T: Copyable & Movable]:\n    var v: Self.T\n    def __init__(out self, v: Self.T):\n        self.v = v\n\ndef main():\n    var a: Box[Int] = Box[Int](\"no\")\n"
        ),
        TypeError::TypeMismatch { .. }
    ));
}
