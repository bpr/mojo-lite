use mojito::{Compiler, CompilerError, SemanticAdjustment, Value, ValueCategory};

#[test]
fn compiler_driver_runs_the_authoritative_pipeline() {
    let compiler = Compiler::default();
    let program = compiler
        .compile_unlinked("comptime n = 2 + 3\ndef main():\n    var x: Int = n\n    print(x)\n")
        .expect("compile");
    let execution = compiler.execute(&program).expect("execute");
    assert_eq!(execution.output, "5\n");
    assert!(
        execution
            .bindings
            .iter()
            .any(|(name, value)| { name == "n" && *value == Value::Int(5) })
    );
}

#[test]
fn compiler_driver_reports_the_failing_stage() {
    let compiler = Compiler::default();
    let error = compiler
        .compile_unlinked("def bad() -> Int:\n    return missing\n")
        .expect_err("type error");
    assert!(matches!(error, CompilerError::Type(_)));

    let error = compiler
        .compile_unlinked(
            "@fieldwise_init\nstruct P:\n    var x: Int\ndef main():\n    var p: P = P(1)\n    var q: P = p^\n    print(p.x)\n",
        )
        .expect_err("ownership error");
    assert!(matches!(error, CompilerError::Ownership(_)));
}

#[test]
fn compiler_rejects_executable_file_scope() {
    let compiler = Compiler::default();
    let error = compiler
        .compile_unlinked("var x: Int = 1\nprint(x)\n")
        .expect_err("file-scope execution must be rejected");
    assert!(matches!(
        error,
        CompilerError::Type(mojito::TypeError::InvalidModuleScope(_))
    ));
}

#[test]
fn checked_boundary_carries_types_categories_edges_and_adjustments() {
    let program = Compiler::default()
        .compile_unlinked(
            "def choose(value: Int) -> Int:\n    return value\ndef choose(value: String) -> Int:\n    return len(value)\ndef main():\n    var result: Int = choose(42)\n    print(result)\n",
        )
        .expect("compile");
    let expressions = program.checked().expressions();
    assert!(
        expressions
            .iter()
            .all(|node| node.id.0 < expressions.len() as u32)
    );
    assert!(
        expressions.iter().all(|node| {
            node.ty.is_some()
                || matches!(
                    node.category,
                    ValueCategory::Type | ValueCategory::CompileTime
                )
        }),
        "runtime checked expressions must carry types: {expressions:#?}"
    );
    assert!(expressions.iter().any(|node| {
        node.ty.as_ref().is_some_and(|ty| ty.to_string() == "Int")
            && node.category == ValueCategory::Place
    }));
    assert!(expressions.iter().any(|node| {
        node.adjustments
            .iter()
            .any(|adjustment| matches!(adjustment, SemanticAdjustment::ResolveCallable(_)))
    }));
    assert!(
        expressions
            .iter()
            .flat_map(|node| &node.children)
            .all(|child| (child.0 as usize) < expressions.len())
    );
}

#[test]
fn checked_hir_and_mir_retain_selected_trait_call_effects() {
    let program = Compiler::default()
        .compile_unlinked(
            "trait Fallible:\n    def run(self) raises -> Int: ...\n\n@fieldwise_init\nstruct Failure(Fallible):\n    var code: Int\n    def run(self) raises -> Int:\n        raise \"failed\"\n        return self.code\n\ndef invoke[T: Fallible](value: T) raises -> Int:\n    return value.run()\n\ndef main():\n    try:\n        var ignored = invoke(Failure(1))\n    except error:\n        pass\n",
        )
        .expect("compile trait effect program");

    let checked_call = program.checked().expressions().iter().find(|expression| {
        matches!(
            &expression.syntax.kind,
            mojito::ast::ExprKind::MethodCall { method, .. } if method == "run"
        )
    });
    assert_eq!(
        checked_call
            .and_then(|expression| expression.effects.raises.as_ref())
            .map(ToString::to_string),
        Some("Error".to_string())
    );

    let mir = mojito::mir::lower_checked_program(program.checked());
    assert!(mir.functions.iter().any(|(_, function)| {
        function.blocks.iter().any(|block| {
            block.instrs.iter().any(|instruction| {
                matches!(
                    instruction,
                    mojito::mir::MirInstr::MethodCall {
                        method,
                        raises: Some(error),
                        ..
                    } if method == "run" && error.to_string() == "Error"
                )
            })
        })
    }));
}

#[test]
fn linked_std_utils_variant_constructs_tests_projects_and_sets() {
    let compiler = Compiler::default();
    let program = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var numeric = Variant[Int, UInt](1)\n    print(numeric.isa[Int](), numeric.isa[UInt]())\n    var value: Variant[Int, String] = Variant[Int, String](7)\n    print(value.isa[Int]())\n    print(value[Int])\n    value.set[String](\"mojo\")\n    print(value.isa[String]())\n    print(value[String])\n",
            std::path::Path::new("/tmp/mojito_variant_completion.mojo"),
        )
        .expect("compile linked Variant");
    let execution = compiler.execute(&program).expect("execute Variant");
    assert_eq!(execution.output, "True False\nTrue\n7\nTrue\nmojo\n");
    assert!(program.checked().expressions().iter().any(|expression| {
        expression.adjustments.iter().any(|adjustment| {
            matches!(
                adjustment,
                SemanticAdjustment::ConstructVariant { index: 0, .. }
            )
        })
    }));
    assert!(program.checked().expressions().iter().any(|expression| {
        expression
            .adjustments
            .iter()
            .any(|adjustment| matches!(adjustment, SemanticAdjustment::VariantSet { index: 1, .. }))
    }));
}

#[test]
fn explicit_type_pack_specializes_variant_annotation_and_construction() {
    let compiler = Compiler::default();
    let program = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef first_variant[*Ts: Movable]() -> Variant[*Ts]:\n    return Variant[*Ts](3)\n\ndef main():\n    var value = first_variant[Int, String]()\n    print(value.isa[Int]())\n    print(value[Int])\n",
            std::path::Path::new("/tmp/mojito_variant_type_pack.mojo"),
        )
        .expect("specialize a Variant type pack");
    let execution = compiler
        .execute(&program)
        .expect("execute specialized Variant construction");
    assert_eq!(execution.output, "True\n3\n");

    let unsupported_alternative = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef first_variant[*Ts: Movable]() -> Variant[*Ts]:\n    return Variant[*Ts](True)\n\ndef main():\n    _ = first_variant[Int, String]()\n",
            std::path::Path::new("/tmp/mojito_variant_type_pack_bad_arm.mojo"),
        )
        .expect_err("the specialized constructor value must match an alternative");
    assert!(matches!(unsupported_alternative, CompilerError::Type(_)));
}

#[test]
fn variant_requires_import_and_checks_projection_tags() {
    let compiler = Compiler::default();
    let unimported = compiler
        .compile_unlinked("def main():\n    var value = Variant[Int, String](7)\n")
        .expect_err("Variant is not a prelude type");
    assert!(matches!(
        unimported,
        CompilerError::Type(mojito::TypeError::UndefinedVariable(name)) if name == "Variant"
    ));

    let program = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var value = Variant[Int, String](7)\n    print(value[String])\n",
            std::path::Path::new("/tmp/mojito_variant_wrong_tag.mojo"),
        )
        .expect("wrong active tag is a runtime check");
    let error = compiler
        .execute(&program)
        .expect_err("typed projection must check the active tag");
    assert!(matches!(
        error,
        CompilerError::Runtime(mojito::RuntimeError::TypeError(message))
            if message.contains("holds 'Int', not 'String'")
    ));
}

#[test]
fn variant_type_queries_take_and_replace_have_checked_ownership_semantics() {
    let compiler = Compiler::default();
    let program = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var value = Variant[Int, String](7)\n    print(value.is_type_supported[Int](), value.is_type_supported[Float64]())\n    var old = value.replace[String, Int](\"seven\")\n    print(old, value[String])\n    var taken = value.take[String]()\n    print(taken)\n    var unchecked = Variant[Int, String](9)\n    var unsafe_old = unchecked.unsafe_replace[String, Int](\"nine\")\n    var unsafe_taken = unchecked.unsafe_take[String]()\n    print(unsafe_old, unsafe_taken)\n",
            std::path::Path::new("/tmp/mojito_variant_take_replace.mojo"),
        )
        .expect("compile Variant take/replace operations");
    let execution = compiler
        .execute(&program)
        .expect("execute Variant take/replace operations");
    assert_eq!(execution.output, "True False\n7 seven\nseven\n9 nine\n");

    let unsupported = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var value = Variant[Int, String](7)\n    _ = value.take[Float64]()\n",
            std::path::Path::new("/tmp/mojito_variant_unsupported_take.mojo"),
        )
        .expect_err("unsupported Variant operation arm must be rejected statically");
    assert!(matches!(unsupported, CompilerError::Type(_)));

    let moved = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var value = Variant[Int, String](7)\n    _ = value.take[Int]()\n    print(value.isa[Int]())\n",
            std::path::Path::new("/tmp/mojito_variant_use_after_take.mojo"),
        )
        .expect_err("Variant.take consumes its receiver");
    assert!(matches!(moved, CompilerError::Ownership(_)));

    let wrong_tag = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var value = Variant[Int, String](7)\n    _ = value.take[String]()\n",
            std::path::Path::new("/tmp/mojito_variant_wrong_take_tag.mojo"),
        )
        .expect("a checked take validates its dynamic tag at runtime");
    let wrong_tag = compiler
        .execute(&wrong_tag)
        .expect_err("checked Variant.take must trap on a tag mismatch");
    assert!(matches!(
        wrong_tag,
        CompilerError::Runtime(mojito::RuntimeError::TypeError(message))
            if message.contains("holds 'Int', not 'String'")
    ));

    let wrong_replace_tag = compiler
        .compile_source(
            "from std.utils import Variant\n\ndef main():\n    var value = Variant[Int, String](7)\n    _ = value.replace[String, String](\"replacement\")\n",
            std::path::Path::new("/tmp/mojito_variant_wrong_replace_tag.mojo"),
        )
        .expect("a checked replace validates its dynamic output tag at runtime");
    let wrong_replace_tag = compiler
        .execute(&wrong_replace_tag)
        .expect_err("checked Variant.replace must trap on a tag mismatch");
    assert!(matches!(
        wrong_replace_tag,
        CompilerError::Runtime(mojito::RuntimeError::TypeError(message))
            if message.contains("holds 'Int', not 'String'")
    ));
}

#[test]
fn variant_protocols_are_conditioned_on_every_alternative() {
    let compiler = Compiler::default();
    let program = compiler
        .compile_source(
            "from std.utils import Variant\n\n@fieldwise_init\nstruct Styled(Writable):\n    var value: Int\n    def write_to(self, mut writer: Some[Writer]):\n        writer.write(\"styled=\", self.value)\n    def write_repr_to(self, mut writer: Some[Writer]):\n        writer.write(\"Styled[\", self.value, \"]\")\n\ndef main():\n    var left = Variant[Int, UInt](7)\n    var same = Variant[Int, UInt](7)\n    var other = Variant[Int, UInt](UInt(7))\n    print(hash(left) == hash(same), hash(left) == hash(other))\n    var styled = Variant[Styled, Int](Styled(4))\n    print(String(styled), repr(styled))\n    var copied = left\n    print(copied == left)\n",
            std::path::Path::new("/tmp/mojito_variant_protocols.mojo"),
        )
        .expect("all alternatives satisfy the requested Variant protocols");
    let execution = compiler
        .execute(&program)
        .expect("execute conditional Variant protocols");
    assert_eq!(execution.output, "True False\nstyled=4 Styled[4]\nTrue\n");

    for (name, body, expected_trait) in [
        ("hash", "print(hash(value))", "Hashable"),
        ("write", "print(value)", "Writable"),
        ("equality", "print(value == value)", "Equatable"),
    ] {
        let source = format!(
            "from std.utils import Variant\n\n@fieldwise_init\nstruct Opaque:\n    var value: Int\n\ndef main():\n    var value = Variant[Int, Opaque](Opaque(1))\n    {body}\n"
        );
        let error = compiler
            .compile_source(
                &source,
                std::path::Path::new(&format!("/tmp/mojito_variant_non_{name}.mojo")),
            )
            .expect_err("one unsupported alternative must disable the protocol");
        match expected_trait {
            "Equatable" | "Writable" => assert!(matches!(error, CompilerError::Type(_))),
            trait_name => assert!(matches!(
                error,
                CompilerError::Type(mojito::TypeError::TraitNotSatisfied {
                    trait_name: found,
                    ..
                }) if found == trait_name
            )),
        }
    }

    let noncopyable = compiler
        .compile_source(
            "from std.utils import Variant\n\n@fieldwise_init\nstruct MoveOnly:\n    var value: Int\n\ndef main():\n    var value = Variant[Int, MoveOnly](MoveOnly(1))\n    var copied = value\n    print(copied.isa[MoveOnly]())\n",
            std::path::Path::new("/tmp/mojito_variant_noncopyable.mojo"),
        )
        .expect_err("a Variant is Copyable only when every alternative is Copyable");
    assert!(matches!(noncopyable, CompilerError::Type(_)));

    let nondeletable = compiler
        .compile_source(
            "from std.utils import Variant\n\nstruct Linear(ImplicitlyDeletable where False):\n    pass\n\ndef require_deletable[T: ImplicitlyDeletable]():\n    pass\n\ndef main():\n    require_deletable[Variant[Int, Linear]]()\n",
            std::path::Path::new("/tmp/mojito_variant_nondeletable.mojo"),
        )
        .expect_err("a Variant is deletable only when every alternative is deletable");
    assert!(matches!(nondeletable, CompilerError::Type(_)));
}

#[test]
fn pipeline_verifies_typed_mir_before_execution() {
    // The verification stage sits between checking and ownership: a healthy
    // program compiles, and the dedicated error variant renders findings as a
    // compiler invariant report rather than a user diagnostic.
    let compiler = Compiler::default();
    compiler
        .compile_source(
            "def main():\n    var x = 1\n    print(x)\n",
            std::path::Path::new("/tmp/mojito_verify_stage.mojo"),
        )
        .expect("a checked program passes MIR verification");
    let rendered = CompilerError::Verify(vec![
        "fn 'main': register r1 has no checked type".to_string(),
    ])
    .to_string();
    assert!(rendered.contains("invalid checked program"));
    assert!(rendered.contains("register r1"));
}
