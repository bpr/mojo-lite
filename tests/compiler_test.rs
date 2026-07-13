use mojito::{Compiler, CompilerError, Value};

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
