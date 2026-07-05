use mojo_lite::ast::{
    ArgConvention, Decorator, Expr, FnParam, ImportName, ImportNames, InfixOp, KwArg, Method, Param,
    ParamArg, ParamKind, PrefixOp, Stmt, TStringPart, TraitComptime, TraitMethod, Type, TypeParam,
    WithItem,
};
use mojo_lite::{Lexer, Parser};

/// The `@fieldwise_init` decorator, as it appears in a parsed struct's list.
fn fieldwise_deco() -> Decorator {
    Decorator { path: vec!["fieldwise_init".into()], args: vec![], kwargs: vec![] }
}

/// A plain (regular, no-default, no-convention) function parameter.
fn fnparam(name: &str, ty: Type) -> FnParam {
    FnParam { name: name.into(), ty, default: None, kind: ParamKind::Regular, convention: None }
}

fn iname(name: &str, alias: Option<&str>) -> ImportName {
    ImportName { name: name.into(), alias: alias.map(Into::into) }
}

fn parse(source: &str) -> Vec<Stmt> {
    let mut parser = Parser::new(Lexer::new(source));
    parser.parse_program().expect("parse error")
}

/// Parse a single bare-expression statement and return its expression.
fn parse_expr(source: &str) -> Expr {
    let stmts = parse(source);
    assert_eq!(stmts.len(), 1, "expected exactly one statement");
    match stmts.into_iter().next().unwrap() {
        Stmt::Expr(expr) => expr,
        other => panic!("expected an expression statement, got {:?}", other),
    }
}

fn int(n: i64) -> Box<Expr> {
    Box::new(Expr::Int(n))
}

fn ident(name: &str) -> Box<Expr> {
    Box::new(Expr::Identifier(name.into()))
}

#[test]
fn product_binds_tighter_than_sum() {
    // 1 + 2 * 3  ==  1 + (2 * 3)
    assert_eq!(
        parse_expr("1 + 2 * 3"),
        Expr::Infix(
            InfixOp::Add,
            int(1),
            Box::new(Expr::Infix(InfixOp::Mul, int(2), int(3)))
        )
    );
}

#[test]
fn parentheses_override_precedence() {
    // (1 + 2) * 3
    assert_eq!(
        parse_expr("(1 + 2) * 3"),
        Expr::Infix(
            InfixOp::Mul,
            Box::new(Expr::Infix(InfixOp::Add, int(1), int(2))),
            int(3)
        )
    );
}

#[test]
fn subtraction_is_left_associative() {
    // 1 - 2 - 3  ==  (1 - 2) - 3
    assert_eq!(
        parse_expr("1 - 2 - 3"),
        Expr::Infix(
            InfixOp::Sub,
            Box::new(Expr::Infix(InfixOp::Sub, int(1), int(2))),
            int(3)
        )
    );
}

#[test]
fn unary_minus_binds_tighter_than_sum() {
    // -a + 1  ==  (-a) + 1
    assert_eq!(
        parse_expr("-a + 1"),
        Expr::Infix(
            InfixOp::Add,
            Box::new(Expr::Prefix(PrefixOp::Neg, ident("a"))),
            int(1)
        )
    );
}

#[test]
fn not_binds_looser_than_comparison() {
    // not a == b  ==  not (a == b)
    assert_eq!(
        parse_expr("not a == b"),
        Expr::Prefix(
            PrefixOp::Not,
            Box::new(Expr::Infix(InfixOp::Eq, ident("a"), ident("b")))
        )
    );
}

#[test]
fn or_is_looser_than_and() {
    // a or b and c  ==  a or (b and c)
    assert_eq!(
        parse_expr("a or b and c"),
        Expr::Infix(
            InfixOp::Or,
            ident("a"),
            Box::new(Expr::Infix(InfixOp::And, ident("b"), ident("c")))
        )
    );
}

#[test]
fn parses_call_with_args() {
    assert_eq!(
        parse_expr("f(1, a)"),
        Expr::Call {
            name: "f".into(),
            param_args: vec![],
            args: vec![Expr::Int(1), Expr::Identifier("a".into())],
            kwargs: vec![],
        }
    );
}

#[test]
fn parses_struct_with_field_and_method() {
    let stmts =
        parse("@fieldwise_init\nstruct Point:\n    var x: Int\n\n    def get(self) -> Int:\n        return self.x\n");
    assert_eq!(
        stmts[0],
        Stmt::Struct {
            name: "Point".into(),
            decorators: vec![fieldwise_deco()],
            type_params: vec![],
            conforms: vec![],
            fields: vec![Param { name: "x".into(), ty: Type::Int }],
            methods: vec![Method {
                name: "get".into(),
                has_self: true,
                self_convention: None,
                decorators: vec![],
                params: vec![],
                positional_only: None,
                keyword_only: None,
                raises: false,
                ret: Some(Type::Int),
                body: vec![Stmt::Return(Some(Expr::Member {
                    object: ident("self"),
                    field: "x".into(),
                }))],
            }],
            fieldwise_init: true,
        }
    );
}

#[test]
fn parses_member_access_and_method_call() {
    assert_eq!(
        parse_expr("p.x"),
        Expr::Member { object: ident("p"), field: "x".into() }
    );
    assert_eq!(
        parse_expr("p.move(1, a)"),
        Expr::MethodCall {
            object: ident("p"),
            method: "move".into(),
            args: vec![Expr::Int(1), Expr::Identifier("a".into())],
            kwargs: vec![],
        }
    );
}

#[test]
fn member_access_chains_left_to_right() {
    // a.b.c  ==  (a.b).c
    assert_eq!(
        parse_expr("a.b.c"),
        Expr::Member {
            object: Box::new(Expr::Member { object: ident("a"), field: "b".into() }),
            field: "c".into(),
        }
    );
}

#[test]
fn power_is_right_associative_and_binds_tighter_than_unary_minus() {
    // 2 ** 3 ** 2  ==  2 ** (3 ** 2)
    assert_eq!(
        parse_expr("2 ** 3 ** 2"),
        Expr::Infix(
            InfixOp::Pow,
            int(2),
            Box::new(Expr::Infix(InfixOp::Pow, int(3), int(2))),
        )
    );
    // -2 ** 2  ==  -(2 ** 2)
    assert_eq!(
        parse_expr("-2 ** 2"),
        Expr::Prefix(
            PrefixOp::Neg,
            Box::new(Expr::Infix(InfixOp::Pow, int(2), int(2))),
        )
    );
}

#[test]
fn floor_div_and_mod_have_product_precedence() {
    // 1 + 6 // 4 % 3  ==  1 + ((6 // 4) % 3)
    assert_eq!(
        parse_expr("1 + 6 // 4 % 3"),
        Expr::Infix(
            InfixOp::Add,
            int(1),
            Box::new(Expr::Infix(
                InfixOp::Mod,
                Box::new(Expr::Infix(InfixOp::FloorDiv, int(6), int(4))),
                int(3),
            )),
        )
    );
}

#[test]
fn parses_float_literal_and_division() {
    // 1.0 / 2.0 + 3  ==  (1.0 / 2.0) + 3   ('/' has product precedence)
    assert_eq!(
        parse_expr("1.0 / 2.0 + 3"),
        Expr::Infix(
            InfixOp::Add,
            Box::new(Expr::Infix(
                InfixOp::Div,
                Box::new(Expr::Float(1.0)),
                Box::new(Expr::Float(2.0)),
            )),
            int(3),
        )
    );
}

#[test]
fn parses_uint_and_float64_annotations() {
    assert_eq!(
        parse("var u: UInt = UInt(0)")[0],
        Stmt::VarDecl {
            name: "u".into(),
            ty: Some(Type::UInt),
            value: Expr::Call { name: "UInt".into(), param_args: vec![], args: vec![Expr::Int(0)], kwargs: vec![] },
        }
    );
    assert_eq!(
        parse("var f: Float64 = 3.5")[0],
        Stmt::VarDecl {
            name: "f".into(),
            ty: Some(Type::Float64),
            value: Expr::Float(3.5),
        }
    );
}

#[test]
fn parses_typed_var_decl() {
    assert_eq!(
        parse("var x: Int = 1 + 2")[0],
        Stmt::VarDecl {
            name: "x".into(),
            ty: Some(Type::Int),
            value: Expr::Infix(InfixOp::Add, int(1), int(2)),
        }
    );
}

#[test]
fn parses_def_signature_and_body() {
    let stmts = parse("def add(a: Int, b: Int) -> Int:\n    return a + b\n");
    assert_eq!(
        stmts[0],
        Stmt::Def {
            name: "add".into(),
            decorators: vec![],
            type_params: vec![],
            params: vec![fnparam("a", Type::Int), fnparam("b", Type::Int)],
            positional_only: None,
            keyword_only: None,
            raises: false,
            ret: Some(Type::Int),
            body: vec![Stmt::Return(Some(Expr::Infix(
                InfixOp::Add,
                ident("a"),
                ident("b")
            )))],
        }
    );
}

#[test]
fn parses_if_elif_else() {
    let stmts = parse("if a:\n    pass\nelif b:\n    pass\nelse:\n    pass\n");
    assert_eq!(
        stmts[0],
        Stmt::If {
            branches: vec![
                (Expr::Identifier("a".into()), vec![Stmt::Pass]),
                (Expr::Identifier("b".into()), vec![Stmt::Pass]),
            ],
            orelse: Some(vec![Stmt::Pass]),
        }
    );
}

#[test]
fn parses_if_without_else() {
    let stmts = parse("if a:\n    pass\n");
    assert_eq!(
        stmts[0],
        Stmt::If {
            branches: vec![(Expr::Identifier("a".into()), vec![Stmt::Pass])],
            orelse: None,
        }
    );
}

#[test]
fn parses_while() {
    let stmts = parse("while a:\n    pass\n");
    assert_eq!(
        stmts[0],
        Stmt::While {
            cond: Expr::Identifier("a".into()),
            body: vec![Stmt::Pass],
        }
    );
}

#[test]
fn parses_for_over_range() {
    let stmts = parse("for i in range(n):\n    pass\n");
    assert_eq!(
        stmts[0],
        Stmt::For {
            var: "i".into(),
            iter: Expr::Call {
                name: "range".into(),
                param_args: vec![],
                args: vec![Expr::Identifier("n".into())],
                kwargs: vec![],
            },
            body: vec![Stmt::Pass],
        }
    );
}

#[test]
fn parses_assignment() {
    assert_eq!(
        parse("x = 1 + 2")[0],
        Stmt::Assign {
            name: "x".into(),
            value: Expr::Infix(InfixOp::Add, int(1), int(2)),
        }
    );
}

#[test]
fn rejects_non_identifier_assignment_target() {
    let mut parser = Parser::new(Lexer::new("1 = 2\n"));
    assert!(parser.parse_program().is_err());
}

#[test]
fn parses_break_and_continue() {
    let stmts = parse("while a:\n    break\n    continue\n");
    assert_eq!(
        stmts[0],
        Stmt::While {
            cond: Expr::Identifier("a".into()),
            body: vec![Stmt::Break, Stmt::Continue],
        }
    );
}

// --- Parameterization (generics) ---

#[test]
fn parses_generic_struct_header_and_self_param_field() {
    let stmts = parse(
        "@fieldwise_init\nstruct Pair[T: Copyable & Movable]:\n    var left: Self.T\n    var right: Self.T\n",
    );
    assert_eq!(
        stmts[0],
        Stmt::Struct {
            name: "Pair".into(),
            decorators: vec![fieldwise_deco()],
            type_params: vec![TypeParam {
                name: "T".into(),
                bounds: vec!["Copyable".into(), "Movable".into()],
            }],
            conforms: vec![],
            fields: vec![
                Param { name: "left".into(), ty: Type::SelfParam("T".into()) },
                Param { name: "right".into(), ty: Type::SelfParam("T".into()) },
            ],
            methods: vec![],
            fieldwise_init: true,
        }
    );
}

#[test]
fn parses_generic_def_with_type_param_signature() {
    let stmts = parse("def id[T: AnyType](x: T) -> T:\n    return x\n");
    assert_eq!(
        stmts[0],
        Stmt::Def {
            name: "id".into(),
            decorators: vec![],
            type_params: vec![TypeParam { name: "T".into(), bounds: vec!["AnyType".into()] }],
            params: vec![fnparam("x", Type::Named("T".into(), vec![]))],
            positional_only: None,
            keyword_only: None,
            raises: false,
            ret: Some(Type::Named("T".into(), vec![])),
            body: vec![Stmt::Return(Some(Expr::Identifier("x".into())))],
        }
    );
}

#[test]
fn parses_parameterized_type_annotation() {
    // `Pair[Int]` in a `var` annotation carries its type argument.
    let stmts = parse("var p: Pair[Int] = q\n");
    match &stmts[0] {
        Stmt::VarDecl { ty: Some(ty), .. } => {
            assert_eq!(*ty, Type::Named("Pair".into(), vec![ParamArg::Type(Type::Int)]));
        }
        other => panic!("expected a var decl, got {:?}", other),
    }
}

#[test]
fn rejects_type_parameter_without_a_bound() {
    // Mojo has no unconstrained type parameters, so `[T]` is a parse error.
    let mut parser = Parser::new(Lexer::new("def f[T](x: T) -> T:\n    return x\n"));
    assert!(parser.parse_program().is_err());
}

// --- Traits (Phase 1b) ---

#[test]
fn parses_trait_with_method_requirements() {
    let stmts = parse(
        "trait Quackable:\n    def quack(self) -> String:\n        ...\n    def volume(self, loud: Bool) -> Int:\n        ...\n",
    );
    assert_eq!(
        stmts[0],
        Stmt::Trait {
            name: "Quackable".into(),
            refines: vec![],
            methods: vec![
                TraitMethod {
                    name: "quack".into(),
                    params: vec![],
                    positional_only: None,
                    keyword_only: None,
                    ret: Some(Type::String),
                    default_body: None,
                },
                TraitMethod {
                    name: "volume".into(),
                    params: vec![fnparam("loud", Type::Bool)],
                    positional_only: None,
                    keyword_only: None,
                    ret: Some(Type::Int),
                    default_body: None,
                },
            ],
            comptime_members: vec![],
        }
    );
}

#[test]
fn parses_struct_conformance_list() {
    let stmts = parse("@fieldwise_init\nstruct Duck(Copyable, Quackable):\n    var name: String\n");
    match &stmts[0] {
        Stmt::Struct { conforms, .. } => {
            assert_eq!(conforms, &vec!["Copyable".to_string(), "Quackable".to_string()]);
        }
        other => panic!("expected a struct, got {:?}", other),
    }
}

#[test]
fn parses_bare_self_type_in_trait_method() {
    // `other: Self` — the `Self` type in a trait requirement.
    let stmts = parse("trait Eq2:\n    def same(self, other: Self) -> Bool:\n        ...\n");
    match &stmts[0] {
        Stmt::Trait { methods, .. } => {
            assert_eq!(methods[0].params[0].ty, Type::SelfType);
        }
        other => panic!("expected a trait, got {:?}", other),
    }
}

#[test]
fn parses_trait_default_method_body() {
    // A real method body parses as a default implementation (was a parse error);
    // the checker flags it — see the checker/asset tests.
    match &parse("trait Q:\n    def q(self) -> Int:\n        return 1\n")[0] {
        Stmt::Trait { methods, .. } => {
            assert_eq!(methods[0].default_body, Some(vec![Stmt::Return(Some(Expr::Int(1)))]));
        }
        other => panic!("expected a trait, got {:?}", other),
    }
}

#[test]
fn parses_trait_inheritance_list() {
    // `trait Bird(Animal, Named):` — the refinement (super-trait) list.
    match &parse("trait Bird(Animal, Named):\n    def fly(self):\n        ...\n")[0] {
        Stmt::Trait { refines, methods, .. } => {
            assert_eq!(refines, &vec!["Animal".to_string(), "Named".to_string()]);
            assert_eq!(methods[0].default_body, None); // `...` is a pure requirement
        }
        other => panic!("expected a trait, got {:?}", other),
    }
}

#[test]
fn parses_trait_comptime_member() {
    // `comptime count: Int` — a compile-time member requirement.
    match &parse("trait Repeater:\n    comptime count: Int\n")[0] {
        Stmt::Trait { comptime_members, .. } => {
            assert_eq!(
                comptime_members,
                &vec![TraitComptime { name: "count".into(), ty: Type::Int }]
            );
        }
        other => panic!("expected a trait, got {:?}", other),
    }
}

// --- Value parameters + comptime (Phase 2) ---

#[test]
fn parses_comptime_constant() {
    let stmts = parse("comptime N = 2 * 4\n");
    assert_eq!(
        stmts[0],
        Stmt::Comptime {
            name: "N".into(),
            value: Expr::Infix(InfixOp::Mul, int(2), int(4)),
        }
    );
}

#[test]
fn parses_comptime_if_with_else() {
    // `comptime if` mirrors a normal `if` (branches + optional else).
    match &parse("comptime if N > 4:\n    pass\nelse:\n    pass\n")[0] {
        Stmt::ComptimeIf { branches, orelse } => {
            assert_eq!(branches.len(), 1);
            assert_eq!(branches[0].0, Expr::Infix(InfixOp::Gt, ident("N"), int(4)));
            assert_eq!(branches[0].1, vec![Stmt::Pass]);
            assert_eq!(orelse, &Some(vec![Stmt::Pass]));
        }
        other => panic!("expected a ComptimeIf, got {:?}", other),
    }
}

#[test]
fn parses_comptime_for() {
    assert_eq!(
        parse("comptime for i in range(4):\n    pass\n")[0],
        Stmt::ComptimeFor {
            var: "i".into(),
            iter: Expr::Call {
                name: "range".into(),
                param_args: vec![],
                args: vec![Expr::Int(4)],
                kwargs: vec![],
            },
            body: vec![Stmt::Pass],
        }
    );
}

#[test]
fn parses_value_parameter_header() {
    // `[size: Int]` parses like any other parameter (the checker classifies it).
    let stmts = parse("@fieldwise_init\nstruct FixedBuffer[size: Int]:\n    var tag: Int\n");
    match &stmts[0] {
        Stmt::Struct { type_params, .. } => {
            assert_eq!(
                type_params,
                &vec![TypeParam { name: "size".into(), bounds: vec!["Int".into()] }]
            );
        }
        other => panic!("expected a struct, got {:?}", other),
    }
}

#[test]
fn parses_explicit_value_argument_in_annotation_and_call() {
    // Value argument in an annotation: `FixedBuffer[2 + 3]`.
    let stmts = parse("var b: FixedBuffer[2 + 3] = FixedBuffer[5](0)\n");
    match &stmts[0] {
        Stmt::VarDecl { ty: Some(ty), value, .. } => {
            assert_eq!(
                *ty,
                Type::Named(
                    "FixedBuffer".into(),
                    vec![ParamArg::Value(Expr::Infix(InfixOp::Add, int(2), int(3)))],
                )
            );
            // Value argument in a construction: `FixedBuffer[5](0)`.
            assert_eq!(
                *value,
                Expr::Call {
                    name: "FixedBuffer".into(),
                    param_args: vec![ParamArg::Value(Expr::Int(5))],
                    args: vec![Expr::Int(0)],
                    kwargs: vec![],
                }
            );
        }
        other => panic!("expected a var decl, got {:?}", other),
    }
}

// --- Imports (parsed, not resolved) ---

#[test]
fn parses_import_dotted_with_alias() {
    assert_eq!(
        parse("import mypackage.mymodule as mm\n")[0],
        Stmt::Import { path: vec!["mypackage".into(), "mymodule".into()], alias: Some("mm".into()) }
    );
    assert_eq!(
        parse("import mymodule\n")[0],
        Stmt::Import { path: vec!["mymodule".into()], alias: None }
    );
}

#[test]
fn parses_from_import_names_and_aliases() {
    assert_eq!(
        parse("from mypackage.mymodule import a, b as c, d\n")[0],
        Stmt::FromImport {
            level: 0,
            path: vec!["mypackage".into(), "mymodule".into()],
            names: ImportNames::Names(vec![
                iname("a", None),
                iname("b", Some("c")),
                iname("d", None),
            ]),
        }
    );
}

#[test]
fn parses_from_import_wildcard() {
    assert_eq!(
        parse("from mymodule import *\n")[0],
        Stmt::FromImport { level: 0, path: vec!["mymodule".into()], names: ImportNames::Wildcard }
    );
}

#[test]
fn parses_relative_imports() {
    // One dot before a module.
    assert_eq!(
        parse("from .mymodule import X\n")[0],
        Stmt::FromImport { level: 1, path: vec!["mymodule".into()], names: ImportNames::Names(vec![iname("X", None)]) }
    );
    // Dots only (`from . import X`).
    assert_eq!(
        parse("from . import X\n")[0],
        Stmt::FromImport { level: 1, path: vec![], names: ImportNames::Names(vec![iname("X", None)]) }
    );
    // Two dots.
    assert_eq!(
        parse("from ..pkg import X\n")[0],
        Stmt::FromImport { level: 2, path: vec!["pkg".into()], names: ImportNames::Names(vec![iname("X", None)]) }
    );
    // Three dots come through as a single ellipsis token.
    assert_eq!(
        parse("from ...pkg import X\n")[0],
        Stmt::FromImport { level: 3, path: vec!["pkg".into()], names: ImportNames::Names(vec![iname("X", None)]) }
    );
}

#[test]
fn rejects_from_without_a_module() {
    let mut parser = Parser::new(Lexer::new("from import X\n"));
    assert!(parser.parse_program().is_err());
}

// --- Exceptions ---

#[test]
fn parses_raise() {
    let stmts = parse("raise Error(\"boom\")\n");
    assert_eq!(
        stmts[0],
        Stmt::Raise(Expr::Call {
            name: "Error".into(),
            param_args: vec![],
            args: vec![Expr::Str("boom".into())],
            kwargs: vec![],
        })
    );
}

#[test]
fn parses_try_except_else_finally() {
    let stmts = parse("try:\n    pass\nexcept e:\n    pass\nelse:\n    pass\nfinally:\n    pass\n");
    assert_eq!(
        stmts[0],
        Stmt::Try {
            body: vec![Stmt::Pass],
            except: Some((Some("e".into()), vec![Stmt::Pass])),
            orelse: Some(vec![Stmt::Pass]),
            finalbody: Some(vec![Stmt::Pass]),
        }
    );
}

#[test]
fn parses_try_with_only_finally_and_bare_except() {
    // A bare `except:` (no name) and finally-only forms.
    assert_eq!(
        parse("try:\n    pass\nfinally:\n    pass\n")[0],
        Stmt::Try { body: vec![Stmt::Pass], except: None, orelse: None, finalbody: Some(vec![Stmt::Pass]) }
    );
    assert_eq!(
        parse("try:\n    pass\nexcept:\n    pass\n")[0],
        Stmt::Try { body: vec![Stmt::Pass], except: Some((None, vec![Stmt::Pass])), orelse: None, finalbody: None }
    );
}

// --- With statements (context managers) ---

#[test]
fn parses_with_single_item_and_binding() {
    assert_eq!(
        parse("with open(p) as f:\n    pass\n")[0],
        Stmt::With {
            items: vec![WithItem {
                context: Expr::Call {
                    name: "open".into(),
                    param_args: vec![],
                    args: vec![Expr::Identifier("p".into())],
                    kwargs: vec![],
                },
                var: Some("f".into()),
            }],
            body: vec![Stmt::Pass],
        }
    );
}

#[test]
fn parses_with_multiple_items_and_optional_binding() {
    // Comma-separated managers; the `as` binding is optional per item.
    match &parse("with a() as x, lock():\n    pass\n")[0] {
        Stmt::With { items, body } => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].var, Some("x".into()));
            assert_eq!(items[1].var, None);
            assert_eq!(body, &vec![Stmt::Pass]);
        }
        other => panic!("expected a With statement, got {:?}", other),
    }
}

#[test]
fn rejects_with_missing_name_after_as() {
    let mut parser = Parser::new(Lexer::new("with open(p) as:\n    pass\n"));
    assert!(parser.parse_program().is_err());
}

#[test]
fn parses_raises_effect_on_def() {
    // `raises` (with a discarded error type) parses; the def records it.
    match &parse("def f(x: Int) raises ValidationError -> Int:\n    return x\n")[0] {
        Stmt::Def { raises, .. } => assert!(*raises),
        other => panic!("expected a def, got {:?}", other),
    }
    match &parse("def g(x: Int) -> Int:\n    return x\n")[0] {
        Stmt::Def { raises, .. } => assert!(!*raises),
        other => panic!("expected a def, got {:?}", other),
    }
}

#[test]
fn parses_transfer_sigil() {
    assert_eq!(parse_expr("x^"), Expr::Transfer(ident("x")));
    // `raise e^` — transfer inside a raise.
    assert_eq!(
        parse("raise e^\n")[0],
        Stmt::Raise(Expr::Transfer(ident("e")))
    );
}

#[test]
fn rejects_try_without_except_or_finally() {
    let mut parser = Parser::new(Lexer::new("try:\n    pass\nelse:\n    pass\n"));
    assert!(parser.parse_program().is_err());
}

// --- SIMD ---

#[test]
fn parses_simd_type_and_construction() {
    let stmts = parse("var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\n");
    match &stmts[0] {
        Stmt::VarDecl { ty: Some(ty), value, .. } => {
            assert_eq!(
                *ty,
                Type::Named(
                    "SIMD".into(),
                    vec![
                        ParamArg::Value(Expr::Member {
                            object: ident("DType"),
                            field: "int32".into(),
                        }),
                        ParamArg::Value(Expr::Int(4)),
                    ],
                )
            );
            match value {
                Expr::Call { name, param_args, args, .. } => {
                    assert_eq!(name, "SIMD");
                    assert_eq!(param_args.len(), 2);
                    assert_eq!(args.len(), 4);
                }
                other => panic!("expected a SIMD construction, got {:?}", other),
            }
        }
        other => panic!("expected a var decl, got {:?}", other),
    }
}

#[test]
fn parses_subscript_as_index() {
    // `v[0]` (no following `(`) is a subscript, not a generic call.
    assert_eq!(
        parse_expr("v[0]"),
        Expr::Index { object: ident("v"), index: int(0) }
    );
}

#[test]
fn parses_nested_type_argument() {
    // A parameterized type as a type argument: `Box[Pair[Int]]`.
    let stmts = parse("var b: Box[Pair[Int]] = q\n");
    match &stmts[0] {
        Stmt::VarDecl { ty: Some(ty), .. } => {
            assert_eq!(
                *ty,
                Type::Named(
                    "Box".into(),
                    vec![ParamArg::Type(Type::Named(
                        "Pair".into(),
                        vec![ParamArg::Type(Type::Int)],
                    ))],
                )
            );
        }
        other => panic!("expected a var decl, got {:?}", other),
    }
}

// --- List literals ---

#[test]
fn parses_list_literal() {
    assert_eq!(
        parse_expr("[1, 2, 3]"),
        Expr::ListLit(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)])
    );
}

#[test]
fn rejects_empty_list_literal() {
    let mut parser = Parser::new(Lexer::new("var xs: List[Int] = []\n"));
    assert!(parser.parse_program().is_err());
}

// --- Membership: in / not in ---

#[test]
fn parses_in_and_not_in() {
    assert_eq!(
        parse_expr("x in xs"),
        Expr::Infix(InfixOp::In, ident("x"), ident("xs"))
    );
    assert_eq!(
        parse_expr("x not in xs"),
        Expr::Infix(InfixOp::NotIn, ident("x"), ident("xs"))
    );
}

#[test]
fn in_shares_comparison_precedence() {
    // `1 in xs and 2 in ys` == `(1 in xs) and (2 in ys)`
    assert_eq!(
        parse_expr("1 in xs and 2 in ys"),
        Expr::Infix(
            InfixOp::And,
            Box::new(Expr::Infix(InfixOp::In, int(1), ident("xs"))),
            Box::new(Expr::Infix(InfixOp::In, int(2), ident("ys"))),
        )
    );
}

#[test]
fn rejects_not_without_in() {
    let mut parser = Parser::new(Lexer::new("var a: Bool = 1 not xs\n"));
    assert!(parser.parse_program().is_err());
}

// --- Member-write: mut self + place assignment ---

#[test]
fn parses_mut_self_method() {
    let stmts = parse("@fieldwise_init\nstruct C:\n    var n: Int\n\n    def inc(mut self):\n        self.n = self.n + 1\n");
    let Stmt::Struct { methods, .. } = &stmts[0] else { panic!("expected a struct") };
    assert_eq!(methods[0].self_convention, Some(ArgConvention::Mut), "method should be mut self");
}

#[test]
fn parses_field_and_nested_place_assignment() {
    // `p.x = e` → SetPlace with a Member place.
    match &parse("p.x = 1\n")[0] {
        Stmt::SetPlace { place, .. } => {
            assert_eq!(*place, Expr::Member { object: ident("p"), field: "x".into() });
        }
        other => panic!("expected SetPlace, got {:?}", other),
    }
    // `xs[i].y = e` is also a place.
    assert!(matches!(&parse("xs[0].y = 1\n")[0], Stmt::SetPlace { .. }));
}

#[test]
fn rejects_non_place_assignment_target() {
    let mut parser = Parser::new(Lexer::new("f() = 1\n"));
    assert!(parser.parse_program().is_err());
}

// --- Tuple unpacking (bare form `a, b = t`; `var a, b = …` is not valid Mojo) ---

#[test]
fn parses_tuple_unpacking() {
    assert_eq!(
        parse("x, y = point\n")[0],
        Stmt::Unpack {
            targets: vec![Expr::Identifier("x".into()), Expr::Identifier("y".into())],
            value: Expr::Identifier("point".into()),
        }
    );
}

#[test]
fn tuple_unpacking_allows_place_targets() {
    // Each target obeys the assignment-target rule: a NAME or a place.
    assert!(matches!(
        &parse("p.x, xs[0] = t\n")[0],
        Stmt::Unpack { targets, .. }
            if matches!(targets[0], Expr::Member { .. })
                && matches!(targets[1], Expr::Index { .. })
    ));
}

#[test]
fn tuple_unpacking_allows_a_trailing_comma() {
    // `a, = t` is a one-target unpack (a trailing comma terminates the list).
    assert_eq!(
        parse("a, = t\n")[0],
        Stmt::Unpack {
            targets: vec![Expr::Identifier("a".into())],
            value: Expr::Identifier("t".into()),
        }
    );
}

#[test]
fn rejects_non_place_unpacking_target() {
    let mut parser = Parser::new(Lexer::new("a, f() = t\n"));
    assert!(parser.parse_program().is_err());
}

// --- The `parse` convenience helper (parse-only front end) ---

#[test]
fn parse_helper_matches_parser() {
    let src = "var x: Int = 1\n";
    assert_eq!(mojo_lite::parse(src).unwrap(), parse(src));
}

#[test]
fn parse_helper_surfaces_errors() {
    assert!(mojo_lite::parse("var x: Int =\n").is_err());
}

// --- Augmented assignment ---

#[test]
fn parses_augmented_assignment() {
    assert_eq!(
        parse("x += 1\n")[0],
        Stmt::AugAssign { place: Expr::Identifier("x".into()), op: InfixOp::Add, value: Expr::Int(1) }
    );
    // A place target is allowed too.
    assert!(matches!(
        &parse("xs[0] *= 2\n")[0],
        Stmt::AugAssign { op: InfixOp::Mul, .. }
    ));
}

#[test]
fn rejects_augmented_assignment_to_non_place() {
    let mut parser = Parser::new(Lexer::new("f() += 1\n"));
    assert!(parser.parse_program().is_err());
}

// --- Walrus / named expression ---

#[test]
fn parses_walrus_as_named_expression() {
    assert_eq!(
        parse_expr("(n := 5)"),
        Expr::Named { name: "n".into(), value: int(5) }
    );
}

#[test]
fn walrus_binds_looser_than_comparison() {
    // `(n := a > b)` == `n := (a > b)`
    assert_eq!(
        parse_expr("(n := a > b)"),
        Expr::Named {
            name: "n".into(),
            value: Box::new(Expr::Infix(InfixOp::Gt, ident("a"), ident("b"))),
        }
    );
}

#[test]
fn rejects_walrus_with_non_name_target() {
    let mut parser = Parser::new(Lexer::new("var y: Int = (1 := 5)\n"));
    assert!(parser.parse_program().is_err());
}

// --- Inferred `var` (no annotation) ---

#[test]
fn parses_inferred_var_decl() {
    assert_eq!(
        parse("var x = 1 + 2")[0],
        Stmt::VarDecl {
            name: "x".into(),
            ty: None,
            value: Expr::Infix(InfixOp::Add, int(1), int(2)),
        }
    );
}

#[test]
fn annotated_var_still_parses_with_some_ty() {
    match &parse("var x: Int = 5")[0] {
        Stmt::VarDecl { ty: Some(Type::Int), .. } => {}
        other => panic!("expected an annotated var decl, got {:?}", other),
    }
}

// --- Tuple literals ---

#[test]
fn parses_tuple_literals_and_grouping() {
    // A comma makes a tuple; a bare `(e)` is grouping (not a 1-tuple).
    assert_eq!(parse_expr("(1, 2, 3)"), Expr::TupleLit(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]));
    assert_eq!(parse_expr("(1 + 2)"), Expr::Infix(InfixOp::Add, int(1), int(2)));
    assert_eq!(parse_expr("()"), Expr::TupleLit(vec![]));
    // Trailing comma: `(a,)` is a 1-tuple.
    assert_eq!(parse_expr("(7,)"), Expr::TupleLit(vec![Expr::Int(7)]));
}

// --- Function-argument forms (parsed; semantics deferred) ---

/// Extract a `def`'s params + marker positions from a one-def program.
fn def_params(src: &str) -> (Vec<FnParam>, Option<usize>, Option<usize>) {
    match parse(src).into_iter().next().unwrap() {
        Stmt::Def { params, positional_only, keyword_only, .. } => {
            (params, positional_only, keyword_only)
        }
        other => panic!("expected a def, got {:?}", other),
    }
}

#[test]
fn parses_default_argument_value() {
    let (params, _, _) = def_params("def my_pow(base: Int, exp: Int = 2) -> Int:\n    return base\n");
    assert_eq!(params[0].default, None);
    assert_eq!(params[1].default, Some(Expr::Int(2)));
    assert_eq!(params[1].kind, ParamKind::Regular);
}

#[test]
fn parses_variadic_and_kw_variadic() {
    let (p, _, _) = def_params("def sum(*values: Int) -> Int:\n    return 0\n");
    assert_eq!(p[0].kind, ParamKind::Variadic);
    assert_eq!(p[0].name, "values");
    let (p, _, _) = def_params("def opts(**kw: Int):\n    pass\n");
    assert_eq!(p[0].kind, ParamKind::KwVariadic);
}

#[test]
fn parses_positional_only_and_keyword_only_markers() {
    let (p, slash, star) = def_params("def mn(a: Int, b: Int, /) -> Int:\n    return a\n");
    assert_eq!(p.len(), 2);
    assert_eq!(slash, Some(2));
    assert_eq!(star, None);
    let (p, slash, star) = def_params("def kw(a: Int, *, d: Bool) -> Int:\n    return a\n");
    assert_eq!(p.len(), 2); // a and d; the bare `*` is a marker, not a param
    assert_eq!(slash, None);
    assert_eq!(star, Some(1));
}

#[test]
fn parses_argument_conventions() {
    let (p, _, _) = def_params("def f(mut x: Int, owned y: String, out z: Bool, read w: Int):\n    pass\n");
    assert_eq!(p[0].convention, Some(ArgConvention::Mut));
    assert_eq!(p[1].convention, Some(ArgConvention::Owned));
    assert_eq!(p[2].convention, Some(ArgConvention::Out));
    assert_eq!(p[3].convention, Some(ArgConvention::Read));
}

#[test]
fn convention_word_stays_usable_as_a_param_name() {
    // `read` followed by `:` is the parameter name, not a convention.
    let (p, _, _) = def_params("def f(read: Int, mut: Bool):\n    pass\n");
    assert_eq!(p[0].name, "read");
    assert_eq!(p[0].convention, None);
    assert_eq!(p[1].name, "mut");
    assert_eq!(p[1].convention, None);
    // `ref` too — contextual, still a usable name when followed by `:`.
    let (p, _, _) = def_params("def g(ref: Int):\n    pass\n");
    assert_eq!(p[0].name, "ref");
    assert_eq!(p[0].convention, None);
}

#[test]
fn parses_ref_convention_with_optional_origin() {
    // `ref x` and `ref[origin] x` both give the Ref convention; the origin
    // specifier (an expression, or `_`) is parsed and discarded.
    let (p, _, _) = def_params("def f(ref a: Int, ref[b] c: Int, ref[_] d: Int):\n    pass\n");
    assert_eq!(p[0].convention, Some(ArgConvention::Ref));
    assert_eq!(p[0].name, "a");
    assert_eq!(p[1].convention, Some(ArgConvention::Ref));
    assert_eq!(p[1].name, "c");
    assert_eq!(p[2].convention, Some(ArgConvention::Ref));
    assert_eq!(p[2].name, "d");
}

#[test]
fn parses_ref_self_receiver() {
    // `ref self` (with an optional discarded origin) is recognized as a receiver.
    let stmts = parse(
        "struct S:\n    def get(ref self) -> Int:\n        return 0\n    def peek(ref[o] self) -> Int:\n        return 0\n",
    );
    match &stmts[0] {
        Stmt::Struct { methods, .. } => {
            assert_eq!(methods[0].self_convention, Some(ArgConvention::Ref));
            assert!(methods[0].has_self);
            assert_eq!(methods[1].self_convention, Some(ArgConvention::Ref));
        }
        other => panic!("expected a struct, got {:?}", other),
    }
}

#[test]
fn parses_ref_return_type() {
    // `-> ref[origin] T` parses into a `Type::Ref` (origin discarded).
    let stmts = parse("def f(x: Int) -> ref[origin_of(x)] Int:\n    return x\n");
    match &stmts[0] {
        Stmt::Def { ret, .. } => {
            assert_eq!(ret, &Some(Type::Ref(Box::new(Type::Int))));
        }
        other => panic!("expected a def, got {:?}", other),
    }
}

#[test]
fn parses_keyword_call_arguments() {
    assert_eq!(
        parse_expr("f(a=1, b=2)"),
        Expr::Call {
            name: "f".into(),
            param_args: vec![],
            args: vec![],
            kwargs: vec![
                KwArg { name: "a".into(), value: Expr::Int(1) },
                KwArg { name: "b".into(), value: Expr::Int(2) },
            ],
        }
    );
    // Mixed: positional then keyword.
    assert_eq!(
        parse_expr("f(1, b=2)"),
        Expr::Call {
            name: "f".into(),
            param_args: vec![],
            args: vec![Expr::Int(1)],
            kwargs: vec![KwArg { name: "b".into(), value: Expr::Int(2) }],
        }
    );
}

#[test]
fn rejects_positional_after_keyword_argument() {
    let mut parser = Parser::new(Lexer::new("f(a=1, 2)\n"));
    assert!(parser.parse_program().is_err());
}

// --- Expressions: ternary, chained comparison, slices (parsed; semantics deferred) ---

#[test]
fn parses_conditional_expression() {
    assert_eq!(
        parse_expr("a if c else b"),
        Expr::IfExpr {
            cond: ident("c"),
            then_branch: ident("a"),
            else_branch: ident("b"),
        }
    );
}

#[test]
fn conditional_expression_nests_right() {
    // a if p else b if q else c  ==  a if p else (b if q else c)
    assert_eq!(
        parse_expr("a if p else b if q else c"),
        Expr::IfExpr {
            cond: ident("p"),
            then_branch: ident("a"),
            else_branch: Box::new(Expr::IfExpr {
                cond: ident("q"),
                then_branch: ident("b"),
                else_branch: ident("c"),
            }),
        }
    );
}

#[test]
fn parses_chained_comparison() {
    // 0 <= i < n  becomes a Compare node with two links.
    assert_eq!(
        parse_expr("0 <= i < n"),
        Expr::Compare {
            first: int(0),
            rest: vec![
                (InfixOp::Le, Expr::Identifier("i".into())),
                (InfixOp::Lt, Expr::Identifier("n".into())),
            ],
        }
    );
}

#[test]
fn single_comparison_stays_infix() {
    // A lone comparison is unchanged (not a Compare node).
    assert_eq!(parse_expr("a < b"), Expr::Infix(InfixOp::Lt, ident("a"), ident("b")));
    assert_eq!(parse_expr("a not in b"), Expr::Infix(InfixOp::NotIn, ident("a"), ident("b")));
}

#[test]
fn parses_slice_subscripts() {
    assert_eq!(
        parse_expr("xs[1:2]"),
        Expr::Slice {
            object: ident("xs"),
            lower: Some(int(1)),
            upper: Some(int(2)),
            step: None,
        }
    );
    assert_eq!(
        parse_expr("xs[::2]"),
        Expr::Slice { object: ident("xs"), lower: None, upper: None, step: Some(int(2)) }
    );
    assert_eq!(
        parse_expr("xs[i:]"),
        Expr::Slice {
            object: ident("xs"),
            lower: Some(Box::new(Expr::Identifier("i".into()))),
            upper: None,
            step: None,
        }
    );
}

#[test]
fn plain_index_is_not_a_slice() {
    assert_eq!(
        parse_expr("xs[i]"),
        Expr::Index { object: ident("xs"), index: ident("i") }
    );
}

// --- Decorators (general grammar) + dunder / receiver conventions ---

#[test]
fn parses_general_decorators_on_def() {
    let stmts = parse("@staticmethod\n@a.b(1, k=2)\ndef f(x: Int) -> Int:\n    return x\n");
    let Stmt::Def { decorators, .. } = &stmts[0] else { panic!("expected a def") };
    assert_eq!(decorators.len(), 2);
    assert_eq!(decorators[0].path, vec!["staticmethod".to_string()]);
    assert_eq!(decorators[1].path, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(decorators[1].args, vec![Expr::Int(1)]);
    assert_eq!(decorators[1].kwargs, vec![KwArg { name: "k".into(), value: Expr::Int(2) }]);
}

#[test]
fn parses_decorator_on_struct_and_keeps_fieldwise_init() {
    let stmts = parse("@value\n@fieldwise_init\nstruct P:\n    var x: Int\n");
    let Stmt::Struct { decorators, fieldwise_init, .. } = &stmts[0] else {
        panic!("expected a struct")
    };
    assert_eq!(decorators.len(), 2);
    assert!(*fieldwise_init, "@fieldwise_init should still be recognized");
}

#[test]
fn parses_receiver_conventions_and_static_methods() {
    let stmts = parse(
        "struct S:\n    var n: Int\n    def a(mut self):\n        pass\n    def b(out self):\n        pass\n    @staticmethod\n    def c(x: Int) -> Int:\n        return x\n",
    );
    let Stmt::Struct { methods, .. } = &stmts[0] else { panic!("expected a struct") };
    assert_eq!(methods[0].self_convention, Some(ArgConvention::Mut));
    assert!(methods[0].has_self);
    assert_eq!(methods[1].self_convention, Some(ArgConvention::Out));
    assert!(methods[1].has_self);
    assert!(!methods[2].has_self, "@staticmethod has no self");
    assert_eq!(methods[2].decorators.len(), 1);
}

#[test]
fn parses_dunder_method_names() {
    let stmts = parse(
        "@fieldwise_init\nstruct V:\n    var x: Int\n    def __eq__(self, o: V) -> Bool:\n        return self.x == o.x\n",
    );
    let Stmt::Struct { methods, .. } = &stmts[0] else { panic!("expected a struct") };
    assert_eq!(methods[0].name, "__eq__");
}

// --- Function/closure type annotations (parsed; semantics deferred) ---

/// Extract the annotated type from `var NAME: TYPE = expr`.
fn var_anno_type(src: &str) -> Type {
    match parse(src).into_iter().next().unwrap() {
        Stmt::VarDecl { ty: Some(ty), .. } => ty,
        other => panic!("expected an annotated var decl, got {:?}", other),
    }
}

#[test]
fn parses_function_type_annotations() {
    assert_eq!(
        var_anno_type("var f: def(Int) -> Int = g\n"),
        Type::Func { params: vec![Type::Int], ret: Box::new(Type::Int), thin: false, raises: false }
    );
    // `thin` (non-capturing) after the parameter list, multiple params.
    assert_eq!(
        var_anno_type("var h: def(Int, Bool) thin -> String = k\n"),
        Type::Func {
            params: vec![Type::Int, Type::Bool],
            ret: Box::new(Type::String),
            thin: true,
            raises: false,
        }
    );
    // No params + `raises` effect.
    assert_eq!(
        var_anno_type("var n: def() raises -> None = m\n"),
        Type::Func { params: vec![], ret: Box::new(Type::None), thin: false, raises: true }
    );
}

#[test]
fn function_type_return_nests() {
    // `def(Int) -> def(Int) -> Int` — the return type is itself a function type.
    assert_eq!(
        var_anno_type("var c: def(Int) -> def(Int) -> Int = mk\n"),
        Type::Func {
            params: vec![Type::Int],
            ret: Box::new(Type::Func {
                params: vec![Type::Int],
                ret: Box::new(Type::Int),
                thin: false,
                raises: false,
            }),
            thin: false,
            raises: false,
        }
    );
}

#[test]
fn parses_function_typed_parameter() {
    // A function-typed parameter (with `thin`) parses.
    let stmts = parse("def apply(cb: def(Int) thin -> Int, x: Int) -> Int:\n    return x\n");
    let Stmt::Def { params, .. } = &stmts[0] else { panic!("expected a def") };
    assert_eq!(
        params[0].ty,
        Type::Func { params: vec![Type::Int], ret: Box::new(Type::Int), thin: true, raises: false }
    );
}

#[test]
fn parses_tstring_interpolations_into_subexprs() {
    assert_eq!(
        parse_expr("t\"n={n}, x={a + b}\""),
        Expr::TString {
            parts: vec![
                TStringPart::Literal("n=".into()),
                TStringPart::Expr(ident("n")),
                TStringPart::Literal(", x=".into()),
                TStringPart::Expr(Box::new(Expr::Infix(InfixOp::Add, ident("a"), ident("b")))),
            ],
            raw: false,
        }
    );
    // A raw t-string sets `raw`.
    assert_eq!(
        parse_expr("rt\"v={x}\""),
        Expr::TString {
            parts: vec![TStringPart::Literal("v=".into()), TStringPart::Expr(ident("x"))],
            raw: true,
        }
    );
}
