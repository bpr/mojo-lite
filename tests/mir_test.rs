//! Phase 2 (HIR/CFG → flattened MIR) tests. They check that expressions flatten to
//! A-Normal Form, that writes lower through places, and that the program driver
//! produces one function per `def` / method.

use mojito::hir::Cfg;
use mojito::mir::{MirInstr, MirPlace, MirTerm, Proj, lower_cfg, lower_program};
use mojito::parse;

/// Lower a single-block snippet and return that block's instructions.
fn instrs(src: &str) -> Vec<MirInstr> {
    let mir = lower_cfg(&Cfg::build(&parse(src).expect("parse error")));
    assert_eq!(
        mir.blocks.len(),
        1,
        "snippet should be one straight-line block"
    );
    mir.blocks.into_iter().next().unwrap().instrs
}

#[test]
fn lowers_a_simple_function_body() {
    // `var x = 1 + 2; return x` — one block, flattened to ANF, returning a reg.
    let cfg = Cfg::build(&parse("var x: Int = 1 + 2\nreturn x\n").unwrap());
    let mir = lower_cfg(&cfg);

    assert_eq!(
        mir.blocks.len(),
        cfg.node_count(),
        "one MIR block per HIR block"
    );
    assert!(
        matches!(mir.blocks[0].term, MirTerm::Return(Some(_))),
        "returns a value"
    );
    // Const(1), Const(2), BinOp(+), DefVar(x), UseVar(x) ⇒ 5 instrs; regs r0..r3.
    assert_eq!(mir.blocks[0].instrs.len(), 5);
    assert_eq!(mir.n_regs, 4);

    // VarId consistency: the `DefVar` and the returned `UseVar` name the same var.
    let def = mir.blocks[0].instrs.iter().find_map(|i| match i {
        MirInstr::DefVar { var, .. } => Some(*var),
        _ => None,
    });
    let used = mir.blocks[0].instrs.iter().find_map(|i| match i {
        MirInstr::UseVar { var, .. } => Some(*var),
        _ => None,
    });
    assert_eq!(
        def, used,
        "def and use must refer to the same VarId (seeded interner)"
    );
}

#[test]
fn temps_carry_real_source_spans() {
    // End-to-end span propagation (lexer → parser → MIR): each temp's recorded
    // span must slice the exact source text of the expression it came from.
    let src = "return y + 100\n";
    let mir = lower_cfg(&Cfg::build(&parse(src).expect("parse error")));
    let spans = &mir.spans.0;

    // The `y` read and the `100` constant each get a fresh reg; find them and
    // confirm their spans point back at the real tokens (not the old `(0, 0)`).
    let const_reg = mir.blocks[0]
        .instrs
        .iter()
        .find_map(|i| match i {
            MirInstr::Const { dest, .. } => Some(dest.0),
            _ => None,
        })
        .expect("a Const temp");
    let (cspan, _) = &spans[&const_reg];
    assert_eq!(&src[cspan.span.0..cspan.span.1], "100");
    assert_ne!(
        cspan.span,
        (0, 0),
        "spans must be real, not the placeholder"
    );

    let use_reg = mir.blocks[0]
        .instrs
        .iter()
        .find_map(|i| match i {
            MirInstr::UseVar { dest, .. } => Some(dest.0),
            _ => None,
        })
        .expect("a UseVar temp");
    let (uspan, origin) = &spans[&use_reg];
    assert_eq!(&src[uspan.span.0..uspan.span.1], "y");
    assert!(origin.is_some(), "a variable read records its origin VarId");
}

#[test]
fn control_flow_block_count_matches_hir() {
    let cfg = Cfg::build(&parse("if a:\n    var x: Int = 1\nelse:\n    var y: Int = 2\n").unwrap());
    let mir = lower_cfg(&cfg);
    assert_eq!(mir.blocks.len(), cfg.node_count());
    // The entry block ends in a Branch on the flattened condition.
    assert!(matches!(mir.blocks[0].term, MirTerm::Branch { .. }));
}

#[test]
fn nested_calls_flatten_to_temps_in_order() {
    // `f(g(x))` ⇒ UseVar(x); Call g([x]); Call f([g_result]).
    let is = instrs("f(g(x))\n");
    assert_eq!(is.len(), 3);
    assert!(matches!(is[0], MirInstr::UseVar { .. }));
    match (&is[1], &is[2]) {
        (
            MirInstr::Call {
                func: g,
                args: ga,
                dest: gd,
                ..
            },
            MirInstr::Call {
                func: f, args: fa, ..
            },
        ) => {
            assert_eq!(g.0, "g");
            assert_eq!(f.0, "f");
            assert_eq!(ga.len(), 1);
            assert_eq!(
                fa,
                &vec![*gd],
                "outer call takes the inner call's result register"
            );
        }
        other => panic!("expected two Calls, got {other:?}"),
    }
}

#[test]
fn transfer_lowers_to_a_move_use() {
    // `x^` is a move out of the variable.
    let is = instrs("f(x^)\n");
    assert!(is.iter().any(|i| matches!(
        i,
        MirInstr::UseVar {
            mode: mojito::mir::UseMode::Move,
            ..
        }
    )));
}

#[test]
fn member_write_lowers_to_a_store_through_a_place() {
    // `p.x = 1` ⇒ Const(1); Store { place: p.x }.
    let is = instrs("p.x = 1\n");
    let store = is.iter().find_map(|i| match i {
        MirInstr::Store { place, .. } => Some(place),
        _ => None,
    });
    match store {
        Some(MirPlace { proj, .. }) => {
            assert!(matches!(proj.as_slice(), [Proj::Field(f)] if f == "x"));
        }
        None => panic!("expected a Store, got {is:?}"),
    }
}

#[test]
fn checked_mir_places_carry_root_projection_and_storage_types() {
    let program = parse(
        "@fieldwise_init\nstruct Cell:\n    var value: Int\n\ndef main():\n    var cell = Cell(1)\n    cell.value = 2\n",
    )
    .expect("parse");
    let mir = lower_program(&program).expect("checked lowering");
    assert!(
        mir.invariant_errors.is_empty(),
        "{:?}",
        mir.invariant_errors
    );
    let function = mir
        .functions
        .iter()
        .find(|(name, _)| name == "main")
        .unwrap();
    let place = function
        .1
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .find_map(|instruction| match instruction {
            MirInstr::Store { place, .. } => Some(place),
            _ => None,
        })
        .expect("typed store place");
    assert!(place.is_typed());
    assert!(matches!(place.root_ty, Some(mojito::Ty::Struct(ref name, _)) if name == "Cell"));
    assert_eq!(place.projection_tys, vec![mojito::Ty::Int]);
    assert_eq!(place.ty, Some(mojito::Ty::Int));
}

#[test]
fn index_write_lowers_to_a_store_with_an_index_projection() {
    // `xs[0] = 1` ⇒ the place is `xs[<reg>]`.
    let is = instrs("xs[0] = 1\n");
    let store = is.iter().find_map(|i| match i {
        MirInstr::Store { place, .. } => Some(place),
        _ => None,
    });
    assert!(
        matches!(store.map(|p| p.proj.as_slice()), Some([Proj::Index(_)])),
        "index write should Store through an Index projection, got {is:?}"
    );
}

#[test]
fn nested_place_write_stacks_projections() {
    // `p.items[i].x = 1` ⇒ place proj = [Field(items), Index(i), Field(x)].
    let is = instrs("p.items[0].x = 1\n");
    let place = is
        .iter()
        .find_map(|i| match i {
            MirInstr::Store { place, .. } => Some(place),
            _ => None,
        })
        .expect("a Store");
    assert!(
        matches!(
            place.proj.as_slice(),
            [Proj::Field(a), Proj::Index(_), Proj::Field(b)] if a == "items" && b == "x"
        ),
        "got {:?}",
        place.proj
    );
}

#[test]
fn aug_assign_on_a_name_is_read_modify_write() {
    // `x += 1` ⇒ UseVar(x); Const(1); BinOp(+); DefVar(x) — one read, one write.
    let is = instrs("x += 1\n");
    assert_eq!(is.len(), 4);
    assert!(matches!(is[0], MirInstr::UseVar { .. }));
    assert!(matches!(is[3], MirInstr::DefVar { .. }));
    // The read and the write-back name the same variable.
    let read = match is[0] {
        MirInstr::UseVar { var, .. } => var,
        _ => unreachable!(),
    };
    let write = match is[3] {
        MirInstr::DefVar { var, .. } => var,
        _ => unreachable!(),
    };
    assert_eq!(read, write);
}

#[test]
fn aug_assign_through_a_place_loads_and_stores_the_same_place() {
    // `xs[0] += 1` ⇒ one LoadPlace + one Store, both over the SAME place — i.e. the
    // subscript index is flattened once and shared (not re-evaluated for the store).
    let is = instrs("xs[0] += 1\n");
    let idx_reg = |p: &MirPlace| match p.proj.as_slice() {
        [Proj::Index(r)] => *r,
        other => panic!("expected a single Index projection, got {other:?}"),
    };
    let loaded = is.iter().find_map(|i| match i {
        MirInstr::LoadPlace { place, .. } => Some(idx_reg(place)),
        _ => None,
    });
    let stored = is.iter().find_map(|i| match i {
        MirInstr::Store { place, .. } => Some(idx_reg(place)),
        _ => None,
    });
    assert!(
        loaded.is_some() && loaded == stored,
        "load and store share one index reg: {is:?}"
    );
}

#[test]
fn program_driver_makes_a_function_per_def_and_method() {
    let program = parse(
        "def f() -> Int:\n    return 1\n\n@fieldwise_init\nstruct P:\n    var x: Int\n    def get(self) -> Int:\n        return self.x\n\nvar top: Int = 0\n",
    )
    .unwrap();
    let mir = lower_program(&program).expect("type error");
    let names: Vec<&str> = mir.functions.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"f"), "a def becomes a function: {names:?}");
    assert!(
        names.contains(&"P.get"),
        "a method becomes Struct.method: {names:?}"
    );
    assert!(
        names.contains(&"__toplevel__"),
        "top-level stmts collect into __toplevel__: {names:?}"
    );
}

#[test]
fn checked_lowering_owns_typed_declarations_and_normalized_defaults() {
    let program = parse("def f(x: UInt = 3):\n    pass\n").expect("parse");
    let checked = mojito::check_program(&program).expect("check");
    let mir = mojito::mir::lower_checked_program(&checked);
    let declaration = mir
        .declarations
        .functions
        .iter()
        .find(|declaration| declaration.lowered_name == "f")
        .unwrap();
    assert_eq!(declaration.param_types, vec![mojito::Ty::UInt]);
    assert!(matches!(
        declaration.defaults.as_slice(),
        [Some(mojito::CheckedConst::Int(3))]
    ));
}

#[test]
fn executable_mir_carries_checked_binding_and_parameter_types() {
    let program =
        parse("def widen(x: UInt):\n    var y: Float64 = 3\n    print(x, y)\n").expect("parse");
    let checked = mojito::check_program(&program).expect("check");
    let mir = mojito::mir::lower_checked_program(&checked);
    let function = mir
        .functions
        .iter()
        .find(|(name, _)| name == "widen")
        .expect("lowered function");

    assert_eq!(function.1.param_types, vec![mojito::Ty::UInt]);
    assert!(
        function
            .1
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|instruction| matches!(
                instruction,
                MirInstr::DefVar {
                    binding_ty: Some(mojito::Ty::Float64),
                    ..
                }
            ))
    );
}

#[test]
fn bounded_trait_calls_carry_the_requirement_error_contract_into_mir() {
    let program = parse(
        "trait Fallible:\n    def run(self) raises -> Int: ...\n\ndef invoke[T: Fallible](value: T) raises -> Int:\n    return value.run()\n",
    )
    .expect("parse");
    let checked = mojito::check_program(&program).expect("check");
    let mir = mojito::mir::lower_checked_program(&checked);
    let invoke = &mir
        .functions
        .iter()
        .find(|(name, _)| name == "invoke")
        .expect("invoke function")
        .1;

    assert!(invoke.blocks.iter().flat_map(|block| &block.instrs).any(
        |instruction| matches!(
            instruction,
            MirInstr::MethodCall {
                method,
                raises: Some(mojito::Ty::Error),
                ..
            } if method == "run"
        )
    ));
}

#[test]
fn checked_declaration_types_are_keyed_by_source_site_not_type_syntax() {
    let program = parse(
        "def keep_any[T: AnyType](x: T):\n    pass\n\
         def keep_hashable[T: Hashable](x: T):\n    pass\n",
    )
    .expect("parse");
    let checked = mojito::check_program(&program).expect("check");
    let mir = mojito::mir::lower_checked_program(&checked);

    let param_type = |name: &str| {
        mir.declarations
            .functions
            .iter()
            .find(|declaration| declaration.lowered_name.starts_with(name))
            .expect("function declaration")
            .param_types[0]
            .clone()
    };
    assert_eq!(
        param_type("keep_any"),
        mojito::Ty::Param {
            name: "T".into(),
            bounds: vec!["AnyType".into()],
        }
    );
    assert_eq!(
        param_type("keep_hashable"),
        mojito::Ty::Param {
            name: "T".into(),
            bounds: vec!["Hashable".into()],
        }
    );
}

#[test]
fn mir_declarations_carry_generic_free_and_method_keyword_collectors() {
    let program = parse(
        "def collect[T: Copyable & Movable](**options: T):\n    pass\n\nstruct Relay:\n    def collect[T: Copyable & Movable](self, **options: T):\n        pass\n",
    )
    .expect("parse");
    let checked = mojito::check_program(&program).expect("check");
    let mir = mojito::mir::lower_checked_program(&checked);
    let collector = |name: &str| {
        mir.declarations
            .functions
            .iter()
            .find(|declaration| declaration.lowered_name == name)
            .expect("keyword collector declaration")
    };

    for declaration in [collector("collect"), collector("Relay.collect")] {
        assert_eq!(declaration.kw_variadic_index, Some(0));
        assert_eq!(
            declaration.kw_variadic,
            Some(mojito::Ty::Param {
                name: "T".into(),
                bounds: vec!["Copyable".into(), "Movable".into()],
            })
        );
    }
}

#[test]
fn compatibility_lowering_propagates_checker_errors() {
    let program = parse("def bad() -> Int:\n    return missing\n").expect("parse");
    assert!(matches!(
        lower_program(&program),
        Err(mojito::TypeError::UndefinedVariable(name)) if name == "missing"
    ));
}

#[test]
fn member_read_lowers_to_a_place_load() {
    // A pure field chain read (`p.a`) lowers to a `LoadPlace` (a place read), so
    // the ownership analysis sees which field is read (field-sensitivity). A
    // member of a temporary keeps the register-based `GetField`.
    let is = instrs("var p: Foo = mk()\nreturn p.a\n");
    let place = is.iter().find_map(|i| match i {
        MirInstr::LoadPlace { place, .. } => Some(place),
        _ => None,
    });
    match place {
        Some(MirPlace { proj, .. }) => {
            assert!(
                matches!(proj.as_slice(), [Proj::Field(f)] if f == "a"),
                "got {proj:?}"
            );
        }
        None => panic!("member read should be a LoadPlace, got {is:?}"),
    }
    assert!(
        !is.iter().any(|i| matches!(i, MirInstr::GetField { .. })),
        "a pure field read should not use GetField"
    );
}

#[test]
fn partial_move_lowers_to_a_move_place() {
    // `p.a^` (a pure field chain transfer) lowers to a `MovePlace` over that
    // field, distinct from a whole-variable `UseVar { Move }`.
    let is = instrs("var p: Foo = mk()\nvar x: Bar = p.a^\n");
    let place = is.iter().find_map(|i| match i {
        MirInstr::MovePlace { place, .. } => Some(place),
        _ => None,
    });
    match place {
        Some(MirPlace { proj, .. }) => {
            assert!(
                matches!(proj.as_slice(), [Proj::Field(f)] if f == "a"),
                "got {proj:?}"
            );
        }
        None => panic!("partial move should be a MovePlace, got {is:?}"),
    }
}

#[test]
fn break_crossing_try_lowers_to_escape_jump() {
    use mojito::mir::{MirInstr, MirTerm, lower_program};
    // `break` inside a `try` in a `for` loop lowers to a `MirTerm::EscapeJump` in
    // the try's body region — not a `MirInstr::Unsupported`.
    let src = "def main():\n    for i in range(3):\n        try:\n            break\n        finally:\n            print(i)\n";
    let prog = lower_program(&parse(src).expect("parse"));
    let prog = prog.expect("type error");
    let (_, main) = prog
        .functions
        .iter()
        .find(|(n, _)| n == "main")
        .expect("main");

    let mut found_escape = false;
    let mut found_unsupported = false;
    for b in &main.blocks {
        for instr in &b.instrs {
            match instr {
                MirInstr::Unsupported(_) => found_unsupported = true,
                MirInstr::Try { body, .. } => {
                    for rb in body {
                        if matches!(rb.term, MirTerm::EscapeJump { .. }) {
                            found_escape = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        found_escape,
        "break in the try body should lower to an EscapeJump"
    );
    assert!(
        !found_unsupported,
        "a function-level try escape must not be Unsupported"
    );
}
