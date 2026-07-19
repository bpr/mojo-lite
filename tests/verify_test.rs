//! Phase — standalone typed-MIR verification tests.
//!
//! Positive coverage lowers every executable fixture and requires complete
//! register/place typing with no invariant errors, before and after drop
//! elaboration. Negative coverage (hand-built malformed MIR) pins each
//! verifier check class.

use mojito::analysis::elaborate_drops_program;
use mojito::mir::verify::verify;
use mojito::{check_program, link};
use std::fs;
use std::path::Path;

#[test]
fn every_executable_fixture_lowers_with_complete_typed_mir() {
    let mut checked_fixtures = 0;
    for category in ["ok", "origin_ok", "ownership_ok"] {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join(category);
        for entry in fs::read_dir(dir).expect("fixture directory") {
            let path = entry.expect("fixture entry").path();
            if path.extension().is_none_or(|extension| extension != "mojo") {
                continue;
            }
            let program = link(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let program = mojito::elaborate(program)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let checked = check_program(&program)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let mir = mojito::mir::lower_checked_program(&checked);
            assert!(
                mir.invariant_errors.is_empty(),
                "{}: {:?}",
                path.display(),
                mir.invariant_errors
            );
            // Drop elaboration must preserve every verification invariant the
            // VM relies on: the executed program is the elaborated one.
            let elaborated = elaborate_drops_program(mir);
            let errors = verify(&elaborated);
            assert!(errors.is_empty(), "{}: {errors:?}", path.display());
            checked_fixtures += 1;
        }
    }
    assert!(checked_fixtures > 40, "fixture corpus unexpectedly small");
}

// --- Negative coverage: hand-built malformed MIR per verifier check class ---

use mojito::Ty;
use mojito::mir::{
    Const, FuncRef, MirBlock, MirDeclarations, MirFunction, MirInstr, MirProgram, MirTerm, Reg,
    SpanTable,
};
use std::collections::HashMap;

fn function(blocks: Vec<MirBlock>, n_regs: u32, reg_types: &[(u32, Ty)]) -> MirFunction {
    MirFunction {
        blocks,
        n_regs,
        n_vars: 1,
        var_names: vec!["x".to_string()],
        n_params: 0,
        param_types: Vec::new(),
        owned_params: Vec::new(),
        ref_params: Vec::new(),
        returns_reference: false,
        var_tys: HashMap::new(),
        ret_ty: Some(Ty::Int),
        raises: false,
        error_ty: None,
        spans: SpanTable::default(),
        reg_types: reg_types.iter().cloned().collect(),
    }
}

fn program(f: MirFunction) -> MirProgram {
    MirProgram {
        functions: vec![("test".to_string(), f)],
        declarations: MirDeclarations::default(),
        invariant_errors: Vec::new(),
    }
}

fn block(instrs: Vec<MirInstr>, term: MirTerm) -> MirBlock {
    MirBlock { instrs, term }
}

fn expect_finding(prog: &MirProgram, needle: &str) {
    let errors = verify(prog);
    assert!(
        errors.iter().any(|error| error.contains(needle)),
        "expected a finding containing '{needle}', got {errors:?}"
    );
}

#[test]
fn verifier_rejects_untyped_registers() {
    let f = function(
        vec![block(
            vec![MirInstr::Const {
                dest: Reg(0),
                k: Const::Int(1),
            }],
            MirTerm::Return(Some(Reg(0))),
        )],
        1,
        &[],
    );
    expect_finding(&program(f), "untyped register r0");
}

#[test]
fn verifier_rejects_invalid_jump_targets() {
    let f = function(vec![block(Vec::new(), MirTerm::Jump(7))], 0, &[]);
    expect_finding(&program(f), "jump to invalid block 7");
}

#[test]
fn verifier_rejects_top_level_falloff_and_escape() {
    let f = function(vec![block(Vec::new(), MirTerm::FallOff)], 0, &[]);
    expect_finding(&program(f), "FallOff terminator outside a try region");
    let f = function(
        vec![block(
            Vec::new(),
            MirTerm::EscapeJump {
                target: 0,
                cleanup: Vec::new(),
            },
        )],
        0,
        &[],
    );
    expect_finding(&program(f), "EscapeJump terminator outside a try region");
}

#[test]
fn verifier_rejects_non_bool_branch_conditions() {
    let f = function(
        vec![
            block(
                vec![MirInstr::Const {
                    dest: Reg(0),
                    k: Const::Int(1),
                }],
                MirTerm::Branch {
                    cond: Reg(0),
                    then_b: 1,
                    else_b: 1,
                },
            ),
            block(Vec::new(), MirTerm::Return(None)),
        ],
        1,
        &[(0, Ty::Int)],
    );
    expect_finding(&program(f), "branch condition has type Int");
}

#[test]
fn verifier_rejects_return_type_mismatches() {
    let f = function(
        vec![block(
            vec![MirInstr::Const {
                dest: Reg(0),
                k: Const::Str("no".to_string()),
            }],
            MirTerm::Return(Some(Reg(0))),
        )],
        1,
        &[(0, Ty::String)],
    );
    expect_finding(
        &program(f),
        "return of String from a function returning Int",
    );
}

#[test]
fn verifier_rejects_unprotected_raises_in_nonraising_functions() {
    let f = function(
        vec![block(
            vec![
                MirInstr::Const {
                    dest: Reg(0),
                    k: Const::Str("boom".to_string()),
                },
                MirInstr::Raise { src: Reg(0) },
            ],
            MirTerm::Return(None),
        )],
        1,
        &[(0, Ty::String)],
    );
    expect_finding(&program(f), "unprotected raise in nonraising function");
}

#[test]
fn verifier_rejects_mismatched_declared_call_arguments() {
    use mojito::mir::MirFunctionDeclaration;
    let f = function(
        vec![block(
            vec![
                MirInstr::Const {
                    dest: Reg(0),
                    k: Const::Str("no".to_string()),
                },
                MirInstr::Call {
                    dest: Reg(1),
                    func: FuncRef::named("callee"),
                    raises: None,
                    args: vec![Reg(0)],
                    kwargs: Vec::new(),
                    arg_places: vec![None],
                    param_arg_regs: Vec::new(),
                },
            ],
            MirTerm::Return(None),
        )],
        2,
        &[(0, Ty::String), (1, Ty::Int)],
    );
    let mut prog = program(f);
    prog.declarations.functions.push(MirFunctionDeclaration {
        lowered_name: "callee".to_string(),
        param_names: vec!["x".to_string()],
        param_types: vec![Ty::Int],
        defaults: vec![None],
        required: vec![true],
        variadic: None,
        variadic_index: None,
        kw_variadic: None,
        kw_variadic_index: None,
        positional_only: None,
        keyword_only: None,
        param_decls: Vec::new(),
        ret_ty: Ty::Int,
        raises: false,
        error_ty: None,
        ref_params: vec![false],
    });
    expect_finding(
        &prog,
        "argument 0 of 'callee' has type String, declared Int",
    );
}
