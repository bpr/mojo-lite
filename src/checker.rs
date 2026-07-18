//! Static semantic checker: the authoritative handoff between elaborated AST and
//! compiler lowering. It resolves annotations, calls, traits, and conventions
//! into [`CheckedProgram`](crate::checked::CheckedProgram). It is a *sound*
//! approximation: if [`check`] succeeds, compiled execution will not raise
//! `UndefinedVariable`, `TypeError`, `NotCallable`, `ArityMismatch`, or
//! `ClosureEscape`. It is deliberately not *complete* — see the forward-reference
//! note below — so a few valid Mojo programs are rejected.
//!
//! ## Scoping
//! A stack of scopes (`Vec<HashMap<String, Ty>>`) models lexical name lookup.
//! Names are bound *sequentially* in source order, and a nested `def` body is checked at its definition
//! site with the enclosing scopes still on the stack (so capture is lexical).
//! One consequence: a function body may not forward-reference a sibling `def`
//! declared later in the same block (mutual recursion). Choosing soundness over completeness here keeps
//! the checker simple; hoisting `def` signatures per block is future work.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArgConvention, Dtype, Expr, ExprKind, FnParam, InfixOp, Method, PrefixOp, Stmt, StmtKind,
    StructComptime, SubscriptArg, TStringPart, TraitComptime, Type as SourceType,
};
use crate::call::{
    ArgSlot, CallVariadics, MatchError, effective_keyword_only_index, match_call_slots,
    regular_marker_index,
};
use crate::ct::{CtExpr, CtValue};
use crate::error::TypeError;
use crate::token::{SourceSpan, Span};
use crate::types::{ConstraintOperand, GenericConstraint, ParamDecl, SliceKind, Ty, TyArg};

/// The checked signature of a struct, kept in the checker's registry.
struct StructInfo {
    /// Compile-time parameters (type and value); empty for a non-generic struct.
    decls: Vec<ParamDecl>,
    /// Traits this struct declares conformance to (verified at definition).
    conforms: Vec<String>,
    callable_conformance: Option<Ty>,
    conformance_conditions: HashMap<String, Expr>,
    /// Declared fields, in order (drives the fieldwise constructor).
    fields: Vec<(String, Ty)>,
    /// Associated compile-time facts declared by `comptime NAME = ...` in the
    /// struct body. These live on the type, not on runtime instances.
    associated: HashMap<String, CtValue>,
    methods: HashMap<String, Vec<MethodSig>>,
    fieldwise_init: bool,
    explicit_destroy_message: Option<String>,
    explicit_destructors: HashMap<String, bool>,
}

/// The source-level pieces of a struct declaration passed through checking.
struct StructDeclaration<'a> {
    module: &'a Option<String>,
    span: Span,
    name: &'a str,
    type_params: &'a [crate::ast::TypeParam],
    conforms: &'a [String],
    callable_conformance: &'a Option<SourceType>,
    conformance_conditions: &'a [(String, Expr)],
    fields: &'a [crate::ast::Param],
    associated: &'a [StructComptime],
    methods: &'a [Method],
    fieldwise_init: bool,
    decorators: &'a [crate::ast::Decorator],
}

/// The checked signature of a trait: required methods plus associated
/// compile-time facts. A method requirement's signature may mention
/// `Ty::SelfType` (the conforming type).
struct TraitInfo {
    refines: Vec<String>,
    methods: HashMap<String, Vec<MethodSig>>,
    comptime_members: HashMap<String, CtMemberReq>,
}

/// The required kind/type of a trait `comptime NAME: Annotation` member.
#[derive(Clone, PartialEq)]
enum CtMemberReq {
    /// A compile-time value whose value type must match this type.
    Value(Ty),
    /// A compile-time type value whose type must conform to these trait bounds.
    Type { bounds: Vec<String> },
}

/// Compose inherited associated-member requirements. Type-valued members with
/// the same name denote one associated type, so refinement accumulates their
/// bounds instead of treating stronger composition as an ambiguity. Value
/// members must retain one exact type; mixing value and type requirements is a
/// real conflict.
fn merge_associated_requirement(
    existing: &mut CtMemberReq,
    incoming: &CtMemberReq,
    member: &str,
) -> Result<(), TypeError> {
    match (existing, incoming) {
        (CtMemberReq::Type { bounds }, CtMemberReq::Type { bounds: more }) => {
            for bound in more {
                if !bounds.contains(bound) {
                    bounds.push(bound.clone());
                }
            }
            Ok(())
        }
        (CtMemberReq::Value(left), CtMemberReq::Value(right)) if left == right => Ok(()),
        _ => Err(TypeError::Unsupported(format!(
            "conflicting inherited associated member '{member}'"
        ))),
    }
}

fn conformance_operand(expression: &Expr, arguments: &HashMap<&str, &TyArg>) -> Option<CtValue> {
    match &expression.kind {
        ExprKind::Int(value) => Some(CtValue::Int(*value)),
        ExprKind::Bool(value) => Some(CtValue::Bool(*value)),
        ExprKind::Str(value) => Some(CtValue::Str(value.clone())),
        ExprKind::Identifier(name) => match arguments.get(name.as_str())? {
            TyArg::Val(value) => Some((*value).clone()),
            TyArg::Ty(_) => None,
        },
        _ => None,
    }
}

#[derive(Clone, PartialEq)]
struct MethodSig {
    decls: Vec<ParamDecl>,
    availability: Vec<GenericConstraint>,
    has_self: bool,
    /// Regular parameters only; variadic element type is stored separately.
    params: Vec<Ty>,
    names: Vec<String>,
    required: Vec<bool>,
    variadic: Option<Box<Ty>>,
    variadic_index: Option<usize>,
    kw_variadic: Option<Box<Ty>>,
    kw_variadic_index: Option<usize>,
    positional_only: Option<usize>,
    keyword_only: Option<usize>,
    conventions: Vec<Option<ArgConvention>>,
    ret: Ty,
    raises: bool,
    error: Option<Box<Ty>>,
    /// Receiver convention. `None` means plain read-only `self`; explicit
    /// conventions (`mut`, `var`, `ref`, ...) are preserved so trait
    /// requirements can compare them exactly. Today only `mut self` changes call
    /// checking behavior.
    self_convention: Option<crate::ast::ArgConvention>,
    ref_params: Vec<Option<crate::origin::RefSig>>,
    ref_return: Option<crate::origin::RefSig>,
    implicit: bool,
}

type MethodInstantiation = (
    Vec<Ty>,
    Option<Ty>,
    Option<Ty>,
    HashMap<String, Ty>,
    HashMap<String, TyArg>,
);

impl MethodSig {
    fn intrinsic(params: Vec<Ty>, ret: Ty) -> MethodSig {
        let len = params.len();
        MethodSig {
            decls: Vec::new(),
            availability: Vec::new(),
            has_self: true,
            params,
            names: (0..len).map(|i| format!("arg{i}")).collect(),
            required: vec![true; len],
            variadic: None,
            variadic_index: None,
            kw_variadic: None,
            kw_variadic_index: None,
            positional_only: None,
            keyword_only: None,
            conventions: vec![None; len],
            ret,
            raises: false,
            error: None,
            self_convention: None,
            ref_params: vec![None; len],
            ref_return: None,
            implicit: false,
        }
    }
}

fn callable_parameter_count(ty: &Ty) -> Option<usize> {
    match ty {
        Ty::Func { params, .. } => Some(params.len()),
        Ty::GenericFunc { params, .. } => Some(params.len()),
        _ => None,
    }
}

fn place_root_name(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Identifier(name) => Some(name),
        ExprKind::Member { object, .. }
        | ExprKind::Index { object, .. }
        | ExprKind::Slice { object, .. }
        | ExprKind::MultiIndex { object, .. } => place_root_name(object),
        ExprKind::TypeApply { name, .. } => Some(name),
        _ => None,
    }
}

fn method_arity_range(sig: &MethodSig) -> (usize, usize) {
    (sig.params.len(), sig.params.len())
}

fn same_method_shape(a: &MethodSig, b: &MethodSig) -> bool {
    method_arity_range(a) == method_arity_range(b)
        && a.params == b.params
        && a.variadic == b.variadic
        && a.kw_variadic == b.kw_variadic
}

/// A conforming method may promise no error where its trait requirement raises,
/// but a raising implementation must preserve the exact declared error family.
/// Bare `raises` denotes `Error`; it is not a wildcard for a distinct typed
/// error. `raises Never` is already normalized to a non-raising signature when
/// `MethodSig` is built.
fn method_satisfies_requirement(got: &MethodSig, required: &MethodSig) -> bool {
    let mut got_shape = got.clone();
    got_shape.raises = false;
    got_shape.error = None;
    let mut required_shape = required.clone();
    required_shape.raises = false;
    required_shape.error = None;
    if got_shape != required_shape {
        return false;
    }
    if !got.raises {
        return true;
    }
    if !required.raises {
        return false;
    }
    got.error == required.error
}

fn same_callable_signature(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        (
            Ty::Func {
                params: ap,
                variadic: av,
                kw_variadic: akw,
                ..
            },
            Ty::Func {
                params: bp,
                variadic: bv,
                kw_variadic: bkw,
                ..
            },
        ) => ap == bp && av == bv && akw == bkw,
        (
            Ty::GenericFunc {
                decls: ad,
                params: ap,
                variadic: av,
                kw_variadic: akw,
                ..
            },
            Ty::GenericFunc {
                decls: bd,
                params: bp,
                variadic: bv,
                kw_variadic: bkw,
                ..
            },
        ) => {
            let aparams: Vec<_> = ap
                .iter()
                .chain(av.iter().map(Box::as_ref))
                .chain(akw.iter().map(Box::as_ref))
                .cloned()
                .collect();
            let bparams: Vec<_> = bp
                .iter()
                .chain(bv.iter().map(Box::as_ref))
                .chain(bkw.iter().map(Box::as_ref))
                .cloned()
                .collect();
            canonical_generic_signature(ad, &aparams) == canonical_generic_signature(bd, &bparams)
        }
        _ => false,
    }
}

fn canonical_generic_signature(decls: &[ParamDecl], params: &[Ty]) -> (Vec<ParamDecl>, Vec<Ty>) {
    let mut subst = HashMap::new();
    let canonical_decls = decls
        .iter()
        .enumerate()
        .map(|(index, decl)| match decl {
            ParamDecl::Type {
                name,
                bounds,
                default,
                infer_only,
                variadic,
                constraints,
            } => {
                let canonical_name = format!("${index}");
                subst.insert(
                    name.clone(),
                    Ty::Param {
                        name: canonical_name.clone(),
                        bounds: bounds.clone(),
                    },
                );
                ParamDecl::Type {
                    name: canonical_name,
                    bounds: bounds.clone(),
                    default: default.clone(),
                    infer_only: *infer_only,
                    variadic: *variadic,
                    constraints: constraints.clone(),
                }
            }
            ParamDecl::Value {
                ty,
                default,
                infer_only,
                variadic,
                constraints,
                ..
            } => ParamDecl::Value {
                name: format!("${index}"),
                ty: Box::new(substitute(ty, &subst)),
                default: default.clone(),
                infer_only: *infer_only,
                variadic: *variadic,
                constraints: constraints.clone(),
            },
        })
        .collect();
    let canonical_params = params.iter().map(|ty| substitute(ty, &subst)).collect();
    (canonical_decls, canonical_params)
}

/// The lowered symbol the checker records as the resolved callee of an
/// overloaded free-function call — formatted by the canonical symbol module so
/// it names exactly the `MirFunction` the MIR emits for that definition.
fn callable_lowered_name(name: &str, ty: &Ty) -> Option<String> {
    let (params, variadic, kw_variadic) = match ty {
        Ty::Func {
            params,
            variadic,
            kw_variadic,
            ..
        }
        | Ty::GenericFunc {
            params,
            variadic,
            kw_variadic,
            ..
        } => (params, variadic, kw_variadic),
        _ => return None,
    };
    let signature_types: Vec<_> = params
        .iter()
        .chain(variadic.iter().map(Box::as_ref))
        .chain(kw_variadic.iter().map(Box::as_ref))
        .collect();
    Some(crate::symbol::function_symbol(
        name,
        &crate::symbol::SignatureKey::from_tys(signature_types),
    ))
}

/// The lowered symbol of an overloaded method/constructor resolution, likewise
/// canonical (`sig.params` are the declared parameter types, unsubstituted —
/// matching the MIR definition side, which mangles the declared annotations).
fn method_lowered_name(type_name: &str, method: &str, sig: &MethodSig) -> String {
    let signature_types = sig
        .params
        .iter()
        .chain(sig.variadic.iter().map(Box::as_ref))
        .chain(sig.kw_variadic.iter().map(Box::as_ref));
    let signature = crate::symbol::SignatureKey::from_tys(signature_types);
    if method == "__iter__" {
        crate::symbol::iterator_method_symbol(type_name, sig.self_convention, &signature)
    } else {
        crate::symbol::method_symbol(type_name, method, &signature)
    }
}

enum OverloadSelect {
    NoMatch,
    Ambiguous,
}

const CONVERSION_RANK: usize = 1 << 24;
const VARIADIC_RANK: usize = 1 << 16;
const SIGNATURE_LENGTH_RANK: usize = 1 << 8;

fn overload_rank(conversions: usize, variadic: bool, signature_len: usize, generic: bool) -> usize {
    conversions * CONVERSION_RANK
        + usize::from(variadic) * VARIADIC_RANK
        + signature_len * SIGNATURE_LENGTH_RANK
        + usize::from(generic)
}

fn conversion_count(actual: &Ty, expected: &Ty) -> usize {
    if actual == expected
        || matches!(actual, Ty::IntLiteral) && matches!(expected, Ty::Int)
        || matches!(actual, Ty::FloatLiteral) && matches!(expected, Ty::Float64)
    {
        0
    } else {
        1
    }
}

/// A concrete method candidate after receiver-type substitution and argument
/// scoring. Named fields keep overload resolution readable as it evolves.
struct MethodCallResolution {
    conversion_score: usize,
    slots: Vec<ArgSlot>,
    keyword_overflow: Vec<usize>,
    keyword_element: Option<Ty>,
    conventions: Vec<Option<ArgConvention>>,
    return_type: Ty,
    raises: bool,
    error: Option<Box<Ty>>,
    mutates_receiver: bool,
    consumes_receiver: bool,
    lowered_name: Option<String>,
    ref_params: Vec<Option<crate::origin::RefSig>>,
    ref_return: Option<crate::origin::RefSig>,
    param_types: Vec<Ty>,
}

struct MethodCallScore {
    rank: usize,
    slots: Vec<ArgSlot>,
    keyword_overflow: Vec<usize>,
}

struct SubscriptResolution {
    return_type: Ty,
    lowered_name: Option<String>,
    value_keyword: bool,
}

type ReturnRefContract = (
    crate::origin::RefSig,
    Vec<crate::origin::OwnerId>,
    Option<crate::origin::OwnerId>,
);

fn select_callable_overload(
    matches: Vec<(Ty, usize, String, Option<Ty>)>,
) -> Result<(Ty, String, Option<Ty>), OverloadSelect> {
    let best = matches
        .iter()
        .map(|(_, score, _, _)| *score)
        .min()
        .ok_or(OverloadSelect::NoMatch)?;
    let mut best_matches = matches
        .into_iter()
        .filter(|(_, score, _, _)| *score == best)
        .collect::<Vec<_>>();
    if best_matches.len() != 1 {
        return Err(OverloadSelect::Ambiguous);
    }
    let (ret, _, target, error) = best_matches.remove(0);
    Ok((ret, target, error))
}

fn select_method_overload(
    _method: &str,
    matches: Vec<MethodCallResolution>,
) -> Result<MethodCallResolution, OverloadSelect> {
    let best = matches
        .iter()
        .map(|candidate| candidate.conversion_score)
        .min()
        .ok_or(OverloadSelect::NoMatch)?;
    let mut best_matches = matches
        .into_iter()
        .filter(|candidate| candidate.conversion_score == best)
        .collect::<Vec<_>>();
    if best_matches.len() == 1 {
        Ok(best_matches.remove(0))
    } else {
        Err(OverloadSelect::Ambiguous)
    }
}

fn overload_candidates(existing: &Ty, new_ty: &Ty) -> Option<Vec<Ty>> {
    callable_parameter_count(new_ty)?;
    match existing {
        Ty::Func { .. } | Ty::GenericFunc { .. }
            if callable_parameter_count(existing).is_some() =>
        {
            Some(vec![existing.clone()])
        }
        Ty::Overload(candidates) => Some(candidates.clone()),
        _ => None,
    }
}

/// Type-check a whole program. Convenience wrapper over [`Checker`].
pub fn check(stmts: &[Stmt]) -> Result<(), TypeError> {
    check_program(stmts).map(|_| ())
}

/// Type-check and retain the semantic facts consumed by lowering/backends.
pub fn check_program(stmts: &[Stmt]) -> Result<crate::checked::CheckedProgram, TypeError> {
    let expanded = expand_trait_defaults(stmts)?;
    let mut checker = Checker::new();
    checker.check_program(&expanded)?;
    let explicit_destroy_types = checker
        .structs
        .iter()
        .filter_map(|(name, info)| {
            let self_ty = Ty::Struct(name.clone(), info.decls.iter().map(param_as_arg).collect());
            (!checker.is_implicitly_deletable(&self_ty)).then(|| {
                (
                    name.clone(),
                    crate::checked::ExplicitDestroyInfo {
                        message: info.explicit_destroy_message.clone().unwrap_or_else(|| {
                            "value is not implicitly deletable and must be explicitly destroyed"
                                .to_string()
                        }),
                        destructors: info.explicit_destructors.clone(),
                        fields: info
                            .fields
                            .iter()
                            .filter_map(|(field, ty)| match ty {
                                Ty::Struct(field_ty, _) if !checker.is_implicitly_deletable(ty) => {
                                    Some((field.clone(), field_ty.clone()))
                                }
                                _ => None,
                            })
                            .collect(),
                    },
                )
            })
        })
        .collect();
    crate::explicit_destroy::check(
        &expanded,
        &checker.binding_types.borrow(),
        &checker.comprehension_bindings.borrow(),
        &explicit_destroy_types,
    )?;
    Ok(crate::checked::CheckedProgram::new(
        expanded,
        checker.overload_targets.into_inner(),
        checker.implicit_conversions.into_inner(),
        checker.declaration_types.into_inner(),
        checker.expression_types.into_inner(),
        checker.expression_bindings.into_inner(),
        checker.comprehension_bindings.into_inner(),
        checker.expression_place_types.into_inner(),
        checker.binding_types.into_inner(),
        checker.expression_effects.into_inner(),
        checker.iteration_protocols.into_inner(),
        checker.simd_constructions.into_inner(),
        checker.variant_operations.into_inner(),
        explicit_destroy_types,
        checker.explicit_destroy_calls.into_inner(),
        checker.reference_value_uses.into_inner(),
    ))
}

/// Materialize trait default methods into each conforming struct before semantic
/// checking. This keeps default dispatch static: downstream MIR sees an ordinary
/// struct method and needs no trait-object runtime machinery.
fn expand_trait_defaults(stmts: &[Stmt]) -> Result<Vec<Stmt>, TypeError> {
    #[derive(Clone)]
    struct TraitDefaults {
        refines: Vec<String>,
        methods: Vec<crate::ast::TraitMethod>,
    }

    fn defaults_for(
        name: &str,
        traits: &HashMap<String, TraitDefaults>,
        visiting: &mut HashSet<String>,
    ) -> Result<HashMap<String, Method>, TypeError> {
        if !visiting.insert(name.to_string()) {
            return Err(TypeError::Unsupported(format!(
                "cyclic trait refinement involving '{name}'"
            )));
        }
        let Some(info) = traits.get(name) else {
            visiting.remove(name);
            return Ok(HashMap::new());
        };
        let mut defaults = HashMap::new();
        for parent in &info.refines {
            for (method, implementation) in defaults_for(parent, traits, visiting)? {
                if defaults.insert(method.clone(), implementation).is_some() {
                    return Err(TypeError::Unsupported(format!(
                        "ambiguous inherited default method '{method}'"
                    )));
                }
            }
        }
        for method in &info.methods {
            let Some(body) = &method.default_body else {
                continue;
            };
            defaults.insert(
                method.name.clone(),
                Method {
                    name: method.name.clone(),
                    type_params: method.type_params.clone(),
                    has_self: true,
                    self_convention: method.self_convention,
                    self_origin: method.self_origin.clone(),
                    decorators: Vec::new(),
                    params: method.params.clone(),
                    positional_only: method.positional_only,
                    keyword_only: method.keyword_only,
                    raises: method.raises,
                    raises_type: method.raises_type.clone(),
                    ret: method.ret.clone(),
                    body: body.clone(),
                    where_clause: method.where_clause.clone(),
                },
            );
        }
        visiting.remove(name);
        Ok(defaults)
    }

    let traits: HashMap<_, _> = stmts
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            StmtKind::Trait {
                name,
                refines,
                methods,
                ..
            } => Some((
                name.clone(),
                TraitDefaults {
                    refines: refines.clone(),
                    methods: methods.clone(),
                },
            )),
            _ => None,
        })
        .collect();
    let mut expanded = stmts.to_vec();
    for stmt in &mut expanded {
        let StmtKind::Struct {
            conforms, methods, ..
        } = &mut stmt.kind
        else {
            continue;
        };
        let explicit: HashSet<_> = methods.iter().map(|method| method.name.clone()).collect();
        let mut inherited = HashMap::<String, Method>::new();
        for trait_name in conforms.iter() {
            for (name, implementation) in defaults_for(trait_name, &traits, &mut HashSet::new())? {
                if explicit.contains(&name) {
                    continue;
                }
                if inherited.insert(name.clone(), implementation).is_some() {
                    return Err(TypeError::Unsupported(format!(
                        "ambiguous default method '{name}'; provide an explicit override"
                    )));
                }
            }
        }
        methods.extend(inherited.into_values());
    }
    Ok(expanded)
}

/// Type-check a program and return the concrete lowered callee chosen for every
/// overloaded call site. MIR lowering uses this side table so source calls like
/// `f(x)` can lower to a signature-specific function even when overloads share
/// the same arity.
pub fn resolve_overload_targets(stmts: &[Stmt]) -> Result<HashMap<SourceSpan, String>, TypeError> {
    Ok(check_program(stmts)?.overload_targets().clone())
}

#[derive(Clone)]
struct CapturePolicy {
    /// Scope index at which the nested function's own locals begin.
    base: usize,
    function_name: String,
    entries: HashMap<String, crate::ast::CaptureKind>,
    default_read: bool,
}

/// A single-pass static type checker over the parsed AST.
pub struct Checker {
    /// Lexical scope chain, innermost last. Starts with the global scope.
    scopes: Vec<HashMap<String, Ty>>,
    /// Binding mutability, parallel to `scopes`. `var` locals are writable;
    /// ordinary function parameters are not.
    mutable_scopes: Vec<HashMap<String, bool>>,
    /// Stable identities for value bindings, parallel to `scopes`. Origin and
    /// loan facts use these identities so a shadowing declaration cannot be
    /// confused with the binding of the same source name in an outer scope.
    owner_scopes: Vec<HashMap<String, crate::origin::OwnerId>>,
    /// Origins retained inside reference-bearing aggregate bindings, parallel
    /// to the lexical value scopes.  Unlike `Ty::Struct`, this preserves the
    /// use-site owner identity needed for escape checking.
    aggregate_origin_scopes: Vec<HashMap<String, Vec<crate::origin::Origin>>>,
    /// Reference-parameter handle types. Parameter expression typing still
    /// reads through to the declared referent, while storage contexts can ask
    /// for the handle explicitly.
    reference_parameter_scopes: Vec<HashMap<String, crate::origin::RefTy>>,
    next_owner: u32,
    /// Index of the local scope for each function currently being checked.
    function_bases: Vec<usize>,
    /// Function/method scope base and caller-owned inputs which may legally
    /// appear inside a returned reference-bearing aggregate.
    aggregate_escape_contexts: Vec<(usize, HashSet<crate::origin::OwnerId>)>,
    /// Explicit capture policy for each nested function body being checked.
    capture_contexts: RefCell<Vec<CapturePolicy>>,
    /// Defined structs, by name (a separate namespace from value bindings).
    structs: HashMap<String, StructInfo>,
    /// Defined traits, by name (their method requirements).
    traits: HashMap<String, TraitInfo>,
    /// Stack of a generic `def`'s type parameters (`name → bounds`), innermost
    /// last. A bare `T` annotation resolves to `Ty::Param` when in this stack.
    /// (A `def`'s *value* parameters are ordinary `Int` locals, not here.)
    tparams: Vec<HashMap<String, Vec<String>>>,
    /// The enclosing struct's parameters while checking its fields and methods,
    /// so `Self.T` resolves to `Ty::Param` and `Self.n` to a value parameter.
    /// Saved/restored around a (possibly nested) struct definition.
    self_decls: Vec<ParamDecl>,
    enclosing_type_params: Vec<crate::ast::TypeParam>,
    /// The `Ty` denoted by a bare `Self` while checking a struct's members (the
    /// struct type) or a trait's requirements (`Ty::SelfType`). `None` elsewhere.
    self_ty: Option<Ty>,
    /// Trait-associated comptime requirements in scope while checking a trait's
    /// own method requirement signatures, so `Self.Element` can resolve.
    trait_self_comptime: Vec<HashMap<String, CtMemberReq>>,
    /// Compile-time `Int` constants declared by `comptime NAME = value`.
    comptimes: HashMap<String, i64>,
    /// Whether `self` is writable in the method body being checked — set while
    /// checking a `mut self` method's body (so `self.x = e` is allowed there).
    self_mutable: bool,
    /// An `out self` lifecycle initializer is establishing field storage.  For
    /// a reference-valued field, assigning a reference here stores its handle;
    /// later assignments write through the established handle instead.
    self_initializing: bool,
    /// Source-span to lowered callee for calls whose source name denotes an
    /// overload set. Interior mutability keeps expression inference usable from
    /// read-only helper methods while still recording resolution facts.
    overload_targets: RefCell<HashMap<SourceSpan, String>>,
    implicit_conversions: RefCell<HashMap<SourceSpan, String>>,
    simd_constructions: RefCell<HashMap<SourceSpan, (Dtype, i64)>>,
    /// Checked `Variant` construction/tag/projection/update operations.  These
    /// decisions cross the typed boundary so MIR never reinterprets syntax.
    variant_operations: RefCell<HashMap<SourceSpan, crate::checked::SemanticAdjustment>>,
    declaration_types: RefCell<HashMap<crate::checked::AnnotationSite, Ty>>,
    expression_types: RefCell<HashMap<SourceSpan, Ty>>,
    expression_bindings: RefCell<HashMap<SourceSpan, crate::origin::OwnerId>>,
    /// Stable identities/types for the lexical binders introduced by each
    /// comprehension, retained for checked HIR and explicit-destroy analysis.
    comprehension_bindings:
        RefCell<HashMap<SourceSpan, Vec<crate::checked::CheckedComprehensionBinding>>>,
    expression_place_types: RefCell<HashMap<SourceSpan, Ty>>,
    binding_types: RefCell<HashMap<SourceSpan, Ty>>,
    /// Selected call effects keyed by the checked call expression. This records
    /// the contract chosen during overload/bounded dispatch so later phases do
    /// not have to rediscover it from source syntax.
    expression_effects: RefCell<HashMap<SourceSpan, crate::checked::EffectFacts>>,
    /// Exact iterator protocol selected for each loop/comprehension iterable.
    /// Lowering consumes this fact instead of re-selecting `__iter__` by name.
    iteration_protocols: RefCell<HashMap<SourceSpan, crate::checked::IterationProtocol>>,
    explicit_destroy_calls: RefCell<std::collections::HashSet<SourceSpan>>,
    /// Expressions whose reference handle, rather than referent value, is
    /// required by an origin-bearing aggregate operation. The bool is writable.
    reference_value_uses: RefCell<HashMap<SourceSpan, bool>>,
    return_ref_contracts: Vec<Option<ReturnRefContract>>,
    named_result_context: Vec<bool>,
    raising_context: Vec<Option<Ty>>,
    handled_raise_depth: usize,
    handled_raise_types: RefCell<Vec<Vec<Ty>>>,
    uninitialized: RefCell<HashSet<crate::origin::OwnerId>>,
}

impl Checker {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
            mutable_scopes: vec![HashMap::new()],
            owner_scopes: vec![HashMap::new()],
            aggregate_origin_scopes: vec![HashMap::new()],
            reference_parameter_scopes: vec![HashMap::new()],
            next_owner: 0,
            function_bases: Vec::new(),
            aggregate_escape_contexts: Vec::new(),
            capture_contexts: RefCell::new(Vec::new()),
            structs: HashMap::new(),
            traits: HashMap::new(),
            tparams: Vec::new(),
            self_decls: Vec::new(),
            enclosing_type_params: Vec::new(),
            self_ty: None,
            trait_self_comptime: Vec::new(),
            comptimes: HashMap::new(),
            self_mutable: false,
            self_initializing: false,
            overload_targets: RefCell::new(HashMap::new()),
            implicit_conversions: RefCell::new(HashMap::new()),
            simd_constructions: RefCell::new(HashMap::new()),
            variant_operations: RefCell::new(HashMap::new()),
            declaration_types: RefCell::new(HashMap::new()),
            expression_types: RefCell::new(HashMap::new()),
            expression_bindings: RefCell::new(HashMap::new()),
            comprehension_bindings: RefCell::new(HashMap::new()),
            expression_place_types: RefCell::new(HashMap::new()),
            binding_types: RefCell::new(HashMap::new()),
            expression_effects: RefCell::new(HashMap::new()),
            iteration_protocols: RefCell::new(HashMap::new()),
            explicit_destroy_calls: RefCell::new(std::collections::HashSet::new()),
            reference_value_uses: RefCell::new(HashMap::new()),
            return_ref_contracts: Vec::new(),
            named_result_context: Vec::new(),
            raising_context: Vec::new(),
            handled_raise_depth: 0,
            handled_raise_types: RefCell::new(Vec::new()),
            uninitialized: RefCell::new(HashSet::new()),
        }
    }

    fn raising_allowed(&self) -> bool {
        self.handled_raise_depth > 0
            || self
                .raising_context
                .last()
                .is_some_and(|error| error.as_ref().is_some_and(|ty| *ty != Ty::Never))
    }

    fn require_error(&self, operation: impl Into<String>, error: Ty) -> Result<(), TypeError> {
        if self.handled_raise_depth > 0 {
            if let Some(types) = self.handled_raise_types.borrow_mut().last_mut() {
                types.push(error);
            }
            return Ok(());
        }
        if let Some(Some(expected)) = self.raising_context.last() {
            if *expected == error {
                return Ok(());
            }
            return Err(TypeError::RaiseTypeMismatch {
                expected: expected.to_string(),
                found: error.to_string(),
            });
        }
        if self.raising_allowed() {
            Ok(())
        } else {
            Err(TypeError::UnhandledRaise(operation.into()))
        }
    }

    fn record_call_effect(&self, span: SourceSpan, error: Ty) {
        self.expression_effects.borrow_mut().insert(
            span,
            crate::checked::EffectFacts {
                raises: Some(error),
                may_suspend: false,
                diverges: false,
            },
        );
    }

    fn declared_error(
        &self,
        raises: bool,
        raises_type: Option<&SourceType>,
    ) -> Result<Option<Ty>, TypeError> {
        if !raises {
            return Ok(None);
        }
        Ok(Some(match raises_type {
            Some(error) => self.ty_from_anno(error)?,
            None => Ty::Error,
        }))
    }

    /// The type denoted by a source annotation; resolves type parameters and
    /// validates struct names and type-argument counts.
    fn ty_from_anno(&self, ty: &SourceType) -> Result<Ty, TypeError> {
        self.resolve_ty_from_anno(ty)
    }

    /// Contextually instantiate a generic function value when a monomorphic
    /// callable type supplies all of its type information. Runtime execution is
    /// still type-erased; this produces the checked callable view used by the
    /// binding or argument site.
    fn value_coerces(&self, from: &Ty, to: &Ty) -> bool {
        if coerces(from, to) {
            return true;
        }
        if let Ty::Struct(name, _) = from
            && let Some(callable) = self
                .structs
                .get(name)
                .and_then(|info| info.callable_conformance.as_ref())
        {
            return coerces(callable, to);
        }
        let (
            Ty::GenericFunc {
                decls,
                params,
                ret,
                required,
                variadic,
                kw_variadic,
                positional_only,
                keyword_only,
                raises,
                error,
                conventions,
                ref_params,
                ref_return,
                ..
            },
            Ty::Func {
                params: expected_params,
                ret: expected_ret,
                ..
            },
        ) = (from, to)
        else {
            return false;
        };
        let mut patterns = params.clone();
        patterns.push((**ret).clone());
        let mut actuals = expected_params.clone();
        actuals.push((**expected_ret).clone());
        let Ok((subst, _)) =
            self.resolve_use_params("<generic callable>", decls, &[], &patterns, &actuals)
        else {
            return false;
        };
        let instantiated = Ty::Func {
            params: params.iter().map(|ty| substitute(ty, &subst)).collect(),
            names: (0..params.len())
                .map(|index| format!("arg{index}"))
                .collect(),
            ret: Box::new(substitute(ret, &subst)),
            required: required.clone(),
            variadic: variadic.as_ref().map(|ty| Box::new(substitute(ty, &subst))),
            kw_variadic: kw_variadic
                .as_ref()
                .map(|ty| Box::new(substitute(ty, &subst))),
            positional_only: *positional_only,
            keyword_only: *keyword_only,
            raises: *raises,
            error: error
                .as_ref()
                .map(|error| Box::new(substitute(error, &subst))),
            conventions: conventions.clone(),
            ref_params: ref_params.clone(),
            ref_return: ref_return.clone(),
        };
        coerces(&instantiated, to)
    }

    fn implicit_conversion_target(&self, from: &Ty, to: &Ty) -> Result<Option<String>, TypeError> {
        let Ty::Struct(name, args) = to else {
            return Ok(None);
        };
        let Some(info) = self.structs.get(name) else {
            return Ok(None);
        };
        if info.decls.len() != args.len() {
            return Ok(None);
        }
        let subst = struct_subst(&info.decls, args);
        let Some(constructors) = info.methods.get("__init__") else {
            return Ok(None);
        };
        let matches = constructors
            .iter()
            .filter(|sig| {
                sig.implicit
                    && sig.params.len() == 1
                    && coerces(from, &substitute(&sig.params[0], &subst))
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [sig] => Ok(Some(if constructors.len() == 1 {
                name.clone()
            } else {
                method_lowered_name(name, "__init__", sig)
            })),
            _ => Err(TypeError::BadCall {
                func: name.clone(),
                reason: format!("ambiguous implicit conversion from '{from}' to '{to}'"),
            }),
        }
    }

    fn record_implicit_conversion(
        &self,
        expression: &Expr,
        from: &Ty,
        to: &Ty,
    ) -> Result<bool, TypeError> {
        if let Ty::Overload(candidates) = from {
            let matches: Vec<_> = candidates
                .iter()
                .filter(|candidate| self.value_coerces(candidate, to))
                .filter_map(|candidate| {
                    let ExprKind::Identifier(name) = &expression.kind else {
                        return None;
                    };
                    callable_lowered_name(name, candidate).map(|target| (candidate, target))
                })
                .collect();
            return match matches.as_slice() {
                [(_, target)] => {
                    self.overload_targets
                        .borrow_mut()
                        .insert(expression.source_span(), target.clone());
                    Ok(true)
                }
                [] => Ok(false),
                _ => Err(TypeError::BadCall {
                    func: "overloaded callable value".to_string(),
                    reason: format!("multiple overloads fit expected type '{to}'"),
                }),
            };
        }
        if let Ty::Simd { dtype, width: 1 } = to
            && splats_to(from, *dtype)
        {
            if !literal_fits_dtype(expression, *dtype) {
                return Err(TypeError::TypeMismatch {
                    expected: to.to_string(),
                    found: from.to_string(),
                    context: "numeric literal materialization".to_string(),
                });
            }
            return Ok(true);
        }
        if self.value_coerces(from, to) {
            return Ok(true);
        }
        let Some(target) = self.implicit_conversion_target(from, to)? else {
            return Ok(false);
        };
        self.implicit_conversions
            .borrow_mut()
            .insert(expression.source_span(), target);
        Ok(true)
    }

    fn resolve_ty_from_anno(&self, ty: &SourceType) -> Result<Ty, TypeError> {
        Ok(match ty {
            SourceType::Int => Ty::Int,
            SourceType::UInt => Ty::UInt,
            SourceType::Bool => Ty::Bool,
            SourceType::String => Ty::String,
            SourceType::Float64 => Ty::Float64,
            SourceType::None => Ty::None,
            SourceType::Func {
                params,
                ret,
                thin: _,
                raises,
                raises_type,
            } => Ty::Func {
                params: params
                    .iter()
                    .map(|param| self.resolve_ty_from_anno(param))
                    .collect::<Result<_, _>>()?,
                names: (0..params.len())
                    .map(|index| format!("arg{index}"))
                    .collect(),
                ret: Box::new(self.resolve_ty_from_anno(ret)?),
                required: vec![true; params.len()],
                variadic: None,
                kw_variadic: None,
                positional_only: None,
                keyword_only: None,
                raises: *raises,
                error: if *raises {
                    Some(Box::new(match raises_type {
                        Some(error) => self.resolve_ty_from_anno(error)?,
                        None => Ty::Error,
                    }))
                } else {
                    None
                },
                conventions: vec![None; params.len()],
                ref_params: Box::new(vec![None; params.len()]),
                ref_return: None,
            },
            SourceType::Ref { referent, origin } => {
                let spec = origin.as_ref().ok_or_else(|| {
                    TypeError::Unsupported(
                        "reference-valued fields require an explicit origin".to_string(),
                    )
                })?;
                let [origin_expr] = spec.as_slice() else {
                    return Err(TypeError::Unsupported(
                        "reference-valued fields currently require one origin parameter"
                            .to_string(),
                    ));
                };
                let ExprKind::Identifier(origin_name) = &origin_expr.kind else {
                    return Err(TypeError::Unsupported(
                        "reference-valued fields require a named origin parameter".to_string(),
                    ));
                };
                if origin_name == "UnsafeAnyOrigin" {
                    return Err(TypeError::Unsupported(
                        "UnsafeAnyOrigin cannot be hidden in a stored reference field".to_string(),
                    ));
                }
                if origin_name == "UntrackedOrigin" {
                    return Ok(Ty::Ref(crate::origin::RefTy {
                        referent: Box::new(self.resolve_ty_from_anno(referent)?),
                        origin: crate::origin::Origin::Untracked { mutable: false },
                        mutability: crate::origin::Mutability::Immutable,
                    }));
                }
                let (index, parameter) = self
                    .enclosing_type_params
                    .iter()
                    .enumerate()
                    .find(|(_, parameter)| {
                        parameter.name == *origin_name && parameter.bounds.as_slice() == ["Origin"]
                    })
                    .ok_or_else(|| TypeError::UndefinedVariable(origin_name.clone()))?;
                let mutability = match parameter.origin_mutability.as_ref().map(|e| &e.kind) {
                    Some(ExprKind::Bool(true)) => crate::origin::Mutability::Mutable,
                    Some(ExprKind::Bool(false)) => crate::origin::Mutability::Immutable,
                    _ => {
                        crate::origin::Mutability::Param(crate::origin::OriginParamId(index as u32))
                    }
                };
                Ty::Ref(crate::origin::RefTy {
                    referent: Box::new(self.resolve_ty_from_anno(referent)?),
                    origin: crate::origin::Origin::Param(crate::origin::OriginParamId(
                        index as u32,
                    )),
                    mutability,
                })
            }
            // A bare name may be an in-scope type parameter (a generic `def`'s
            // `T`) or a struct type, optionally applied to parameter arguments.
            SourceType::Named(name, args) => {
                let existential_trait = args.first().and_then(|argument| match argument {
                    crate::ast::ParamArg::Type(SourceType::Named(trait_name, trait_args))
                        if trait_args.is_empty() =>
                    {
                        Some(trait_name)
                    }
                    crate::ast::ParamArg::Value(Expr {
                        kind: ExprKind::Identifier(trait_name),
                        ..
                    }) => Some(trait_name),
                    _ => None,
                });
                if name == "Some"
                    && args.len() == 1
                    && let Some(trait_name) = existential_trait
                    && (BUILTIN_TRAITS.contains(&trait_name.as_str())
                        || self.traits.contains_key(trait_name))
                {
                    return Ok(Ty::Param {
                        name: format!("Some[{trait_name}]"),
                        bounds: vec![trait_name.clone()],
                    });
                }
                if name == "Never" && args.is_empty() {
                    return Ok(Ty::Never);
                }
                if matches!(name.as_str(), "Slice" | "ContiguousSlice" | "StridedSlice")
                    && args.is_empty()
                {
                    return Ok(Ty::Struct(name.clone(), Vec::new()));
                }
                // Mojo exposes the compile-time `StringLiteral` type. Mojito
                // materializes string literals directly as runtime strings, so
                // it is represented by the existing string type.
                if name == "StringLiteral" && args.is_empty() {
                    return Ok(Ty::String);
                }
                if args.is_empty()
                    && let Some(bounds) = self.lookup_tparam(name)
                {
                    return Ok(Ty::Param {
                        name: name.clone(),
                        bounds,
                    });
                }
                // SIMD vector types and their fixed-width scalar aliases.
                if let Some(dtype) = Dtype::from_scalar_alias(name) {
                    if !args.is_empty() {
                        return Err(TypeError::WrongTypeArgCount {
                            name: name.clone(),
                            expected: 0,
                            got: args.len(),
                        });
                    }
                    return Ok(Ty::Simd { dtype, width: 1 });
                }
                if name == "SIMD" {
                    return self.simd_type(args);
                }
                if name == "Scalar" {
                    if args.len() != 1 {
                        return Err(TypeError::WrongTypeArgCount {
                            name: name.clone(),
                            expected: 1,
                            got: args.len(),
                        });
                    }
                    return Ok(simd_ty(dtype_from_arg(&args[0])?, 1));
                }
                if name == "$pack" {
                    return self.tuple_type(args);
                }
                if name == "Error" && args.is_empty() {
                    return Ok(Ty::Error);
                }
                // `Variant` is a compiler-provided tagged union even when its
                // stdlib declaration has been module-qualified by the linker.
                if is_variant_name(name) && (name != "Variant" || self.structs.contains_key(name)) {
                    return self.variant_type(args);
                }
                if let Some(info) = self.structs.get(name) {
                    let decls = info.decls.clone();
                    let (_, tyargs) = self.resolve_use_params(name, &decls, args, &[], &[])?;
                    return Ok(Ty::Struct(name.clone(), tyargs));
                }
                if name == "List" {
                    return self.list_type(args);
                }
                if name == "Set" {
                    return self.set_type(args);
                }
                if name == "Dict" {
                    return self.dict_type(args);
                }
                if name == "Tuple" {
                    return self.tuple_type(args);
                }
                if matches!(name.as_str(), "UnsafePointer" | "Pointer") {
                    return self.pointer_type(args);
                }
                return Err(TypeError::UnknownType(name.clone()));
            }
            // `Self.T` — one of the enclosing struct's *type* parameters (a value
            // parameter is not a type, so `Self.n` in type position is an error).
            SourceType::SelfParam(name) => {
                match self.self_decls.iter().find(|d| d.name() == name) {
                    Some(ParamDecl::Type { bounds, .. }) => Ty::Param {
                        name: name.clone(),
                        bounds: bounds.clone(),
                    },
                    _ => return self.associated_type_for_self(name),
                }
            }
            // Bare `Self` — the enclosing struct type or a trait's abstract Self.
            // Not usable as a type in a value-parameterized struct (a value
            // parameter can't appear in a type).
            SourceType::SelfType => match &self.self_ty {
                Some(Ty::Struct(_, args)) if args.iter().any(|a| matches!(a, TyArg::Val(_))) => {
                    return Err(TypeError::UnknownSelfParam("Self".to_string()));
                }
                Some(ty) => ty.clone(),
                None => return Err(TypeError::UnknownSelfParam("Self".to_string())),
            },
            SourceType::Assoc { base, name } => {
                let base_ty = self.ty_from_anno(base)?;
                self.associated_type_from_base(&base_ty, name)?
            }
        })
    }

    fn associated_type_for_self(&self, name: &str) -> Result<Ty, TypeError> {
        if let Some(reqs) = self.trait_self_comptime.last()
            && let Some(req) = reqs.get(name)
        {
            return match req {
                CtMemberReq::Type { .. } => Ok(Ty::Assoc {
                    base: Box::new(Ty::SelfType),
                    name: name.to_string(),
                }),
                CtMemberReq::Value(_) => Err(TypeError::NoSuchAssociatedType {
                    object_type: "Self".to_string(),
                    member: name.to_string(),
                }),
            };
        }
        let Some(self_ty) = &self.self_ty else {
            return Err(TypeError::UnknownSelfParam(name.to_string()));
        };
        if let Ty::Struct(sname, _) = self_ty
            && !self.structs.contains_key(sname)
        {
            return Err(TypeError::UnknownSelfParam(name.to_string()));
        }
        self.associated_type_from_base(self_ty, name)
    }

    fn associated_type_from_base(&self, base: &Ty, name: &str) -> Result<Ty, TypeError> {
        match base {
            Ty::Struct(sname, targs) => {
                let info = self
                    .structs
                    .get(sname)
                    .ok_or_else(|| TypeError::UnknownType(sname.clone()))?;
                let value =
                    info.associated
                        .get(name)
                        .ok_or_else(|| TypeError::NoSuchAssociatedType {
                            object_type: base.to_string(),
                            member: name.to_string(),
                        })?;
                let CtValue::Type(ty) = value else {
                    return Err(TypeError::NoSuchAssociatedType {
                        object_type: base.to_string(),
                        member: name.to_string(),
                    });
                };
                let subst = struct_subst(&info.decls, targs);
                Ok(self.resolve_assoc_ty(&substitute(ty, &subst)))
            }
            Ty::Param { bounds, .. } => {
                if self.lookup_trait_assoc_type(bounds, name).is_some() {
                    Ok(Ty::Assoc {
                        base: Box::new(base.clone()),
                        name: name.to_string(),
                    })
                } else {
                    Err(TypeError::NoSuchAssociatedType {
                        object_type: base.to_string(),
                        member: name.to_string(),
                    })
                }
            }
            Ty::Assoc { .. } => Ok(Ty::Assoc {
                base: Box::new(base.clone()),
                name: name.to_string(),
            }),
            _ => Err(TypeError::NoSuchAssociatedType {
                object_type: base.to_string(),
                member: name.to_string(),
            }),
        }
    }

    fn resolve_assoc_ty(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Assoc { base, name } => {
                let base = self.resolve_assoc_ty(base);
                self.associated_type_from_base(&base, name)
                    .unwrap_or_else(|_| Ty::Assoc {
                        base: Box::new(base),
                        name: name.clone(),
                    })
            }
            Ty::Struct(name, args) => {
                Ty::Struct(name.clone(), map_tyargs(args, |t| self.resolve_assoc_ty(t)))
            }
            Ty::List(elem) => Ty::List(Box::new(self.resolve_assoc_ty(elem))),
            Ty::Set(elem) => Ty::Set(Box::new(self.resolve_assoc_ty(elem))),
            Ty::Dict(key, value) => Ty::Dict(
                Box::new(self.resolve_assoc_ty(key)),
                Box::new(self.resolve_assoc_ty(value)),
            ),
            Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| self.resolve_assoc_ty(t)).collect()),
            Ty::Variant(alternatives) => Ty::Variant(
                alternatives
                    .iter()
                    .map(|ty| self.resolve_assoc_ty(ty))
                    .collect(),
            ),
            Ty::Pointer { element, origin } => Ty::Pointer {
                element: Box::new(self.resolve_assoc_ty(element)),
                origin: origin.clone(),
            },
            Ty::Func {
                params,
                names,
                ret,
                required,
                variadic,
                kw_variadic,
                positional_only,
                keyword_only,
                raises,
                error,
                conventions,
                ref_params,
                ref_return,
            } => Ty::Func {
                params: params.iter().map(|p| self.resolve_assoc_ty(p)).collect(),
                names: names.clone(),
                ret: Box::new(self.resolve_assoc_ty(ret)),
                required: required.clone(),
                variadic: variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                kw_variadic: kw_variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                positional_only: *positional_only,
                keyword_only: *keyword_only,
                raises: *raises,
                error: error
                    .as_ref()
                    .map(|error| Box::new(self.resolve_assoc_ty(error))),
                conventions: conventions.clone(),
                ref_params: ref_params.clone(),
                ref_return: ref_return.clone(),
            },
            Ty::GenericFunc {
                decls,
                params,
                names,
                ret,
                required,
                variadic,
                kw_variadic,
                positional_only,
                keyword_only,
                raises,
                error,
                conventions,
                ref_params,
                ref_return,
            } => Ty::GenericFunc {
                decls: decls.clone(),
                params: params.iter().map(|p| self.resolve_assoc_ty(p)).collect(),
                names: names.clone(),
                ret: Box::new(self.resolve_assoc_ty(ret)),
                required: required.clone(),
                variadic: variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                kw_variadic: kw_variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                positional_only: *positional_only,
                keyword_only: *keyword_only,
                raises: *raises,
                error: error
                    .as_ref()
                    .map(|error| Box::new(self.resolve_assoc_ty(error))),
                conventions: conventions.clone(),
                ref_params: ref_params.clone(),
                ref_return: ref_return.clone(),
            },
            Ty::Overload(candidates) => Ty::Overload(
                candidates
                    .iter()
                    .map(|candidate| self.resolve_assoc_ty(candidate))
                    .collect(),
            ),
            _ => ty.clone(),
        }
    }

    /// Resolve one supplied parameter argument against its declared parameter: a
    /// type parameter takes a type (bound-checked); a value parameter takes a
    /// comptime `Int`. A lone-identifier value argument is reinterpreted as a
    /// type when the parameter is a type parameter.
    fn resolve_param_arg(
        &self,
        decl: &ParamDecl,
        arg: &crate::ast::ParamArg,
    ) -> Result<TyArg, TypeError> {
        use crate::ast::ParamArg;
        match decl {
            ParamDecl::Type { name, bounds, .. } => {
                let ty = match arg {
                    ParamArg::Type(t) => self.ty_from_anno(t)?,
                    ParamArg::Value(Expr {
                        kind: ExprKind::Identifier(id),
                        ..
                    }) => self.ty_from_anno(&SourceType::Named(id.clone(), vec![]))?,
                    ParamArg::Value(_) => {
                        return Err(TypeError::TypeMismatch {
                            expected: "a type".to_string(),
                            found: "a value".to_string(),
                            context: format!("type parameter '{}'", name),
                        });
                    }
                    ParamArg::Named { value, .. } => {
                        return self.resolve_param_arg(decl, value);
                    }
                };
                for bound in bounds {
                    if !self.conforms_to(&ty, bound) {
                        return Err(TypeError::TraitNotSatisfied {
                            param: name.clone(),
                            ty: ty.to_string(),
                            trait_name: bound.clone(),
                            reason: self.trait_failure_reason(&ty, bound),
                        });
                    }
                }
                Ok(TyArg::Ty(ty))
            }
            ParamDecl::Value { name, ty, .. } => match arg {
                ParamArg::Value(expr) => {
                    let value = self.eval_associated_ct(expr, &HashMap::new())?;
                    let actual =
                        self.ct_value_ty(&value, ty)
                            .ok_or_else(|| TypeError::TypeMismatch {
                                expected: ty.to_string(),
                                found: "a non-materializable compile-time value".to_string(),
                                context: format!("value parameter '{}'", name),
                            })?;
                    if !coerces(&actual, ty) {
                        return Err(TypeError::TypeMismatch {
                            expected: ty.to_string(),
                            found: actual.to_string(),
                            context: format!("value parameter '{}'", name),
                        });
                    }
                    Ok(TyArg::Val(value))
                }
                ParamArg::Type(_) => Err(TypeError::TypeMismatch {
                    expected: "a value".to_string(),
                    found: "a type".to_string(),
                    context: format!("value parameter '{}'", name),
                }),
                ParamArg::Named { value, .. } => self.resolve_param_arg(decl, value),
            },
        }
    }

    /// Resolve `List[T]` from its single type argument.
    fn list_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        if args.len() != 1 {
            return Err(TypeError::WrongTypeArgCount {
                name: "List".to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        match &args[0] {
            crate::ast::ParamArg::Type(t) => Ok(Ty::List(Box::new(self.ty_from_anno(t)?))),
            // A bare-identifier arg is reinterpreted as a type (as elsewhere).
            crate::ast::ParamArg::Value(Expr {
                kind: ExprKind::Identifier(id),
                ..
            }) => Ok(Ty::List(Box::new(
                self.ty_from_anno(&SourceType::Named(id.clone(), vec![]))?,
            ))),
            crate::ast::ParamArg::Value(_) => Err(TypeError::TypeMismatch {
                expected: "a type".to_string(),
                found: "a value".to_string(),
                context: "List element type".to_string(),
            }),
            crate::ast::ParamArg::Named { .. } => Err(TypeError::TypeMismatch {
                expected: "a positional type argument".to_string(),
                found: "a named argument".to_string(),
                context: "List element type".to_string(),
            }),
        }
    }

    fn collection_type_argument(
        &self,
        collection: &str,
        argument: &crate::ast::ParamArg,
    ) -> Result<Ty, TypeError> {
        match argument {
            crate::ast::ParamArg::Type(ty) => self.ty_from_anno(ty),
            crate::ast::ParamArg::Value(Expr {
                kind: ExprKind::Identifier(name),
                ..
            }) => self.ty_from_anno(&SourceType::Named(name.clone(), Vec::new())),
            crate::ast::ParamArg::Value(_) => Err(TypeError::TypeMismatch {
                expected: "a type".to_string(),
                found: "a value".to_string(),
                context: format!("{collection} type argument"),
            }),
            crate::ast::ParamArg::Named { .. } => Err(TypeError::TypeMismatch {
                expected: "a positional type argument".to_string(),
                found: "a named argument".to_string(),
                context: format!("{collection} type argument"),
            }),
        }
    }

    fn set_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        if args.len() != 1 {
            return Err(TypeError::WrongTypeArgCount {
                name: "Set".to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        Ok(Ty::Set(Box::new(
            self.collection_type_argument("Set", &args[0])?,
        )))
    }

    fn dict_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        if args.len() != 2 {
            return Err(TypeError::WrongTypeArgCount {
                name: "Dict".to_string(),
                expected: 2,
                got: args.len(),
            });
        }
        Ok(Ty::Dict(
            Box::new(self.collection_type_argument("Dict", &args[0])?),
            Box::new(self.collection_type_argument("Dict", &args[1])?),
        ))
    }

    /// Resolve Mojito's legacy `UnsafePointer[T]` spelling or current Mojo's
    /// origin-bearing `UnsafePointer[T, origin]`.  The inferred mutability
    /// parameter is intentionally absent from the user-facing argument list.
    fn pointer_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        if !matches!(args.len(), 1 | 2) {
            return Err(TypeError::WrongTypeArgCount {
                name: "UnsafePointer".to_string(),
                expected: 2,
                got: args.len(),
            });
        }
        let elem = match &args[0] {
            crate::ast::ParamArg::Type(t) => self.ty_from_anno(t)?,
            crate::ast::ParamArg::Value(Expr {
                kind: ExprKind::Identifier(id),
                ..
            }) => self.ty_from_anno(&SourceType::Named(id.clone(), vec![]))?,
            crate::ast::ParamArg::Value(_) => {
                return Err(TypeError::TypeMismatch {
                    expected: "a type".to_string(),
                    found: "a value".to_string(),
                    context: "UnsafePointer element type".to_string(),
                });
            }
            crate::ast::ParamArg::Named { .. } => {
                return Err(TypeError::Unsupported(
                    "named Tuple element arguments".to_string(),
                ));
            }
        };
        let origin = if args.len() == 1 {
            crate::origin::PointerOrigin::Legacy
        } else {
            self.pointer_origin_arg(&args[1])?
        };
        Ok(Ty::Pointer {
            element: Box::new(elem),
            origin,
        })
    }

    fn pointer_origin_arg(
        &self,
        argument: &crate::ast::ParamArg,
    ) -> Result<crate::origin::PointerOrigin, TypeError> {
        use crate::origin::{Mutability, OriginParamId, PointerOrigin};

        let constant = match argument {
            crate::ast::ParamArg::Type(SourceType::SelfParam(name)) => {
                let (index, parameter) = self
                    .enclosing_type_params
                    .iter()
                    .enumerate()
                    .find(|(_, parameter)| {
                        parameter.name == *name && parameter.bounds.as_slice() == ["Origin"]
                    })
                    .ok_or_else(|| TypeError::UnknownSelfParam(name.clone()))?;
                let id = OriginParamId(index as u32);
                let mutability = match parameter.origin_mutability.as_ref().map(|e| &e.kind) {
                    Some(ExprKind::Bool(true)) => Mutability::Mutable,
                    Some(ExprKind::Bool(false)) => Mutability::Immutable,
                    _ => Mutability::Param(id),
                };
                return Ok(PointerOrigin::Param { id, mutability });
            }
            crate::ast::ParamArg::Value(Expr {
                kind: ExprKind::Identifier(name),
                ..
            }) => name.as_str(),
            crate::ast::ParamArg::Type(SourceType::Named(name, arguments))
                if arguments.is_empty() =>
            {
                name.as_str()
            }
            _ => {
                return Err(TypeError::TypeMismatch {
                    expected: "Self.origin or a concrete Origin value".to_string(),
                    found: "a non-origin parameter argument".to_string(),
                    context: "UnsafePointer origin".to_string(),
                });
            }
        };
        match constant {
            "MutUntrackedOrigin" => Ok(PointerOrigin::Untracked { mutable: true }),
            "ImmutUntrackedOrigin" => Ok(PointerOrigin::Untracked { mutable: false }),
            "MutUnsafeAnyOrigin" => Ok(PointerOrigin::UnsafeAny { mutable: true }),
            "ImmutUnsafeAnyOrigin" => Ok(PointerOrigin::UnsafeAny { mutable: false }),
            "StaticConstantOrigin" => Ok(PointerOrigin::Static),
            name => Err(TypeError::UndefinedVariable(name.to_string())),
        }
    }

    /// Resolve `Tuple[T1, …, Tn]` from its type arguments (each a type).
    fn tuple_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        let mut elems = Vec::with_capacity(args.len());
        for arg in args {
            elems.push(match arg {
                crate::ast::ParamArg::Type(t) => self.ty_from_anno(t)?,
                // A bare-identifier arg is reinterpreted as a type (as elsewhere).
                crate::ast::ParamArg::Value(Expr {
                    kind: ExprKind::Identifier(id),
                    ..
                }) => self.ty_from_anno(&SourceType::Named(id.clone(), vec![]))?,
                crate::ast::ParamArg::Value(_) => {
                    return Err(TypeError::TypeMismatch {
                        expected: "a type".to_string(),
                        found: "a value".to_string(),
                        context: "Tuple element type".to_string(),
                    });
                }
                crate::ast::ParamArg::Named { .. } => {
                    return Err(TypeError::Unsupported(
                        "named Tuple element arguments".to_string(),
                    ));
                }
            });
        }
        Ok(Ty::Tuple(elems))
    }

    /// Resolve the alternatives of `Variant[T1, ..., Tn]`.  Alternative order
    /// is significant because it becomes the runtime tag; duplicate types would
    /// make `isa[T]` and `value[T]` ambiguous and are rejected here.
    fn variant_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        if args.is_empty() {
            return Err(TypeError::WrongTypeArgCount {
                name: "Variant".to_string(),
                expected: 1,
                got: 0,
            });
        }
        let mut alternatives = Vec::with_capacity(args.len());
        for arg in args {
            let alternative = self.type_param_argument(arg, "Variant alternative")?;
            if alternatives.contains(&alternative) {
                return Err(TypeError::Unsupported(format!(
                    "Variant contains duplicate alternative '{alternative}'"
                )));
            }
            alternatives.push(alternative);
        }
        Ok(Ty::Variant(alternatives))
    }

    fn type_param_argument(
        &self,
        arg: &crate::ast::ParamArg,
        context: &str,
    ) -> Result<Ty, TypeError> {
        match arg {
            crate::ast::ParamArg::Type(ty) => self.ty_from_anno(ty),
            crate::ast::ParamArg::Value(Expr {
                kind: ExprKind::Identifier(name),
                ..
            }) => self.ty_from_anno(&SourceType::Named(name.clone(), Vec::new())),
            crate::ast::ParamArg::Value(_) => Err(TypeError::TypeMismatch {
                expected: "a type".to_string(),
                found: "a value".to_string(),
                context: context.to_string(),
            }),
            crate::ast::ParamArg::Named { .. } => Err(TypeError::Unsupported(format!(
                "named arguments are not supported in {context}"
            ))),
        }
    }

    /// Resolve `SIMD[DType.<dt>, width]` from its two parameter arguments to its
    /// `(dtype, width)` (raw — not canonicalized).
    fn simd_dims(&self, args: &[crate::ast::ParamArg]) -> Result<(Dtype, i64), TypeError> {
        if args.len() != 2 {
            return Err(TypeError::WrongTypeArgCount {
                name: "SIMD".to_string(),
                expected: 2,
                got: args.len(),
            });
        }
        let dtype = dtype_from_arg(&args[0])?;
        let width = if matches!(
            &args[1],
            crate::ast::ParamArg::Value(Expr { kind: ExprKind::Identifier(name), .. }) if name == "_"
        ) {
            -1
        } else {
            self.simd_width(&args[1])?
        };
        Ok((dtype, width))
    }

    /// The (canonicalized) `Ty` for `SIMD[DType.<dt>, width]` — a width-1 `float64`
    /// resolves to `Ty::Float64` (the unification).
    fn simd_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        let (dtype, width) = self.simd_dims(args)?;
        Ok(simd_ty(dtype, width))
    }

    /// Evaluate a SIMD width argument: a comptime `Int` that is a power of two.
    fn simd_width(&self, arg: &crate::ast::ParamArg) -> Result<i64, TypeError> {
        let w = match arg {
            crate::ast::ParamArg::Value(expr) => self.eval_ct(expr)?,
            crate::ast::ParamArg::Type(_) => {
                return Err(TypeError::BadSimdWidth("a type".to_string()));
            }
            crate::ast::ParamArg::Named { .. } => {
                return Err(TypeError::BadSimdWidth("a named argument".to_string()));
            }
        };
        if w >= 1 && (w & (w - 1)) == 0 {
            Ok(w)
        } else {
            Err(TypeError::BadSimdWidth(w.to_string()))
        }
    }

    /// If `name` is a generic type parameter currently in scope (in a `def`'s
    /// `[type_params]`), return its trait bounds.
    fn lookup_tparam(&self, name: &str) -> Option<Vec<String>> {
        self.tparams
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned()
    }

    /// Evaluate a compile-time `Int` expression: literals, `comptime` constants,
    /// and `+ - * // % **` / unary `-`. Rejects anything non-comptime (a value
    /// parameter, a call, a non-`Int` operation).
    fn eval_ct(&self, expr: &Expr) -> Result<i64, TypeError> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(*n),
            ExprKind::Identifier(name) => self
                .comptimes
                .get(name)
                .copied()
                .ok_or_else(|| TypeError::NotComptime(name.clone())),
            ExprKind::Prefix(PrefixOp::Neg, e) => Ok(-self.eval_ct(e)?),
            ExprKind::Infix(op, l, r) => {
                let (a, b) = (self.eval_ct(l)?, self.eval_ct(r)?);
                match op {
                    InfixOp::Add => Ok(a + b),
                    InfixOp::Sub => Ok(a - b),
                    InfixOp::Mul => Ok(a * b),
                    InfixOp::FloorDiv if b != 0 => Ok(a.div_euclid(b)),
                    InfixOp::Mod if b != 0 => Ok(a.rem_euclid(b)),
                    InfixOp::Pow if b >= 0 => Ok(a.pow(b as u32)),
                    _ => Err(TypeError::NotComptime(
                        "unsupported comptime operation".to_string(),
                    )),
                }
            }
            _ => Err(TypeError::NotComptime(
                "not a comptime Int expression".to_string(),
            )),
        }
    }

    /// Classify a trait comptime-member annotation. In Mojo terms,
    /// `comptime count: Int` requires an integer compile-time value, while
    /// `comptime Element: AnyType` requires a type-valued member whose type
    /// conforms to `AnyType`.
    fn ct_member_req_from_anno(&self, ty: &SourceType) -> Result<CtMemberReq, TypeError> {
        if let SourceType::Named(name, args) = ty
            && name == "$trait_composition"
        {
            let mut bounds = Vec::with_capacity(args.len());
            for argument in args {
                let crate::ast::ParamArg::Type(SourceType::Named(bound, bound_args)) = argument
                else {
                    return Err(TypeError::Unsupported(
                        "associated type bounds must be trait names".to_string(),
                    ));
                };
                if !bound_args.is_empty() {
                    return Err(TypeError::Unsupported(
                        "associated type bounds cannot take arguments".to_string(),
                    ));
                }
                self.check_trait_name(bound)?;
                if !bounds.contains(bound) {
                    bounds.push(bound.clone());
                }
            }
            return Ok(CtMemberReq::Type { bounds });
        }
        if let SourceType::Named(name, args) = ty
            && args.is_empty()
            && (BUILTIN_TRAITS.contains(&name.as_str()) || self.traits.contains_key(name))
        {
            self.check_trait_name(name)?;
            return Ok(CtMemberReq::Type {
                bounds: vec![name.clone()],
            });
        }
        Ok(CtMemberReq::Value(self.ty_from_anno(ty)?))
    }

    fn check_struct_associated(
        &self,
        associated: &[StructComptime],
    ) -> Result<HashMap<String, CtValue>, TypeError> {
        let mut out = HashMap::new();
        for member in associated {
            if out.contains_key(&member.name) {
                return Err(TypeError::Redeclaration(member.name.clone()));
            }
            let value = self.eval_associated_ct(&member.value, &out)?;
            out.insert(member.name.clone(), value);
        }
        Ok(out)
    }

    /// Evaluate a struct-level associated comptime value. This intentionally
    /// accepts type-valued expressions in addition to runtime-materializable
    /// constants because associated facts are type metadata, not executable code.
    fn eval_associated_ct(
        &self,
        expr: &Expr,
        associated: &HashMap<String, CtValue>,
    ) -> Result<CtValue, TypeError> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(CtValue::Int(*n)),
            ExprKind::Float(value) => Ok(CtValue::Float(value.to_bits())),
            ExprKind::Bool(b) => Ok(CtValue::Bool(*b)),
            ExprKind::Str(s) => Ok(CtValue::Str(s.clone())),
            ExprKind::Identifier(name) => {
                if let Some(n) = self.comptimes.get(name) {
                    return Ok(CtValue::Int(*n));
                }
                self.ty_value_from_name(name, &[])
                    .ok_or_else(|| TypeError::NotComptime(name.clone()))
            }
            ExprKind::TypeApply { name, args } => self
                .ty_value_from_name(name, args)
                .ok_or_else(|| TypeError::NotComptime(name.clone())),
            ExprKind::Member { object, field } => {
                if let ExprKind::Identifier(s) = &object.kind
                    && s == "Self"
                {
                    if let Some(value) = self.self_param_ct_value(field) {
                        return Ok(value);
                    }
                    if let Some(value) = associated.get(field) {
                        return Ok(value.clone());
                    }
                    return Err(TypeError::UnknownSelfParam(field.clone()));
                }
                Err(TypeError::NotComptime(
                    "unsupported associated comptime member access".to_string(),
                ))
            }
            ExprKind::Prefix(PrefixOp::Neg, e) => {
                let CtValue::Int(n) = self.eval_associated_ct(e, associated)? else {
                    return Err(TypeError::NotComptime(
                        "unary '-' expects a comptime Int".to_string(),
                    ));
                };
                Ok(CtValue::Int(-n))
            }
            ExprKind::Infix(op, l, r) => {
                let (CtValue::Int(a), CtValue::Int(b)) = (
                    self.eval_associated_ct(l, associated)?,
                    self.eval_associated_ct(r, associated)?,
                ) else {
                    return Err(TypeError::NotComptime(
                        "integer comptime operation expects Int operands".to_string(),
                    ));
                };
                let value = match op {
                    InfixOp::Add => a + b,
                    InfixOp::Sub => a - b,
                    InfixOp::Mul => a * b,
                    InfixOp::FloorDiv if b != 0 => a.div_euclid(b),
                    InfixOp::Mod if b != 0 => a.rem_euclid(b),
                    InfixOp::Pow if b >= 0 => a.pow(b as u32),
                    _ => {
                        return Err(TypeError::NotComptime(
                            "unsupported associated comptime operation".to_string(),
                        ));
                    }
                };
                Ok(CtValue::Int(value))
            }
            ExprKind::TupleLit(elems) => elems
                .iter()
                .map(|e| self.eval_associated_ct(e, associated))
                .collect::<Result<Vec<_>, _>>()
                .map(CtValue::Tuple),
            ExprKind::ListLit(elems) => elems
                .iter()
                .map(|e| self.eval_associated_ct(e, associated))
                .collect::<Result<Vec<_>, _>>()
                .map(CtValue::List),
            _ => Err(TypeError::NotComptime(
                "not an associated comptime expression".to_string(),
            )),
        }
    }

    fn self_param_ct_value(&self, name: &str) -> Option<CtValue> {
        self.self_decls.iter().find_map(|decl| match decl {
            ParamDecl::Type {
                name: n, bounds, ..
            } if n == name => Some(CtValue::Type(Box::new(Ty::Param {
                name: n.clone(),
                bounds: bounds.clone(),
            }))),
            ParamDecl::Value { name: n, .. } if n == name => Some(CtValue::Param(n.clone())),
            _ => None,
        })
    }

    fn ty_value_from_name(&self, name: &str, args: &[crate::ast::ParamArg]) -> Option<CtValue> {
        if args.is_empty() {
            if let Some(ty) = scalar_type_name(name) {
                return Some(CtValue::Type(Box::new(ty)));
            }
            if name == "None" {
                return Some(CtValue::Type(Box::new(Ty::None)));
            }
        }
        self.ty_from_anno(&SourceType::Named(name.to_string(), args.to_vec()))
            .ok()
            .map(|ty| CtValue::Type(Box::new(ty)))
    }

    pub fn check_program(&mut self, stmts: &[Stmt]) -> Result<(), TypeError> {
        // `ret = None` marks "not inside a function", so a top-level `return`
        // is rejected; `in_loop = false` likewise rejects a top-level `break`.
        self.check_block(stmts, None, false)
    }

    /// Check the statements of a block in the current scope. `ret` is the
    /// enclosing function's declared return type (or `None` at module level);
    /// `in_loop` is true inside a `while`/`for` body (gating `break`/`continue`).
    fn check_block(
        &mut self,
        stmts: &[Stmt],
        ret: Option<&Ty>,
        in_loop: bool,
    ) -> Result<(), TypeError> {
        for stmt in stmts {
            self.check_stmt(stmt, ret, in_loop)?;
        }
        Ok(())
    }

    /// Check a block in a fresh nested scope (the body of an `if`/`elif`/`else`
    /// or loop). The new scope is popped before returning.
    fn check_scoped_block(
        &mut self,
        stmts: &[Stmt],
        ret: Option<&Ty>,
        in_loop: bool,
    ) -> Result<(), TypeError> {
        self.push_scope();
        let result = self.check_block(stmts, ret, in_loop);
        self.pop_scope();
        result
    }

    fn check_stmt(
        &mut self,
        stmt: &Stmt,
        ret: Option<&Ty>,
        in_loop: bool,
    ) -> Result<(), TypeError> {
        match &stmt.kind {
            StmtKind::RefDecl { name, value } => {
                if let Ty::Ref(reference) = self.infer(value)? {
                    let mutable = reference.mutability == crate::origin::Mutability::Mutable;
                    return self.declare_with_mutability(name, Ty::Ref(reference), mutable);
                }
                let place = self.origin_place(value)?;
                let referent = self.infer(value)?;
                let mutable = self.owner_is_mutable(place.root);
                self.declare_with_mutability(
                    name,
                    Ty::Ref(crate::origin::RefTy {
                        referent: Box::new(referent),
                        origin: crate::origin::Origin::Place(place),
                        mutability: if mutable {
                            crate::origin::Mutability::Mutable
                        } else {
                            crate::origin::Mutability::Immutable
                        },
                    }),
                    mutable,
                )
            }
            StmtKind::VarDecl { name, ty, value } => {
                if matches!(value.kind, ExprKind::Uninitialized) {
                    let Some(annotation) = ty else {
                        return Err(TypeError::Unsupported(
                            "an uninitialized variable requires a type annotation".to_string(),
                        ));
                    };
                    let declared = self.ty_from_anno(annotation)?;
                    self.declare(name, declared)?;
                    if let Some(owner) = self.lookup_owner(name) {
                        self.uninitialized.borrow_mut().insert(owner);
                    }
                    return Ok(());
                }
                self.register_named_bindings(value)?;
                let contextual = ty
                    .as_ref()
                    .map(|annotation| self.ty_from_anno(annotation))
                    .transpose()?;
                let found = match contextual.as_ref() {
                    Some(expected) => self.infer_with_expected(value, expected, true)?,
                    None => self.infer(value)?,
                };
                self.check_consuming(value, &found, &format!("variable '{name}'"))?;
                let declared = match ty {
                    // Annotated: the value must coerce to the annotation.
                    Some(anno) => {
                        let expected = contextual.clone().unwrap_or(self.ty_from_anno(anno)?);
                        if !self.record_implicit_conversion(value, &found, &expected)? {
                            return Err(TypeError::TypeMismatch {
                                expected: expected.to_string(),
                                found: found.to_string(),
                                context: format!("variable '{}'", name),
                            });
                        }
                        expected
                    }
                    // Inferred `var x = e`: declare the value's materialized type.
                    None => self.inferred_binding_ty(&found, name)?,
                };
                self.binding_types
                    .borrow_mut()
                    .insert(value.source_span(), declared.clone());
                let aggregate_origins =
                    if !matches!(declared, Ty::Ref(_)) && self.type_contains_reference(&declared) {
                        self.aggregate_origins(value)
                    } else {
                        Vec::new()
                    };
                self.declare(name, declared)?;
                self.set_aggregate_origins(name, aggregate_origins);
                Ok(())
            }

            StmtKind::Assign { name, value } => {
                self.register_named_bindings(value)?;
                self.check_capture_access(name, true)?;
                let found = self.infer(value)?;
                self.check_consuming(value, &found, &format!("assignment to '{name}'"))?;
                // Mojo treats a bare assignment in a function as a local
                // introduction unless that name is already local to this
                // function. Its initializer may still read an outer binding.
                let target = if let Some(&base) = self.function_bases.last() {
                    self.scopes[base..]
                        .iter()
                        .rev()
                        .find_map(|s| s.get(name))
                        .cloned()
                        .or_else(|| {
                            // A mutable captured variable is updated by reference;
                            // an immutable capture (notably `comptime`) is instead
                            // shadowed by a new function-local binding.
                            let mutable = self.mutable_scopes[..base]
                                .iter()
                                .rev()
                                .find_map(|s| s.get(name))
                                .copied()
                                .unwrap_or(false);
                            if mutable {
                                self.scopes[..base]
                                    .iter()
                                    .rev()
                                    .find_map(|s| s.get(name))
                                    .cloned()
                            } else {
                                None
                            }
                        })
                } else {
                    self.lookup(name).cloned()
                };
                match target {
                    // Re-assignment: the value must keep the variable's type.
                    Some(target) => {
                        if !self.is_binding_mutable(name) {
                            return Err(TypeError::ImmutableBinding(name.clone()));
                        }
                        let aggregate_origins = if !matches!(target, Ty::Ref(_))
                            && self.type_contains_reference(&target)
                        {
                            self.aggregate_origins(value)
                        } else {
                            Vec::new()
                        };
                        let target = match target {
                            Ty::Ref(reference) => *reference.referent,
                            other => other,
                        };
                        // Assigning a closure could move it to an outer binding.
                        if matches!(
                            found,
                            Ty::Func { .. } | Ty::GenericFunc { .. } | Ty::Overload(_)
                        ) {
                            return Err(TypeError::ClosureEscape);
                        }
                        if !self.record_implicit_conversion(value, &found, &target)? {
                            return Err(TypeError::TypeMismatch {
                                expected: target.to_string(),
                                found: found.to_string(),
                                context: format!("assignment to '{}'", name),
                            });
                        }
                        if let Some(owner) = self.lookup_owner(name) {
                            self.uninitialized.borrow_mut().remove(&owner);
                        }
                        self.set_aggregate_origins(name, aggregate_origins);
                        Ok(())
                    }
                    // `x = e` on an undeclared name is a **var-less introduction**
                    // (implicit declaration). Mojo allows it; mojito parses and
                    // type-checks it by binding the materialized type. Later
                    // lowering retains the explicit unsupported boundary.
                    None => {
                        let declared = self.inferred_binding_ty(&found, name)?;
                        let aggregate_origins = if !matches!(declared, Ty::Ref(_))
                            && self.type_contains_reference(&declared)
                        {
                            self.aggregate_origins(value)
                        } else {
                            Vec::new()
                        };
                        self.declare_function_implicit(name, declared)?;
                        self.set_aggregate_origins(name, aggregate_origins);
                        if let Some(owner) = self.lookup_owner(name) {
                            self.uninitialized.borrow_mut().remove(&owner);
                        }
                        Ok(())
                    }
                }
            }

            StmtKind::AugAssign { place, op, value } => {
                if let Some(root) = place_root_name(place) {
                    self.check_capture_access(root, true)?;
                }
                // `target OP= value` means `target = target OP value`: the place
                // must be writable, and the result of the operator must keep the
                // place's type. Typing `place OP value` reuses `infer_infix`.
                let target = self.check_place(place)?;
                if let Some(Ty::Ref(reference)) = self.place_storage_ty(place)
                    && reference.mutability != crate::origin::Mutability::Mutable
                {
                    return Err(TypeError::ImmutableBinding(
                        "immutable reference field".to_string(),
                    ));
                }
                let result = self.infer_infix(*op, place, value)?;
                if !coerces(&result, &target) {
                    return Err(TypeError::TypeMismatch {
                        expected: target.to_string(),
                        found: result.to_string(),
                        context: "augmented assignment".to_string(),
                    });
                }
                Ok(())
            }

            // Tuple unpacking `a, b = t`: `t` must be a tuple of matching arity; each
            // target (a NAME or a place) receives the corresponding element type. A
            // NAME follows the assignment rules (re-assign if in scope, else a
            // var-less introduction).
            StmtKind::Unpack { targets, value } => {
                let vt = self.infer(value)?;
                let Ty::Tuple(elems) = &vt else {
                    return Err(TypeError::TypeMismatch {
                        expected: "a tuple".to_string(),
                        found: vt.to_string(),
                        context: "tuple unpacking".to_string(),
                    });
                };
                if elems.len() != targets.len() {
                    return Err(TypeError::TypeMismatch {
                        expected: format!("a {}-element tuple", targets.len()),
                        found: vt.to_string(),
                        context: "tuple unpacking".to_string(),
                    });
                }
                let elems = elems.clone();
                for (target, elem) in targets.iter().zip(&elems) {
                    match &target.kind {
                        ExprKind::Identifier(name) => match self.lookup(name).cloned() {
                            Some(existing) => {
                                self.check_capture_access(name, true)?;
                                if !self.is_binding_mutable(name) {
                                    return Err(TypeError::ImmutableBinding(name.clone()));
                                }
                                if !coerces(elem, &existing) {
                                    return Err(TypeError::TypeMismatch {
                                        expected: existing.to_string(),
                                        found: elem.to_string(),
                                        context: format!("unpacking into '{name}'"),
                                    });
                                }
                            }
                            None => {
                                let declared = self.inferred_binding_ty(elem, name)?;
                                self.declare(name, declared)?;
                            }
                        },
                        _ => {
                            if let Some(root) = place_root_name(target) {
                                self.check_capture_access(root, true)?;
                            }
                            let target_ty = self.check_place(target)?;
                            if !coerces(elem, &target_ty) {
                                return Err(TypeError::TypeMismatch {
                                    expected: target_ty.to_string(),
                                    found: elem.to_string(),
                                    context: "unpacking into a place".to_string(),
                                });
                            }
                        }
                    }
                }
                Ok(())
            }

            // `with` blocks parse, but the context-manager (`__enter__`/`__exit__`)
            // protocol is deferred — flagged, like the other parse-only constructs.
            StmtKind::With { .. } => Err(TypeError::Unsupported("with statement".to_string())),

            StmtKind::SetPlace { place, value } => {
                if let Some(root) = place_root_name(place) {
                    self.check_capture_access(root, true)?;
                }
                // The place must be a writable location (a field/index chain
                // rooted at a mutable variable or `mut self`); the value must
                // keep the place's type. A width-1 SIMD target (a lane write, or
                // a scalar-alias field) additionally accepts a splatting literal.
                let target = self.check_place(place)?;
                let storage = self.place_storage_ty(place);
                let found = self.infer(value)?;
                if let Some(Ty::Ref(expected_reference)) = storage {
                    let initializes_reference =
                        self.self_initializing && place_root_name(place) == Some("self");
                    if initializes_reference {
                        let actual = self.infer_reference_value(value).ok_or_else(|| {
                            TypeError::TypeMismatch {
                                expected: format!("ref {}", expected_reference.referent),
                                found: found.to_string(),
                                context: "reference field initialization".to_string(),
                            }
                        })?;
                        if !coerces(&actual.referent, &expected_reference.referent)
                            || (expected_reference.mutability == crate::origin::Mutability::Mutable
                                && actual.mutability != crate::origin::Mutability::Mutable)
                        {
                            return Err(TypeError::TypeMismatch {
                                expected: format!("ref {}", expected_reference.referent),
                                found: format!("ref {}", actual.referent),
                                context: "reference field initialization".to_string(),
                            });
                        }
                        self.reference_value_uses.borrow_mut().insert(
                            value.source_span(),
                            expected_reference.mutability == crate::origin::Mutability::Mutable,
                        );
                    } else if expected_reference.mutability != crate::origin::Mutability::Mutable {
                        return Err(TypeError::ImmutableBinding(
                            "immutable reference field".to_string(),
                        ));
                    }
                }
                let ok = match &target {
                    Ty::Simd { dtype, width: 1 } => splats_to(&found, *dtype),
                    _ => coerces(&found, &target),
                };
                if !ok {
                    return Err(TypeError::TypeMismatch {
                        expected: target.to_string(),
                        found: found.to_string(),
                        context: "assignment target".to_string(),
                    });
                }
                Ok(())
            }

            // `raises` is parsed but its effect is not analyzed (deferred).
            StmtKind::Def {
                name,
                type_params,
                params,
                positional_only,
                keyword_only,
                captures,
                ret: ret_anno,
                body,
                raises,
                raises_type,
                decorators: _,
                where_clause,
            } => {
                if self.structs.contains_key(name) {
                    return Err(TypeError::Redeclaration(name.clone()));
                }
                // Free functions, including generic functions, share one binder
                // for regular, `*args`, and homogeneous `**kwargs` parameters.
                if let Some(feature) = Self::advanced_param_feature(
                    params,
                    *positional_only,
                    *keyword_only,
                    false,
                    false,
                    false,
                ) {
                    return Err(TypeError::Unsupported(feature.to_string()));
                }
                // A `*args` variadic is supported on non-generic functions; any
                // regular parameters after it are keyword-only.
                let variadic_idx = params
                    .iter()
                    .position(|p| p.kind == crate::ast::ParamKind::Variadic);
                let kw_variadic_idx = params
                    .iter()
                    .position(|p| p.kind == crate::ast::ParamKind::KwVariadic);
                // Regular (non-variadic) parameters, over which arity is computed.
                let regular: Vec<&crate::ast::FnParam> = params
                    .iter()
                    .filter(|p| p.kind == crate::ast::ParamKind::Regular)
                    .collect();
                let out_params: Vec<_> = regular
                    .iter()
                    .copied()
                    .filter(|p| matches!(p.convention, Some(crate::ast::ArgConvention::Out)))
                    .collect();
                if out_params.len() > 1 {
                    return Err(TypeError::Unsupported(
                        "multiple named 'out' results".to_string(),
                    ));
                }
                let named_result = out_params.first().copied();
                if named_result.is_some() && ret_anno.is_some() {
                    return Err(TypeError::Unsupported(
                        "a function cannot declare both a named result and '->' return type"
                            .to_string(),
                    ));
                }
                let caller_regular: Vec<_> = regular
                    .iter()
                    .copied()
                    .filter(|p| !matches!(p.convention, Some(crate::ast::ArgConvention::Out)))
                    .collect();
                let pos_only = regular_marker_index(params, *positional_only);
                let kw_only = effective_keyword_only_index(params, *keyword_only, variadic_idx);
                let required = required_mask(&caller_regular, kw_only)?;
                self.validate_origin_signature(type_params, params, None)?;
                let mut decls = self.classify_params(type_params)?;
                if let Some(condition) = where_clause {
                    let constraint = self.compile_generic_constraint(condition)?;
                    let Some(last) = decls.last_mut() else {
                        return Err(TypeError::Unsupported(
                            "a where clause requires compile-time parameters".to_string(),
                        ));
                    };
                    match last {
                        ParamDecl::Type { constraints, .. }
                        | ParamDecl::Value { constraints, .. } => constraints.push(constraint),
                    }
                }
                // Type parameters are in scope while resolving the signature and
                // checking the body (as bare `T`).
                self.tparams.push(type_scope(&decls));

                let signature = (|| {
                    let param_tys = self.param_tys(params)?;
                    let ret_ty = match (ret_anno, named_result) {
                        (Some(SourceType::Ref { referent, .. }), _) => {
                            self.ty_from_anno(referent)?
                        }
                        (Some(t), _) => self.ty_from_anno(t)?,
                        (None, Some(result)) => self.ty_from_anno(&result.ty)?,
                        (None, None) => Ty::None,
                    };
                    Ok::<_, TypeError>((param_tys, ret_ty))
                })();
                let (param_tys, ret_ty) = match signature {
                    Ok(sig) => sig,
                    Err(e) => {
                        self.tparams.pop();
                        return Err(e);
                    }
                };
                let ref_params = lower_ref_param_sigs(type_params, &caller_regular)?;
                let ref_return = match ret_anno {
                    Some(SourceType::Ref { origin, .. }) => Some(lower_ref_sig(
                        origin.as_ref().ok_or_else(|| {
                            TypeError::Unsupported(
                                "reference return requires an origin".to_string(),
                            )
                        })?,
                        type_params,
                        &regular,
                    )?),
                    _ => None,
                };
                for (param, ty) in param_tys.iter().enumerate() {
                    self.declaration_types.borrow_mut().insert(
                        crate::checked::AnnotationSite::FunctionParam {
                            module: stmt.module.clone(),
                            declaration: stmt.span,
                            param,
                        },
                        ty.clone(),
                    );
                }
                // A default value must fit its parameter's type.
                for (p, pty) in params.iter().zip(&param_tys) {
                    if let Some(d) = &p.default {
                        let dty = match self.infer(d) {
                            Ok(t) => t,
                            Err(e) => {
                                self.tparams.pop();
                                return Err(e);
                            }
                        };
                        if !coerces(&dty, pty) {
                            self.tparams.pop();
                            return Err(TypeError::TypeMismatch {
                                expected: pty.to_string(),
                                found: dty.to_string(),
                                context: format!("default value of '{}'", p.name),
                            });
                        }
                    }
                }

                // Bind the function in the enclosing scope before checking its
                // body, so it can call itself (recursion). A generic `def`
                // becomes a `GenericFunc` (its call sites infer/supply parameters).
                let declared_error = self.declared_error(*raises, raises_type.as_ref())?;
                let effect_raises = declared_error.as_ref().is_some_and(|ty| *ty != Ty::Never);
                let fn_ty = if decls.is_empty() {
                    let regular_tys: Vec<Ty> = params
                        .iter()
                        .zip(&param_tys)
                        .filter(|(p, _)| {
                            p.kind == crate::ast::ParamKind::Regular
                                && !matches!(p.convention, Some(crate::ast::ArgConvention::Out))
                        })
                        .map(|(_, ty)| ty.clone())
                        .collect();
                    Ty::Func {
                        params: regular_tys,
                        names: caller_regular.iter().map(|p| p.name.clone()).collect(),
                        ret: Box::new(ret_ty.clone()),
                        required,
                        variadic: variadic_idx.map(|vi| Box::new(param_tys[vi].clone())),
                        kw_variadic: kw_variadic_idx
                            .map(|index| Box::new(param_tys[index].clone())),
                        positional_only: pos_only,
                        keyword_only: kw_only,
                        raises: effect_raises,
                        error: declared_error.clone().map(Box::new),
                        conventions: caller_regular.iter().map(|p| p.convention).collect(),
                        ref_params: Box::new(ref_params.clone()),
                        ref_return: ref_return.clone().map(Box::new),
                    }
                } else {
                    let regular_tys: Vec<Ty> = params
                        .iter()
                        .zip(&param_tys)
                        .filter(|(p, _)| {
                            p.kind == crate::ast::ParamKind::Regular
                                && !matches!(p.convention, Some(crate::ast::ArgConvention::Out))
                        })
                        .map(|(_, ty)| ty.clone())
                        .collect();
                    Ty::GenericFunc {
                        decls: decls.clone(),
                        params: regular_tys,
                        names: caller_regular.iter().map(|p| p.name.clone()).collect(),
                        ret: Box::new(ret_ty.clone()),
                        required,
                        variadic: variadic_idx.map(|vi| Box::new(param_tys[vi].clone())),
                        kw_variadic: kw_variadic_idx
                            .map(|index| Box::new(param_tys[index].clone())),
                        positional_only: pos_only,
                        keyword_only: kw_only,
                        raises: effect_raises,
                        error: declared_error.clone().map(Box::new),
                        conventions: caller_regular.iter().map(|p| p.convention).collect(),
                        ref_params: Box::new(ref_params.clone()),
                        ref_return: ref_return.clone().map(Box::new),
                    }
                };
                if let Err(e) = self.declare(name, fn_ty) {
                    self.tparams.pop();
                    return Err(e);
                }
                let capture_policy = if self.function_bases.is_empty() {
                    if captures.is_some() {
                        self.tparams.pop();
                        return Err(TypeError::Unsupported(
                            "unified capture lists are valid only on nested functions".to_string(),
                        ));
                    }
                    None
                } else {
                    let mut entries = HashMap::new();
                    if let Some(captures) = captures {
                        for capture in &captures.entries {
                            let Some(scope) = self.binding_scope(&capture.name) else {
                                self.tparams.pop();
                                return Err(TypeError::UndefinedVariable(capture.name.clone()));
                            };
                            if scope == 0 {
                                self.tparams.pop();
                                return Err(TypeError::Unsupported(format!(
                                    "module binding '{}' is not a closure capture",
                                    capture.name
                                )));
                            }
                            if capture.kind == crate::ast::CaptureKind::Mut
                                && !self.is_binding_mutable(&capture.name)
                            {
                                self.tparams.pop();
                                return Err(TypeError::ImmutableBinding(capture.name.clone()));
                            }
                            entries.insert(capture.name.clone(), capture.kind);
                        }
                    }
                    Some(CapturePolicy {
                        base: self.scopes.len(),
                        function_name: name.clone(),
                        entries,
                        default_read: captures.as_ref().is_some_and(|list| list.default_read),
                    })
                };
                self.push_scope();
                self.function_bases.push(self.scopes.len() - 1);
                if let Some(policy) = capture_policy {
                    self.capture_contexts.borrow_mut().push(policy);
                }
                self.raising_context.push(declared_error);
                let mut result = Ok(());
                // Value parameters are ordinary `Int` locals in the body.
                for d in &decls {
                    if let ParamDecl::Value { name, ty, .. } = d {
                        result = self.declare_immutable(
                            name.trim_start_matches('*'),
                            if matches!(d, ParamDecl::Value { variadic: true, .. }) {
                                Ty::List(ty.clone())
                            } else {
                                (**ty).clone()
                            },
                        );
                        if result.is_err() {
                            break;
                        }
                    }
                }
                if result.is_ok() {
                    for (param, ty) in params.iter().zip(&param_tys) {
                        // A `*args` parameter is a `List[element]` inside the body;
                        // a regular parameter keeps its declared type.
                        let bind_ty = match param.kind {
                            crate::ast::ParamKind::Variadic => match ty {
                                Ty::Tuple(elements) => Ty::Tuple(elements.clone()),
                                _ => Ty::List(Box::new(ty.clone())),
                            },
                            crate::ast::ParamKind::KwVariadic => self.kwargs_collector_ty(
                                ty.clone(),
                                &format!("keyword collector '{}'", param.name),
                            )?,
                            crate::ast::ParamKind::Regular => ty.clone(),
                        };
                        // Duplicate parameter names are a redeclaration.
                        result = self.declare_with_mutability(
                            &param.name,
                            bind_ty.clone(),
                            param.kind == crate::ast::ParamKind::KwVariadic
                                || matches!(param.convention, Some(crate::ast::ArgConvention::Out))
                                || ref_parameter_is_writable(param, type_params),
                        );
                        if result.is_ok()
                            && matches!(param.convention, Some(crate::ast::ArgConvention::Ref))
                        {
                            self.register_reference_parameter(
                                &param.name,
                                bind_ty.clone(),
                                ref_parameter_is_writable(param, type_params),
                            );
                        }
                        if result.is_ok()
                            && !matches!(bind_ty, Ty::Ref(_))
                            && self.type_contains_reference(&bind_ty)
                            && let Some(owner) = self.lookup_owner(&param.name)
                        {
                            self.set_aggregate_origins(
                                &param.name,
                                vec![crate::origin::Origin::Place(crate::origin::OriginPlace {
                                    root: owner,
                                    path: Vec::new(),
                                })],
                            );
                        }
                        if result.is_err() {
                            break;
                        }
                    }
                }
                // A function body is a fresh loop context: `break`/`continue`
                // do not cross into a nested `def`.
                if result.is_ok() {
                    let owners: Vec<_> = caller_regular
                        .iter()
                        .map(|param| {
                            self.lookup_owner(&param.name)
                                .expect("bound function parameter")
                        })
                        .collect();
                    let base = *self
                        .function_bases
                        .last()
                        .expect("function scope is active");
                    self.aggregate_escape_contexts
                        .push((base, owners.iter().copied().collect()));
                    self.return_ref_contracts.push(
                        ref_return
                            .clone()
                            .map(|signature| (signature, owners, None)),
                    );
                    self.named_result_context.push(named_result.is_some());
                    result = self.check_block(body, Some(&ret_ty), false);
                    self.named_result_context.pop();
                    self.return_ref_contracts.pop();
                    self.aggregate_escape_contexts.pop();
                }
                // A function with a non-`None` return type must return on every
                // path (falling off the end would yield `None`).
                if result.is_ok()
                    && named_result.is_none()
                    && ret_ty != Ty::None
                    && !definitely_returns(body)
                {
                    result = Err(TypeError::MissingReturn(name.clone()));
                }
                if result.is_ok()
                    && let Some(named_result) = named_result
                    && !definitely_initializes_named_result(body, &named_result.name)
                {
                    result = Err(TypeError::MissingReturn(name.clone()));
                }
                self.pop_scope();
                self.function_bases.pop();
                if !self.function_bases.is_empty() {
                    self.capture_contexts.borrow_mut().pop();
                }
                self.raising_context.pop();
                self.tparams.pop();
                result
            }

            StmtKind::Struct {
                name,
                type_params,
                conforms,
                callable_conformance,
                conformance_conditions,
                fields,
                associated,
                methods,
                fieldwise_init,
                decorators,
            } => {
                if self.lookup(name).is_some() {
                    return Err(TypeError::Redeclaration(name.clone()));
                }
                self.check_struct(&StructDeclaration {
                    module: &stmt.module,
                    span: stmt.span,
                    name,
                    type_params,
                    conforms,
                    callable_conformance,
                    conformance_conditions,
                    fields,
                    associated,
                    methods,
                    fieldwise_init: *fieldwise_init,
                    decorators,
                })
            }

            StmtKind::Trait {
                name,
                refines,
                methods,
                comptime_members,
            } => self.check_trait(name, refines, methods, comptime_members),

            StmtKind::Comptime { name, value } => {
                // A comptime `Int` is recorded (for value-parameter use) and bound as
                // `Int`. A richer comptime value (tuple/list/string) the `Int` folder
                // can't evaluate is still an ordinary binding — the elaborator has
                // already consumed it for any `comptime for`/`comptime if`.
                match self.eval_ct(value) {
                    Ok(v) => {
                        self.comptimes.insert(name.clone(), v);
                        self.declare_immutable(name, Ty::Int)
                    }
                    Err(_) => {
                        let ty = self.infer(value)?;
                        let declared = self.inferred_binding_ty(&ty, name)?;
                        self.declare_immutable(name, declared)
                    }
                }
            }

            // `comptime if` / `comptime for` parse and are grammar-documented, but
            // compile-time branch selection / loop unrolling is deferred — flagged
            // here, like the other syntax-first parse-only constructs.
            StmtKind::ComptimeIf { .. } => Err(TypeError::Unsupported("comptime if".to_string())),
            StmtKind::ComptimeFor { .. } => Err(TypeError::Unsupported("comptime for".to_string())),

            StmtKind::If { branches, orelse } => {
                for (_, body) in branches {
                    self.predeclare_implicit_assignments(body)?;
                }
                if let Some(body) = orelse {
                    self.predeclare_implicit_assignments(body)?;
                }
                let before = self.uninitialized.borrow().clone();
                let mut exits = Vec::new();
                // Definite initialization follows only reachable exits when a
                // condition is a compile-time Bool literal. We still check every
                // source branch for type errors, but `if True: x = ...` establishes
                // a function-scoped implicit binding just as an unconditional
                // assignment does. Unknown conditions retain both the taken and
                // fallthrough possibilities.
                let mut fallthrough_reachable = true;
                for (cond, body) in branches {
                    *self.uninitialized.borrow_mut() = before.clone();
                    self.register_named_bindings(cond)?;
                    self.expect_bool(cond, "if condition")?;
                    self.check_scoped_block(body, ret, in_loop)?;
                    let condition = match &cond.kind {
                        ExprKind::Bool(value) => Some(*value),
                        _ => None,
                    };
                    if fallthrough_reachable && condition != Some(false) {
                        exits.push(self.uninitialized.borrow().clone());
                    }
                    if condition == Some(true) {
                        fallthrough_reachable = false;
                    }
                }
                if let Some(body) = orelse {
                    *self.uninitialized.borrow_mut() = before.clone();
                    self.check_scoped_block(body, ret, in_loop)?;
                    if fallthrough_reachable {
                        exits.push(self.uninitialized.borrow().clone());
                    }
                } else if fallthrough_reachable {
                    exits.push(before);
                }
                *self.uninitialized.borrow_mut() =
                    exits.into_iter().flatten().collect::<HashSet<_>>();
                Ok(())
            }

            StmtKind::While { cond, body, orelse } => {
                self.predeclare_implicit_assignments(body)?;
                let before = self.uninitialized.borrow().clone();
                self.register_named_bindings(cond)?;
                self.expect_bool(cond, "while condition")?;
                self.check_scoped_block(body, ret, true)?;
                *self.uninitialized.borrow_mut() = before;
                if let Some(body) = orelse {
                    self.check_scoped_block(body, ret, in_loop)?;
                }
                Ok(())
            }

            // `raise` — its operand must be an `Error` (or a `String`, the
            // shorthand). The raises *effect* (that this must be in a `raises`
            // function or a `try`) is deliberately not analyzed.
            StmtKind::Raise(expr) => {
                self.register_named_bindings(expr)?;
                let ty = self.infer(expr)?;
                let error = if ty == Ty::String { Ty::Error } else { ty };
                self.require_error("'raise'", error)?;
                Ok(())
            }

            // Imports are parsed but not resolved (no module system yet), so
            // they are a checker no-op — imported names are not made available.
            StmtKind::Import { .. } | StmtKind::FromImport { .. } => Ok(()),

            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                if except.is_some() {
                    self.handled_raise_depth += 1;
                    self.handled_raise_types.borrow_mut().push(Vec::new());
                }
                let body_result = self.check_scoped_block(body, ret, in_loop);
                let handled_types = except.as_ref().map(|_| {
                    self.handled_raise_types
                        .borrow_mut()
                        .pop()
                        .unwrap_or_default()
                });
                if except.is_some() {
                    self.handled_raise_depth -= 1;
                }
                body_result?;
                if let Some((name, ex_body)) = except {
                    self.push_scope();
                    let error = handled_types
                        .as_ref()
                        .and_then(|types| types.first().cloned())
                        .unwrap_or(Ty::Error);
                    if handled_types
                        .as_ref()
                        .is_some_and(|types| types.iter().any(|candidate| *candidate != error))
                    {
                        self.pop_scope();
                        return Err(TypeError::RaiseTypeMismatch {
                            expected: error.to_string(),
                            found: "multiple error types in one try block".to_string(),
                        });
                    }
                    let result = match name {
                        Some(n) => self
                            .declare(n, error)
                            .and_then(|()| self.check_block(ex_body, ret, in_loop)),
                        None => self.check_block(ex_body, ret, in_loop),
                    };
                    self.pop_scope();
                    result?;
                }
                if let Some(body) = orelse {
                    self.check_scoped_block(body, ret, in_loop)?;
                }
                if let Some(body) = finalbody {
                    self.check_scoped_block(body, ret, in_loop)?;
                }
                Ok(())
            }

            StmtKind::For {
                var,
                reference,
                owned,
                iter,
                body,
                orelse,
            } => {
                self.register_named_bindings(iter)?;
                // The loop variable's type comes from the iterable: `Int` for a
                // `range`, the element type for a `List`, or — for a user struct —
                // the element type of its `__iter__()` iterator (`__next__`'s return).
                let iter_ty = self.infer(iter)?;
                let (elem_ty, protocol) = self.iteration_protocol(&iter_ty, *owned)?;
                self.iteration_protocols
                    .borrow_mut()
                    .insert(iter.source_span(), protocol);
                if *owned && !matches!(iter.kind, ExprKind::Transfer(_)) {
                    return Err(TypeError::Unsupported(
                        "owned iteration requires a transferred iterable (`for var item in collection^`)"
                            .to_string(),
                    ));
                }
                if !*owned && matches!(iter.kind, ExprKind::Transfer(_)) {
                    return Err(TypeError::Unsupported(
                        "a transferred iterable requires an explicit `var` loop binding"
                            .to_string(),
                    ));
                }
                if *reference && *owned {
                    return Err(TypeError::Unsupported(
                        "a loop binding cannot be both `ref` and `var`".to_string(),
                    ));
                }
                if !*owned && !*reference && !self.is_copyable(&elem_ty) {
                    return Err(TypeError::NonCopyable {
                        ty: elem_ty.to_string(),
                        context: "immutable iteration; use `for var item in collection^`"
                            .to_string(),
                    });
                }
                if *owned
                    && !self.is_implicitly_deletable(&elem_ty)
                    && block_can_escape_owned_iteration(body, 0)
                {
                    return Err(TypeError::Unsupported(format!(
                        "owned iteration over non-ImplicitlyDeletable '{}' cannot exit early; its residual elements would require explicit destruction",
                        elem_ty
                    )));
                }
                self.push_scope();
                let binding_ty = if *reference {
                    if !matches!(iter_ty, Ty::List(_)) {
                        self.pop_scope();
                        return Err(TypeError::Unsupported(
                            "reference iteration currently requires a List place".to_string(),
                        ));
                    }
                    let mut place = self.origin_place(iter)?;
                    place.path.push(crate::origin::OriginSeg::AnyIndex);
                    Ty::Ref(crate::origin::RefTy {
                        referent: Box::new(elem_ty),
                        origin: crate::origin::Origin::Place(place.clone()),
                        mutability: if self.owner_is_mutable(place.root) {
                            crate::origin::Mutability::Mutable
                        } else {
                            crate::origin::Mutability::Immutable
                        },
                    })
                } else {
                    elem_ty
                };
                self.binding_types
                    .borrow_mut()
                    .insert(stmt.source_span(), binding_ty.clone());
                let mutable = *owned
                    || !*reference
                    || matches!(&binding_ty, Ty::Ref(reference) if reference.mutability == crate::origin::Mutability::Mutable);
                let result = match self.declare_with_mutability(var, binding_ty, mutable) {
                    Ok(()) => self.check_block(body, ret, true),
                    Err(e) => Err(e),
                };
                self.pop_scope();
                result?;
                if let Some(body) = orelse {
                    self.check_scoped_block(body, ret, in_loop)?;
                }
                Ok(())
            }

            StmtKind::Break => {
                if in_loop {
                    Ok(())
                } else {
                    Err(TypeError::BreakOutsideLoop)
                }
            }

            StmtKind::Continue => {
                if in_loop {
                    Ok(())
                } else {
                    Err(TypeError::ContinueOutsideLoop)
                }
            }

            StmtKind::Return(expr) => {
                if let Some(expression) = expr {
                    self.register_named_bindings(expression)?;
                }
                let expected = match ret {
                    Some(ty) => ty,
                    None => return Err(TypeError::ReturnOutsideFunction),
                };
                let found = match expr {
                    Some(e) => self.infer_with_expected(e, expected, true)?,
                    None if self.named_result_context.last() == Some(&true) => expected.clone(),
                    None => Ty::None,
                };
                if let Some(expression) = expr
                    && !matches!(expected, Ty::Ref(_))
                    && self.type_contains_reference(expected)
                    && self
                        .aggregate_origins(expression)
                        .iter()
                        .any(|origin| self.aggregate_origin_escapes(origin))
                {
                    return Err(TypeError::ReturnsReferenceToLocal);
                }
                if let (Some(e), Some(Some((signature, parameter_owners, self_owner)))) =
                    (expr, self.return_ref_contracts.last())
                {
                    let actual = match &e.kind {
                        ExprKind::Identifier(name) => match self.lookup(name) {
                            Some(Ty::Ref(reference)) => reference.origin.clone(),
                            _ => crate::origin::Origin::Place(self.origin_place(e)?),
                        },
                        _ => crate::origin::Origin::Place(self.origin_place(e)?),
                    };
                    let parameter_origins: Vec<_> = parameter_owners
                        .iter()
                        .map(|owner| {
                            Some(crate::origin::Origin::Place(crate::origin::OriginPlace {
                                root: *owner,
                                path: Vec::new(),
                            }))
                        })
                        .collect();
                    let allowed = substitute_sig_origin_with_self(
                        &signature.origin,
                        &parameter_origins,
                        *self_owner,
                    );
                    if !origin_is_within(&actual, &allowed) {
                        return Err(TypeError::ReturnsReferenceToLocal);
                    }
                }
                if let Some(e) = expr {
                    self.check_consuming(e, &found, "return value")?;
                }
                // Returning a function value is an escape regardless of the
                // declared return type; Mojito supports downward funargs only.
                if matches!(
                    found,
                    Ty::Func { .. } | Ty::GenericFunc { .. } | Ty::Overload(_)
                ) {
                    return Err(TypeError::ClosureEscape);
                }
                let compatible = match expr {
                    Some(expression) => {
                        self.record_implicit_conversion(expression, &found, expected)?
                    }
                    None => self.value_coerces(&found, expected),
                };
                if !compatible {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: found.to_string(),
                        context: "return".to_string(),
                    });
                }
                Ok(())
            }

            StmtKind::Pass => Ok(()),

            StmtKind::Expr(expr) => {
                self.register_named_bindings(expr)?;
                self.infer(expr)?;
                Ok(())
            }
        }
    }

    /// Resolve a parameter/field list to its types.
    fn param_tys(&self, params: &[crate::ast::FnParam]) -> Result<Vec<Ty>, TypeError> {
        params.iter().map(|p| self.ty_from_anno(&p.ty)).collect()
    }

    fn method_sig(
        &self,
        method: &Method,
        decls: Vec<ParamDecl>,
        all_types: &[Ty],
    ) -> Result<MethodSig, TypeError> {
        let error = self.declared_error(method.raises, method.raises_type.as_ref())?;
        let variadic_idx = method
            .params
            .iter()
            .position(|p| p.kind == crate::ast::ParamKind::Variadic);
        let kw_variadic_idx = method
            .params
            .iter()
            .position(|p| p.kind == crate::ast::ParamKind::KwVariadic);
        let regular: Vec<_> = method
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == crate::ast::ParamKind::Regular)
            .collect();
        let keyword_only =
            effective_keyword_only_index(&method.params, method.keyword_only, variadic_idx);
        let regular_params: Vec<&FnParam> = regular.iter().map(|(_, param)| *param).collect();
        Ok(MethodSig {
            decls,
            availability: method
                .where_clause
                .as_ref()
                .map(|condition| self.compile_generic_constraint(condition))
                .transpose()?
                .into_iter()
                .collect(),
            has_self: method.has_self,
            params: regular
                .iter()
                .map(|(index, _)| all_types[*index].clone())
                .collect(),
            names: regular.iter().map(|(_, p)| p.name.clone()).collect(),
            required: required_mask(
                &regular.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
                keyword_only,
            )?,
            variadic: variadic_idx.map(|index| Box::new(all_types[index].clone())),
            variadic_index: regular_marker_index(&method.params, variadic_idx),
            kw_variadic: kw_variadic_idx.map(|index| Box::new(all_types[index].clone())),
            kw_variadic_index: kw_variadic_idx,
            positional_only: regular_marker_index(&method.params, method.positional_only),
            keyword_only,
            conventions: regular.iter().map(|(_, p)| p.convention).collect(),
            ret: match &method.ret {
                Some(SourceType::Ref { referent, .. }) => self.ty_from_anno(referent)?,
                Some(ret) => self.ty_from_anno(ret)?,
                None => Ty::None,
            },
            raises: error.as_ref().is_some_and(|ty| *ty != Ty::Never),
            error: error.map(Box::new),
            self_convention: method.self_convention,
            ref_params: lower_ref_param_sigs(&self.enclosing_type_params, &regular_params)?,
            ref_return: match &method.ret {
                Some(SourceType::Ref { origin, .. }) => Some(lower_ref_sig(
                    origin.as_ref().ok_or_else(|| {
                        TypeError::Unsupported("reference return requires an origin".to_string())
                    })?,
                    &self.enclosing_type_params,
                    &regular_params,
                )?),
                _ => None,
            },
            implicit: method
                .decorators
                .iter()
                .any(|decorator| decorator.path.len() == 1 && decorator.path[0] == "implicit"),
        })
    }

    /// The name of the first advanced parameter feature used by a signature (a
    /// default value, a `*args`/`**kwargs` variadic, or an argument convention, or
    /// `None` if the signature is supported by this checking path. `/` and bare
    /// `*` markers are modeled by call matching and are not advanced anymore.
    fn advanced_param_feature(
        params: &[crate::ast::FnParam],
        _positional_only: Option<usize>,
        _keyword_only: Option<usize>,
        flag_defaults: bool,
        flag_variadic: bool,
        flag_kw_variadic: bool,
    ) -> Option<&'static str> {
        use crate::ast::ParamKind;
        if flag_defaults && params.iter().any(|p| p.default.is_some()) {
            return Some("default argument values");
        }
        if flag_variadic && params.iter().any(|p| p.kind == ParamKind::Variadic) {
            return Some("variadic '*args' parameters");
        }
        if flag_kw_variadic && params.iter().any(|p| p.kind == ParamKind::KwVariadic) {
            return Some("variadic '**kwargs' parameters");
        }
        None
    }

    /// Classify a `[...]` parameter list into type and value parameters, and
    /// validate them: names must be distinct; a single bound naming a concrete
    /// type is a **value** parameter (must be `Int`); otherwise the bounds must
    /// all name traits (built-in or user), giving a **type** parameter. The
    /// parser guarantees each parameter carries at least one `: bound` (Mojo has
    /// no unconstrained parameters).
    fn classify_params(&self, tps: &[crate::ast::TypeParam]) -> Result<Vec<ParamDecl>, TypeError> {
        let mut decls = Vec::new();
        for tp in tps {
            if decls.iter().any(|d: &ParamDecl| d.name() == tp.name) {
                return Err(TypeError::Redeclaration(tp.name.clone()));
            }
            // Origin parameters are semantic-only and erased before runtime
            // generic argument binding. Their clauses are validated separately
            // by `validate_origin_signature`.
            if tp.bounds.as_slice() == ["Origin"] {
                continue;
            }
            if let Some(value_type) = &tp.value_type {
                let ty = self.ty_from_anno(value_type)?;
                let default = tp
                    .default
                    .as_ref()
                    .map(|expr| self.compile_dependent_ct_expr(expr))
                    .transpose()?;
                decls.push(ParamDecl::Value {
                    name: tp.name.clone(),
                    ty: Box::new(ty),
                    default,
                    infer_only: tp.infer_only,
                    variadic: tp.name.starts_with('*'),
                    constraints: tp
                        .constraints
                        .iter()
                        .map(|condition| self.compile_generic_constraint(condition))
                        .collect::<Result<_, _>>()?,
                });
                continue;
            }
            // A lone bound that names a scalar type marks a value parameter.
            if let [only] = tp.bounds.as_slice()
                && let Some(vty) = scalar_type_name(only)
            {
                if !matches!(
                    vty,
                    Ty::Int | Ty::UInt | Ty::Bool | Ty::String | Ty::Float64
                ) {
                    return Err(TypeError::BadValueParamType {
                        name: tp.name.clone(),
                        ty: only.clone(),
                    });
                }
                decls.push(ParamDecl::Value {
                    name: tp.name.clone(),
                    ty: Box::new(vty),
                    default: tp
                        .default
                        .as_ref()
                        .map(|expr| self.compile_dependent_ct_expr(expr))
                        .transpose()?,
                    infer_only: tp.infer_only,
                    variadic: tp.name.starts_with('*'),
                    constraints: tp
                        .constraints
                        .iter()
                        .map(|condition| self.compile_generic_constraint(condition))
                        .collect::<Result<_, _>>()?,
                });
                continue;
            }
            for bound in &tp.bounds {
                self.check_trait_name(bound)?;
            }
            decls.push(ParamDecl::Type {
                name: tp.name.clone(),
                bounds: tp.bounds.clone(),
                default: tp
                    .default
                    .as_ref()
                    .map(|value| self.type_default_from_expr(value))
                    .transpose()?
                    .map(Box::new),
                infer_only: tp.infer_only,
                variadic: tp.name.starts_with('*'),
                constraints: tp
                    .constraints
                    .iter()
                    .map(|condition| self.compile_generic_constraint(condition))
                    .collect::<Result<_, _>>()?,
            });
        }
        Ok(decls)
    }

    fn type_default_from_expr(&self, value: &Expr) -> Result<Ty, TypeError> {
        match &value.kind {
            ExprKind::Identifier(name) => {
                if let Some(ty) = scalar_type_name(name) {
                    Ok(ty)
                } else {
                    self.ty_from_anno(&SourceType::Named(name.clone(), Vec::new()))
                }
            }
            ExprKind::TypeApply { name, args } => {
                self.ty_from_anno(&SourceType::Named(name.clone(), args.clone()))
            }
            ExprKind::TypeValue(ty) => self.ty_from_anno(ty),
            _ => Err(TypeError::TypeMismatch {
                expected: "a type".to_string(),
                found: "a value".to_string(),
                context: "type parameter default".to_string(),
            }),
        }
    }

    fn compile_dependent_ct_expr(&self, expr: &Expr) -> Result<CtExpr, TypeError> {
        let pair = |left: &Expr, right: &Expr| {
            Ok((
                Box::new(self.compile_dependent_ct_expr(left)?),
                Box::new(self.compile_dependent_ct_expr(right)?),
            ))
        };
        Ok(match &expr.kind {
            ExprKind::Int(value) => CtExpr::Value(CtValue::Int(*value)),
            ExprKind::Float(value) => CtExpr::Value(CtValue::Float(value.to_bits())),
            ExprKind::Bool(value) => CtExpr::Value(CtValue::Bool(*value)),
            ExprKind::Str(value) => CtExpr::Value(CtValue::Str(value.clone())),
            ExprKind::Identifier(name) => {
                if let Some(value) = self.comptimes.get(name) {
                    CtExpr::Value(CtValue::Int(*value))
                } else {
                    CtExpr::Param(name.clone())
                }
            }
            ExprKind::TupleLit(values) => CtExpr::Value(CtValue::Tuple(
                values
                    .iter()
                    .map(|value| self.eval_associated_ct(value, &HashMap::new()))
                    .collect::<Result<_, _>>()?,
            )),
            ExprKind::ListLit(values) => CtExpr::Value(CtValue::List(
                values
                    .iter()
                    .map(|value| self.eval_associated_ct(value, &HashMap::new()))
                    .collect::<Result<_, _>>()?,
            )),
            ExprKind::Prefix(PrefixOp::Neg, value) => {
                CtExpr::Neg(Box::new(self.compile_dependent_ct_expr(value)?))
            }
            ExprKind::Infix(op, left, right) => {
                let (left, right) = pair(left, right)?;
                match op {
                    InfixOp::Add => CtExpr::Add(left, right),
                    InfixOp::Sub => CtExpr::Sub(left, right),
                    InfixOp::Mul => CtExpr::Mul(left, right),
                    InfixOp::FloorDiv => CtExpr::FloorDiv(left, right),
                    InfixOp::Mod => CtExpr::Mod(left, right),
                    InfixOp::Pow => CtExpr::Pow(left, right),
                    _ => {
                        return Err(TypeError::Unsupported(
                            "unsupported dependent parameter expression".to_string(),
                        ));
                    }
                }
            }
            _ => {
                return Err(TypeError::Unsupported(
                    "unsupported dependent parameter expression".to_string(),
                ));
            }
        })
    }

    fn compile_generic_constraint(&self, expr: &Expr) -> Result<GenericConstraint, TypeError> {
        let binary = |left: &Expr, right: &Expr| {
            Ok((
                self.constraint_operand(left)?,
                self.constraint_operand(right)?,
            ))
        };
        Ok(match &expr.kind {
            ExprKind::Bool(value) => GenericConstraint::Bool(*value),
            ExprKind::Prefix(PrefixOp::Not, value) => {
                GenericConstraint::Not(Box::new(self.compile_generic_constraint(value)?))
            }
            ExprKind::Infix(InfixOp::And, left, right) => GenericConstraint::And(
                Box::new(self.compile_generic_constraint(left)?),
                Box::new(self.compile_generic_constraint(right)?),
            ),
            ExprKind::Infix(InfixOp::Or, left, right) => GenericConstraint::Or(
                Box::new(self.compile_generic_constraint(left)?),
                Box::new(self.compile_generic_constraint(right)?),
            ),
            ExprKind::Infix(op, left, right) => {
                let (left, right) = binary(left, right)?;
                match op {
                    InfixOp::Eq => GenericConstraint::Eq(left, right),
                    InfixOp::Ne => GenericConstraint::Ne(left, right),
                    InfixOp::Lt => GenericConstraint::Lt(left, right),
                    InfixOp::Le => GenericConstraint::Le(left, right),
                    InfixOp::Gt => GenericConstraint::Gt(left, right),
                    InfixOp::Ge => GenericConstraint::Ge(left, right),
                    _ => {
                        return Err(TypeError::Unsupported(
                            "unsupported generic where proposition".to_string(),
                        ));
                    }
                }
            }
            ExprKind::Call {
                name, args, kwargs, ..
            } if name == "conforms_to" && kwargs.is_empty() && args.len() == 2 => {
                let (param, pack) = match &args[0].kind {
                    ExprKind::Identifier(param) => (param.clone(), false),
                    ExprKind::Member { object, field } if matches!(&object.kind, ExprKind::Identifier(name) if name == "Self") => {
                        (field.clone(), false)
                    }
                    ExprKind::Member { object, field }
                        if field == "values" && matches!(&object.kind, ExprKind::Identifier(_)) =>
                    {
                        let ExprKind::Identifier(param) = &object.kind else {
                            unreachable!()
                        };
                        (param.clone(), true)
                    }
                    _ => {
                        return Err(TypeError::Unsupported(
                            "conforms_to requires a parameter name".to_string(),
                        ));
                    }
                };
                let ExprKind::Identifier(trait_name) = &args[1].kind else {
                    return Err(TypeError::Unsupported(
                        "conforms_to requires a trait name".to_string(),
                    ));
                };
                self.check_trait_name(trait_name)?;
                if pack {
                    GenericConstraint::ConformsPack {
                        param,
                        trait_name: trait_name.clone(),
                    }
                } else {
                    GenericConstraint::Conforms {
                        param,
                        trait_name: trait_name.clone(),
                    }
                }
            }
            _ => {
                return Err(TypeError::Unsupported(
                    "unsupported generic where proposition".to_string(),
                ));
            }
        })
    }

    fn constraint_operand(&self, expr: &Expr) -> Result<ConstraintOperand, TypeError> {
        Ok(match &expr.kind {
            ExprKind::Identifier(name) => scalar_type_name(name)
                .map(ConstraintOperand::Type)
                .unwrap_or_else(|| ConstraintOperand::Param(name.clone())),
            ExprKind::Member { object, field } if matches!(&object.kind, ExprKind::Identifier(name) if name == "Self") => {
                ConstraintOperand::Param(field.clone())
            }
            ExprKind::Int(value) => ConstraintOperand::Value(CtValue::Int(*value)),
            ExprKind::Bool(value) => ConstraintOperand::Value(CtValue::Bool(*value)),
            ExprKind::Str(value) => ConstraintOperand::Value(CtValue::Str(value.clone())),
            ExprKind::TypeValue(ty) => ConstraintOperand::Type(self.ty_from_anno(ty)?),
            ExprKind::TypeApply { name, args } => ConstraintOperand::Type(
                self.ty_from_anno(&SourceType::Named(name.clone(), args.clone()))?,
            ),
            _ => {
                return Err(TypeError::Unsupported(
                    "unsupported generic constraint operand".to_string(),
                ));
            }
        })
    }

    fn validate_origin_signature(
        &self,
        type_params: &[crate::ast::TypeParam],
        params: &[crate::ast::FnParam],
        self_origin: Option<&crate::ast::OriginSpec>,
    ) -> Result<(), TypeError> {
        let origin_params: HashSet<&str> = type_params
            .iter()
            .filter(|param| param.bounds.as_slice() == ["Origin"])
            .map(|param| param.name.as_str())
            .collect();
        let value_params: HashSet<&str> = params.iter().map(|param| param.name.as_str()).collect();
        let bool_params: HashSet<&str> = type_params
            .iter()
            .filter(|param| param.bounds.as_slice() == ["Bool"])
            .map(|param| param.name.as_str())
            .collect();

        for origin in type_params
            .iter()
            .filter(|param| param.bounds.as_slice() == ["Origin"])
        {
            if let Some(expr) = &origin.origin_mutability
                && !matches!(expr.kind, ExprKind::Bool(_))
                && !matches!(&expr.kind, ExprKind::Identifier(name) if bool_params.contains(name.as_str()))
            {
                return Err(TypeError::Unsupported(format!(
                    "origin mutability for '{}' must be Bool or a Bool parameter",
                    origin.name
                )));
            }
        }

        let validate = |spec: &crate::ast::OriginSpec| {
            for expr in spec {
                validate_origin_expr(expr, &origin_params, &value_params)?;
            }
            Ok::<(), TypeError>(())
        };
        if let Some(spec) = self_origin {
            validate(spec)?;
        }
        for param in params {
            if param.convention != Some(ArgConvention::Ref) && param.origin.is_some() {
                return Err(TypeError::Unsupported(format!(
                    "origin clause on non-ref parameter '{}'",
                    param.name
                )));
            }
            if let Some(spec) = &param.origin {
                validate(spec)?;
            }
        }
        Ok(())
    }

    /// A trait name is valid if it is a built-in or a user trait defined so far.
    fn check_trait_name(&self, name: &str) -> Result<(), TypeError> {
        if BUILTIN_TRAITS.contains(&name) || self.traits.contains_key(name) {
            Ok(())
        } else {
            Err(TypeError::UnknownTrait(name.to_string()))
        }
    }

    /// Register and check a `trait`: its method requirements (each typed with
    /// `Self` as the abstract conforming type, `Ty::SelfType`).
    fn check_trait(
        &mut self,
        name: &str,
        refines: &[String],
        methods: &[crate::ast::TraitMethod],
        comptime_members: &[TraitComptime],
    ) -> Result<(), TypeError> {
        if self.traits.contains_key(name) || self.structs.contains_key(name) {
            return Err(TypeError::Redeclaration(name.to_string()));
        }
        for parent in refines {
            self.check_trait_name(parent)?;
            if BUILTIN_TRAITS.contains(&parent.as_str()) {
                return Err(TypeError::Unsupported(format!(
                    "user trait '{name}' cannot refine builtin trait '{parent}' yet"
                )));
            }
        }
        let mut ct_members = HashMap::new();
        for parent in refines {
            let inherited = self.traits.get(parent).ok_or_else(|| {
                TypeError::InvariantViolation(format!("trait '{parent}' was not registered"))
            })?;
            for (member, requirement) in &inherited.comptime_members {
                if let Some(existing) = ct_members.get_mut(member) {
                    merge_associated_requirement(existing, requirement, member)?;
                } else {
                    ct_members.insert(member.clone(), requirement.clone());
                }
            }
        }
        for member in comptime_members {
            let requirement = self.ct_member_req_from_anno(&member.ty)?;
            if let Some(existing) = ct_members.get_mut(&member.name) {
                merge_associated_requirement(existing, &requirement, &member.name)?;
            } else {
                ct_members.insert(member.name.clone(), requirement);
            }
        }
        // Requirement signatures resolve `Self` to the abstract `Ty::SelfType`.
        let saved_self_ty = self.self_ty.replace(Ty::SelfType);
        let saved_self_decls = std::mem::take(&mut self.self_decls);
        self.trait_self_comptime.push(ct_members.clone());
        let result = (|| {
            let mut sigs: HashMap<String, Vec<MethodSig>> = HashMap::new();
            for parent in refines {
                let inherited = &self.traits[parent].methods;
                for (method, parent_sigs) in inherited {
                    let overloads = sigs.entry(method.clone()).or_default();
                    for sig in parent_sigs {
                        if !overloads.contains(sig) {
                            overloads.push(sig.clone());
                        }
                    }
                }
            }
            for m in methods {
                self.validate_origin_signature(&[], &m.params, m.self_origin.as_ref())?;
                if ct_members.contains_key(&m.name) {
                    return Err(TypeError::Redeclaration(m.name.clone()));
                }
                if let Some(feature) = Self::advanced_param_feature(
                    &m.params,
                    m.positional_only,
                    m.keyword_only,
                    true,
                    true,
                    false,
                ) {
                    return Err(TypeError::Unsupported(feature.to_string()));
                }
                if m.positional_only.is_some() || m.keyword_only.is_some() {
                    return Err(TypeError::Unsupported(
                        "positional-only/keyword-only markers on trait methods".to_string(),
                    ));
                }
                let mut decls = self.classify_params(&m.type_params)?;
                if let Some(condition) = &m.where_clause {
                    let constraint = self.compile_generic_constraint(condition)?;
                    let Some(last) = decls.last_mut() else {
                        return Err(TypeError::Unsupported(
                            "a where clause requires compile-time parameters".to_string(),
                        ));
                    };
                    match last {
                        ParamDecl::Type { constraints, .. }
                        | ParamDecl::Value { constraints, .. } => constraints.push(constraint),
                    }
                }
                self.tparams.push(type_scope(&decls));
                let signature = (|| {
                    Ok::<_, TypeError>((
                        self.param_tys(&m.params)?,
                        match &m.ret {
                            Some(SourceType::Ref { referent, .. }) => {
                                self.ty_from_anno(referent)?
                            }
                            Some(t) => self.ty_from_anno(t)?,
                            None => Ty::None,
                        },
                        self.declared_error(m.raises, m.raises_type.as_ref())?,
                    ))
                })();
                self.tparams.pop();
                let (all_types, ret, error) = signature?;
                let kw_variadic_idx = m
                    .params
                    .iter()
                    .position(|param| param.kind == crate::ast::ParamKind::KwVariadic);
                if let Some(index) = kw_variadic_idx {
                    self.kwargs_collector_ty(
                        all_types[index].clone(),
                        &format!("trait method '{}.{}' keyword collector", name, m.name),
                    )?;
                }
                let regular: Vec<_> = m
                    .params
                    .iter()
                    .enumerate()
                    .filter(|(_, param)| param.kind == crate::ast::ParamKind::Regular)
                    .collect();
                let regular_params: Vec<_> = regular.iter().map(|(_, param)| *param).collect();
                let sig = MethodSig {
                    decls,
                    availability: Vec::new(),
                    has_self: true,
                    params: regular
                        .iter()
                        .map(|(index, _)| all_types[*index].clone())
                        .collect(),
                    names: regular
                        .iter()
                        .map(|(_, param)| param.name.clone())
                        .collect(),
                    required: vec![true; regular.len()],
                    variadic: None,
                    variadic_index: None,
                    kw_variadic: kw_variadic_idx.map(|index| Box::new(all_types[index].clone())),
                    kw_variadic_index: kw_variadic_idx,
                    positional_only: m.positional_only,
                    keyword_only: m.keyword_only,
                    conventions: regular.iter().map(|(_, param)| param.convention).collect(),
                    ret,
                    raises: error.as_ref().is_some_and(|ty| *ty != Ty::Never),
                    error: error.map(Box::new),
                    self_convention: m.self_convention,
                    ref_params: lower_ref_param_sigs(&m.type_params, &regular_params)?,
                    ref_return: None,
                    implicit: false,
                };
                let overloads = sigs.entry(m.name.clone()).or_default();
                if overloads.iter().any(|existing| {
                    same_method_shape(existing, &sig)
                        && (m.name != "__iter__" || existing.self_convention == sig.self_convention)
                }) {
                    return Err(TypeError::Redeclaration(m.name.clone()));
                }
                overloads.push(sig);
            }
            Ok(sigs)
        })();
        self.trait_self_comptime.pop();
        self.self_ty = saved_self_ty;
        self.self_decls = saved_self_decls;
        let methods = result?;
        self.traits.insert(
            name.to_string(),
            TraitInfo {
                refines: refines.to_vec(),
                methods,
                comptime_members: ct_members,
            },
        );
        Ok(())
    }

    /// Register a struct and check its method bodies. A generic struct's type
    /// parameters are validated and kept in scope (as `Self.T`) for its fields
    /// and methods; field/method types referring to them become `Ty::Param`.
    /// Declared trait conformances are verified once the members are known.
    fn check_struct(&mut self, declaration: &StructDeclaration<'_>) -> Result<(), TypeError> {
        let name = declaration.name;
        let type_params = declaration.type_params;
        let conforms = declaration.conforms;
        if self.structs.contains_key(name) || self.traits.contains_key(name) {
            return Err(TypeError::Redeclaration(name.to_string()));
        }
        let decls = self.classify_params(type_params)?;
        for tr in conforms {
            self.check_trait_name(tr)?;
        }

        // The struct's parameters are in scope as `Self.T` / `Self.n`, and bare
        // `Self` is the struct type, while checking its members. Type parameters
        // appear as `Ty::Param`, value parameters as symbolic `CtValue::Param`.
        let self_ty = Ty::Struct(name.to_string(), decls.iter().map(param_as_arg).collect());
        let saved_self_decls = std::mem::replace(&mut self.self_decls, decls.clone());
        let saved_type_params =
            std::mem::replace(&mut self.enclosing_type_params, type_params.to_vec());
        let saved_self_ty = self.self_ty.replace(self_ty.clone());
        let result = self.check_struct_members(declaration, decls, &self_ty);
        self.self_decls = saved_self_decls;
        self.enclosing_type_params = saved_type_params;
        self.self_ty = saved_self_ty;
        result
    }

    fn check_struct_members(
        &mut self,
        declaration: &StructDeclaration<'_>,
        decls: Vec<ParamDecl>,
        self_ty: &Ty,
    ) -> Result<(), TypeError> {
        let name = declaration.name;
        let conforms = declaration.conforms;
        let fields = declaration.fields;
        let associated = declaration.associated;
        let methods = declaration.methods;
        let fieldwise_init = declaration.fieldwise_init;
        let explicit_destroy_message = declaration
            .decorators
            .iter()
            .find(|decorator| decorator.path.len() == 1 && decorator.path[0] == "explicit_destroy")
            .map(|decorator| {
                if !decorator.kwargs.is_empty() || decorator.args.len() != 1 {
                    return Err(TypeError::Unsupported(
                        "@explicit_destroy requires exactly one positional string message"
                            .to_string(),
                    ));
                }
                match decorator.args.first().map(|arg| &arg.kind) {
                    Some(ExprKind::Str(message)) => Ok(message.clone()),
                    Some(_) => Err(TypeError::Unsupported(
                        "@explicit_destroy message must be a string literal".to_string(),
                    )),
                    None => unreachable!("decorator arity was checked above"),
                }
            })
            .transpose()?;
        let explicit_destructors = methods
            .iter()
            .filter(|method| {
                method.name != "__del__" && method.self_convention == Some(ArgConvention::Deinit)
            })
            .map(|method| (method.name.clone(), method.raises))
            .collect::<HashMap<_, _>>();
        // Field types are resolved against structs defined *so far* (so a struct
        // can't contain itself); duplicate field names are a redeclaration.
        let mut field_tys: Vec<(String, Ty)> = Vec::new();
        for (field_index, f) in fields.iter().enumerate() {
            if field_tys.iter().any(|(n, _)| n == &f.name) {
                return Err(TypeError::Redeclaration(f.name.clone()));
            }
            let ty = self.ty_from_anno(&f.ty)?;
            if Self::type_contains_unsafe_any_pointer(&ty) {
                return Err(TypeError::Unsupported(format!(
                    "field '{}' cannot hide a MutUnsafeAnyOrigin or ImmutUnsafeAnyOrigin pointer",
                    f.name
                )));
            }
            self.declaration_types.borrow_mut().insert(
                crate::checked::AnnotationSite::StructField {
                    module: declaration.module.clone(),
                    declaration: declaration.span,
                    field: field_index,
                },
                ty.clone(),
            );
            field_tys.push((f.name.clone(), ty));
        }
        let associated_values = self.check_struct_associated(associated)?;
        let callable_conformance = declaration
            .callable_conformance
            .as_ref()
            .map(|annotation| self.ty_from_anno(annotation))
            .transpose()?;
        if callable_conformance
            .as_ref()
            .is_some_and(|ty| !matches!(ty, Ty::Func { .. }))
        {
            return Err(TypeError::Unsupported(
                "callable conformance must be a def(...) function type".to_string(),
            ));
        }
        // Register the (method-less) struct first, so methods may reference the
        // struct's own type (even parameterized, `Pair[Self.T]`) in signatures.
        self.structs.insert(
            name.to_string(),
            StructInfo {
                decls,
                conforms: conforms.to_vec(),
                callable_conformance,
                conformance_conditions: declaration
                    .conformance_conditions
                    .iter()
                    .cloned()
                    .collect(),
                fields: field_tys,
                associated: associated_values,
                methods: HashMap::new(),
                fieldwise_init,
                explicit_destroy_message,
                explicit_destructors,
            },
        );
        // Method signatures.
        for (method_index, m) in methods.iter().enumerate() {
            let method_name = lifecycle_method_name(m);
            let method_decls = self.classify_params(&m.type_params)?;
            self.tparams.push(type_scope(&method_decls));
            let all_types = self.param_tys(&m.params)?;
            for (param, ty) in all_types.iter().enumerate() {
                self.declaration_types.borrow_mut().insert(
                    crate::checked::AnnotationSite::MethodParam {
                        module: declaration.module.clone(),
                        declaration: declaration.span,
                        method: method_index,
                        param,
                    },
                    ty.clone(),
                );
            }
            let sig = self.method_sig(m, method_decls, &all_types)?;
            self.tparams.pop();
            let info = self.structs.get_mut(name).ok_or_else(|| {
                TypeError::InvariantViolation(format!("struct '{name}' was not registered"))
            })?;
            let overloads = info.methods.entry(method_name.to_string()).or_default();
            if overloads.iter().any(|existing| {
                same_method_shape(existing, &sig)
                    && (method_name != "__iter__"
                        || existing.self_convention == sig.self_convention)
            }) {
                return Err(TypeError::Redeclaration(method_name.to_string()));
            }
            overloads.push(sig);
        }
        // `@fieldwise_init` and a hand-written `__init__` both define a constructor;
        // having both is a conflict (the decorator *generates* `__init__`).
        if fieldwise_init
            && self
                .structs
                .get(name)
                .is_some_and(|i| i.methods.contains_key("__init__"))
        {
            return Err(TypeError::ConflictingConstructor(name.to_string()));
        }
        // Verify each declared conformance now that the method signatures exist.
        for tr in conforms {
            self.verify_conformance(name, tr, self_ty)?;
        }
        if let Some(expected) = self
            .structs
            .get(name)
            .and_then(|info| info.callable_conformance.clone())
        {
            let Some(call_methods) = self
                .structs
                .get(name)
                .and_then(|info| info.methods.get("__call__"))
            else {
                return Err(TypeError::MissingTraitMethod {
                    struct_name: name.to_string(),
                    trait_name: expected.to_string(),
                    method: "__call__".to_string(),
                });
            };
            let matches = call_methods.iter().any(|method| {
                let actual = Ty::Func {
                    params: method.params.clone(),
                    names: method.names.clone(),
                    ret: Box::new(method.ret.clone()),
                    required: method.required.clone(),
                    variadic: method.variadic.clone(),
                    kw_variadic: method.kw_variadic.clone(),
                    positional_only: method.positional_only,
                    keyword_only: method.keyword_only,
                    raises: method.raises,
                    error: method.error.clone(),
                    conventions: method.conventions.clone(),
                    ref_params: Box::new(method.ref_params.clone()),
                    ref_return: method.ref_return.clone().map(Box::new),
                };
                coerces(&actual, &expected) && coerces(&expected, &actual)
            });
            if !matches {
                return Err(TypeError::TraitMethodMismatch {
                    struct_name: name.to_string(),
                    trait_name: expected.to_string(),
                    method: "__call__".to_string(),
                });
            }
        }
        // Method bodies, each with `self` bound to this struct at its own type
        // parameters (so `self.field : Ty::Param` inside a generic struct).
        for m in methods {
            self.check_method(self_ty, m)?;
        }
        Ok(())
    }

    /// Verify that struct `name` (whose `Self` type is `self_ty`) implements
    /// every method required by trait `tr`, with a matching signature. A few
    /// built-in marker traits have real lifecycle semantics; other built-ins
    /// remain shallow recognized bounds until their corresponding feature grows.
    fn verify_conformance(&self, name: &str, tr: &str, self_ty: &Ty) -> Result<(), TypeError> {
        if BUILTIN_TRAITS.contains(&tr) {
            return self.verify_builtin_conformance(name, tr, self_ty);
        }
        let trait_info = match self.traits.get(tr) {
            Some(info) => info,
            None => return Ok(()),
        };
        let struct_info = self.structs.get(name).ok_or_else(|| {
            TypeError::InvariantViolation(format!(
                "struct '{name}' was not registered before conformance checking"
            ))
        })?;
        for (mname, req_sigs) in &trait_info.methods {
            let got_sigs =
                struct_info
                    .methods
                    .get(mname)
                    .ok_or_else(|| TypeError::MissingTraitMethod {
                        struct_name: name.to_string(),
                        trait_name: tr.to_string(),
                        method: mname.clone(),
                    })?;
            // The requirement's `Self` becomes this struct's type. Receiver
            // conventions are part of the trait method contract.
            for req_sig in req_sigs {
                let want =
                    MethodSig {
                        decls: req_sig.decls.clone(),
                        availability: req_sig.availability.clone(),
                        has_self: true,
                        params: req_sig
                            .params
                            .iter()
                            .map(|t| self.resolve_assoc_ty(&substitute_self(t, self_ty)))
                            .collect(),
                        names: req_sig.names.clone(),
                        required: req_sig.required.clone(),
                        variadic: req_sig.variadic.as_ref().map(|ty| {
                            Box::new(self.resolve_assoc_ty(&substitute_self(ty, self_ty)))
                        }),
                        variadic_index: req_sig.variadic_index,
                        kw_variadic: req_sig.kw_variadic.as_ref().map(|ty| {
                            Box::new(self.resolve_assoc_ty(&substitute_self(ty, self_ty)))
                        }),
                        kw_variadic_index: req_sig.kw_variadic_index,
                        positional_only: req_sig.positional_only,
                        keyword_only: req_sig.keyword_only,
                        conventions: req_sig.conventions.clone(),
                        ret: self.resolve_assoc_ty(&substitute_self(&req_sig.ret, self_ty)),
                        raises: req_sig.raises,
                        error: req_sig.error.as_ref().map(|error| {
                            Box::new(self.resolve_assoc_ty(&substitute_self(error, self_ty)))
                        }),
                        self_convention: req_sig.self_convention,
                        ref_params: req_sig.ref_params.clone(),
                        ref_return: req_sig.ref_return.clone(),
                        implicit: req_sig.implicit,
                    };
                if !got_sigs
                    .iter()
                    .any(|got| method_satisfies_requirement(got, &want))
                {
                    return Err(TypeError::TraitMethodMismatch {
                        struct_name: name.to_string(),
                        trait_name: tr.to_string(),
                        method: mname.clone(),
                    });
                }
            }
        }
        for (member, req) in &trait_info.comptime_members {
            let got = struct_info.associated.get(member).ok_or_else(|| {
                TypeError::MissingTraitComptimeMember {
                    struct_name: name.to_string(),
                    trait_name: tr.to_string(),
                    member: member.clone(),
                }
            })?;
            if !self.ct_member_satisfies(got, req, self_ty) {
                return Err(TypeError::TraitComptimeMemberMismatch {
                    struct_name: name.to_string(),
                    trait_name: tr.to_string(),
                    member: member.clone(),
                });
            }
        }
        Ok(())
    }

    fn verify_builtin_conformance(
        &self,
        name: &str,
        tr: &str,
        self_ty: &Ty,
    ) -> Result<(), TypeError> {
        let ok = match tr {
            "Copyable" => self.struct_copyable_conformance_ok(name),
            "ImplicitlyCopyable" => self.struct_implicitly_copyable_conformance_ok(name),
            "Movable" => self.is_movable(self_ty),
            "ImplicitlyDeletable" => true,
            "Indexer" => self.structs.get(name).is_some_and(|info| {
                info.methods.get("__mlir_index__").is_some_and(|methods| {
                    methods.iter().any(|method| {
                        method.has_self && method.params.is_empty() && method.ret == Ty::Int
                    })
                })
            }),
            "Writer" => self.structs.get(name).is_some_and(|info| {
                info.methods.get("write_string").is_some_and(|methods| {
                    methods.iter().any(|method| {
                        method.has_self
                            && method.self_convention == Some(ArgConvention::Mut)
                            && method.params == [Ty::String]
                            && method.ret == Ty::None
                    })
                })
            }),
            "Hasher" => self.structs.get(name).is_some_and(|info| {
                let initializes = info.methods.get("__init__").is_some_and(|methods| {
                    methods.iter().any(|method| method.params.is_empty())
                });
                let updates = info.methods.get("update").is_some_and(|methods| {
                    methods.iter().any(|method| {
                        method.self_convention == Some(ArgConvention::Mut)
                            && method.params.len() == 1
                            && method.ret == Ty::None
                    })
                });
                let finishes = info.methods.get("finish").is_some_and(|methods| {
                    methods.iter().any(|method| {
                        method.params.is_empty() && method.ret == Ty::UInt
                    })
                });
                initializes && updates && finishes
            }),
            "Writable" => self.structs.get(name).is_some_and(|info| {
                ["write_to", "write_repr_to"].into_iter().all(|name| {
                    info.methods.get(name).is_none_or(|methods| {
                        methods.iter().any(|method| {
                            method.params.len() == 1
                                && method.conventions[0] == Some(ArgConvention::Mut)
                                && matches!(&method.params[0], Ty::Param { bounds, .. } if bounds.iter().any(|bound| bound == "Writer"))
                                && method.ret == Ty::None
                        })
                    })
                })
            }),
            // Layout/backend markers remain accepted-but-shallow until a backend
            // needs them; operation traits are checked at their operations.
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            Err(TypeError::TraitNotSatisfied {
                param: "Self".to_string(),
                ty: self_ty.to_string(),
                trait_name: tr.to_string(),
                reason: self.trait_failure_reason(self_ty, tr),
            })
        }
    }

    fn ct_member_satisfies(&self, value: &CtValue, req: &CtMemberReq, self_ty: &Ty) -> bool {
        match req {
            CtMemberReq::Value(expected) => self
                .ct_value_ty(value, self_ty)
                .is_some_and(|actual| coerces(&actual, expected)),
            CtMemberReq::Type { bounds } => {
                let CtValue::Type(ty) = value else {
                    return false;
                };
                bounds.iter().all(|bound| self.conforms_to(ty, bound))
            }
        }
    }

    fn ct_value_ty(&self, value: &CtValue, self_ty: &Ty) -> Option<Ty> {
        match value {
            CtValue::Int(_) | CtValue::Param(_) => Some(Ty::Int),
            CtValue::UInt(_) => Some(Ty::UInt),
            CtValue::Float(_) => Some(Ty::Float64),
            CtValue::Bool(_) => Some(Ty::Bool),
            CtValue::Str(_) => Some(Ty::String),
            CtValue::Tuple(values) => values
                .iter()
                .map(|v| self.ct_value_ty(v, self_ty))
                .collect::<Option<Vec<_>>>()
                .map(Ty::Tuple),
            CtValue::List(values) => {
                let first = values.first()?;
                let elem = self.ct_value_ty(first, self_ty)?;
                if values.iter().skip(1).all(|v| {
                    self.ct_value_ty(v, self_ty)
                        .is_some_and(|ty| coerces(&ty, &elem))
                }) {
                    Some(Ty::List(Box::new(elem)))
                } else {
                    None
                }
            }
            CtValue::Type(_) | CtValue::Reflected(_) => {
                let _ = self_ty;
                None
            }
        }
    }

    fn check_method(&mut self, self_ty: &Ty, m: &Method) -> Result<(), TypeError> {
        let decls = self.classify_params(&m.type_params)?;
        self.tparams.push(type_scope(&decls));
        let saved = self.enclosing_type_params.clone();
        self.enclosing_type_params.extend(m.type_params.clone());
        let result = self.check_method_inner(self_ty, m);
        self.enclosing_type_params = saved;
        self.tparams.pop();
        result
    }

    fn check_method_inner(&mut self, self_ty: &Ty, m: &Method) -> Result<(), TypeError> {
        let is_implicit = m
            .decorators
            .iter()
            .any(|decorator| decorator.path.len() == 1 && decorator.path[0] == "implicit");
        if is_implicit
            && (m.name != "__init__"
                || !m.has_self
                || m.self_convention != Some(ArgConvention::Out)
                || m.params.len() != 1
                || m.params[0].kind != crate::ast::ParamKind::Regular
                || m.params[0].default.is_some()
                || m.params[0].convention.is_some()
                || m.ret.is_some()
                || m.raises)
        {
            return Err(TypeError::Unsupported(
                "@implicit requires a non-raising single-argument '__init__(out self, value: T)'"
                    .to_string(),
            ));
        }
        self.validate_origin_signature(
            &self.enclosing_type_params,
            &m.params,
            m.self_origin.as_ref(),
        )?;
        if !is_mojo_copy_constructor(m)
            && !is_mojo_move_constructor(m)
            && let Some(feature) = Self::advanced_param_feature(
                &m.params,
                m.positional_only,
                m.keyword_only,
                false,
                false,
                false,
            )
        {
            return Err(TypeError::Unsupported(feature.to_string()));
        }
        // `out self` initializes the receiver: it is allowed on the **`__init__`**
        // lifecycle method (a hand-written constructor), where `self`'s fields are
        // assigned in the body. `ref self` (parametric-mutability references), and
        // `out self` on any other method, still need semantics we don't model, so
        // they stay flagged. A plain `self`, `read self`, `mut self`, or `var
        // self` consuming method is fine.
        // `out self` initializes the receiver — allowed on the lifecycle methods
        // `__init__` (constructor), `__copyinit__` (copy), and `__moveinit__` (move),
        // whose bodies assign `self`'s fields. `ref self`, and `out self` elsewhere,
        // stay flagged.
        let is_lifecycle_init = matches!(
            m.name.as_str(),
            "__init__" | "__copyinit__" | "__moveinit__"
        );
        let out_init =
            matches!(m.self_convention, Some(crate::ast::ArgConvention::Out)) && is_lifecycle_init;
        if matches!(m.self_convention, Some(crate::ast::ArgConvention::Out)) && !out_init {
            return Err(TypeError::Unsupported(
                "'out self' receiver outside a lifecycle initializer".to_string(),
            ));
        }
        let ret_ty = match &m.ret {
            Some(SourceType::Ref { referent, .. }) => self.ty_from_anno(referent)?,
            Some(t) => self.ty_from_anno(t)?,
            None => Ty::None,
        };
        let regular: Vec<&FnParam> = m
            .params
            .iter()
            .filter(|param| param.kind == crate::ast::ParamKind::Regular)
            .collect();
        let ref_return = match &m.ret {
            Some(SourceType::Ref { origin, .. }) => Some(lower_ref_sig(
                origin.as_ref().ok_or_else(|| {
                    TypeError::Unsupported("reference return requires an origin".to_string())
                })?,
                &self.enclosing_type_params,
                &regular,
            )?),
            _ => None,
        };
        for param in &m.params {
            if let Some(default) = &param.default {
                let expected = self.ty_from_anno(&param.ty)?;
                let found = self.infer(default)?;
                if !coerces(&found, &expected) {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: found.to_string(),
                        context: format!("default value of method parameter '{}'", param.name),
                    });
                }
            }
        }
        self.push_scope();
        self.raising_context
            .push(self.declared_error(m.raises, m.raises_type.as_ref())?);
        let mut result = self.bind_and_check_method(self_ty, m, &ret_ty, ref_return);
        // Definite initialization (conservative, flow-insensitive first pass): an
        // `__init__` must assign every declared field somewhere in its body, so a
        // constructed value has no unset fields. Path-sensitive DI (assign exactly
        // once, before any read, on every path) is left for a later refinement.
        if result.is_ok()
            && out_init
            && let Ty::Struct(sname, _) = self_ty
        {
            result = self.check_definite_init(sname, &m.name, &m.body);
        }
        self.raising_context.pop();
        self.pop_scope();
        result
    }

    /// Verify an `out self` lifecycle method (`method`) assigns every declared field
    /// of `sname` (flow-insensitive: assigned *somewhere*). Reports the first missing
    /// field.
    fn check_definite_init(
        &self,
        sname: &str,
        method: &str,
        body: &[Stmt],
    ) -> Result<(), TypeError> {
        let info = self.structs.get(sname).ok_or_else(|| {
            TypeError::InvariantViolation(format!("struct '{sname}' was not registered"))
        })?;
        for (field, _) in &info.fields {
            if !definitely_initializes_self_field(body, field) {
                return Err(TypeError::UninitializedField {
                    struct_name: sname.to_string(),
                    method: method.to_string(),
                    field: field.clone(),
                });
            }
        }
        Ok(())
    }

    fn bind_and_check_method(
        &mut self,
        self_ty: &Ty,
        m: &Method,
        ret_ty: &Ty,
        ref_return: Option<crate::origin::RefSig>,
    ) -> Result<(), TypeError> {
        let self_writable = matches!(
            m.self_convention,
            Some(
                crate::ast::ArgConvention::Mut
                    | crate::ast::ArgConvention::Ref
                    | crate::ast::ArgConvention::Out
            )
        );
        if m.has_self {
            self.declare_with_mutability("self", self_ty.clone(), self_writable)?;
            if self.type_contains_reference(self_ty)
                && let Some(owner) = self.lookup_owner("self")
            {
                self.set_aggregate_origins(
                    "self",
                    vec![crate::origin::Origin::Place(crate::origin::OriginPlace {
                        root: owner,
                        path: Vec::new(),
                    })],
                );
            }
        }
        for p in &m.params {
            let mut pty = self.ty_from_anno(&p.ty)?;
            pty = match p.kind {
                crate::ast::ParamKind::Variadic => Ty::List(Box::new(pty)),
                crate::ast::ParamKind::KwVariadic => {
                    self.kwargs_collector_ty(pty, &format!("keyword collector '{}'", p.name))?
                }
                crate::ast::ParamKind::Regular => pty,
            };
            self.declare_with_mutability(
                &p.name,
                pty.clone(),
                p.kind == crate::ast::ParamKind::KwVariadic
                    || ref_parameter_is_writable(p, &self.enclosing_type_params),
            )?;
            if matches!(p.convention, Some(crate::ast::ArgConvention::Ref)) {
                self.register_reference_parameter(
                    &p.name,
                    pty.clone(),
                    ref_parameter_is_writable(p, &self.enclosing_type_params),
                );
            }
            if !matches!(pty, Ty::Ref(_))
                && self.type_contains_reference(&pty)
                && let Some(owner) = self.lookup_owner(&p.name)
            {
                self.set_aggregate_origins(
                    &p.name,
                    vec![crate::origin::Origin::Place(crate::origin::OriginPlace {
                        root: owner,
                        path: Vec::new(),
                    })],
                );
            }
        }
        // `self` is writable in a `mut self` method, or an `out self` `__init__`
        // (which assigns its fields). Restored after the body.
        let saved = std::mem::replace(&mut self.self_mutable, self_writable);
        let initializing = matches!(m.self_convention, Some(crate::ast::ArgConvention::Out))
            && matches!(
                lifecycle_method_name(m),
                "__init__" | "__copyinit__" | "__moveinit__"
            );
        let saved_initializing = std::mem::replace(&mut self.self_initializing, initializing);
        let owners: Vec<_> = m
            .params
            .iter()
            .filter(|param| param.kind == crate::ast::ParamKind::Regular)
            .map(|param| {
                self.lookup_owner(&param.name)
                    .expect("bound method parameter")
            })
            .collect();
        let self_owner = self.lookup_owner("self");
        let mut allowed: HashSet<_> = owners.iter().copied().collect();
        allowed.extend(self_owner);
        self.aggregate_escape_contexts
            .push((self.scopes.len().saturating_sub(1), allowed));
        self.return_ref_contracts
            .push(ref_return.map(|signature| (signature, owners, self_owner)));
        let result = self.check_block(&m.body, Some(ret_ty), false);
        self.return_ref_contracts.pop();
        self.aggregate_escape_contexts.pop();
        self.self_mutable = saved;
        self.self_initializing = saved_initializing;
        result?;
        if *ret_ty != Ty::None && !definitely_returns(&m.body) {
            return Err(TypeError::MissingReturn(m.name.clone()));
        }
        Ok(())
    }

    /// Type a struct construction `Name[param_args](args)` (the fieldwise
    /// constructor). Type parameters are supplied explicitly or inferred from the
    /// field arguments; value parameters must be supplied explicitly.
    fn infer_construction(
        &self,
        span: SourceSpan,
        name: &str,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Ty, TypeError> {
        let info = self.structs.get(name).ok_or_else(|| {
            TypeError::InvariantViolation(format!("constructor target '{name}' is not registered"))
        })?;
        if !kwargs.is_empty() && args.is_empty() && kwargs.len() == 1 && kwargs[0].name == "copy" {
            let Some(sig) = info
                .methods
                .get("__copyinit__")
                .and_then(|sigs| sigs.iter().find(|sig| sig.params.len() == 1))
            else {
                return Err(TypeError::BadCall {
                    func: name.to_string(),
                    reason: "no matching copy constructor".to_string(),
                });
            };
            let params = sig.params.clone();
            let decls = info.decls.clone();
            let arg_ty = self.infer(&kwargs[0].value)?;
            let (subst, tyargs) = self.resolve_use_params(
                name,
                &decls,
                param_args,
                &params,
                std::slice::from_ref(&arg_ty),
            )?;
            let expected = substitute(&params[0], &subst);
            if !coerces(&arg_ty, &expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: arg_ty.to_string(),
                    context: format!("argument 'copy' to '{}.__init__'", name),
                });
            }
            return Ok(Ty::Struct(name.to_string(), tyargs));
        }
        // A hand-written `def __init__(out self, …)` is the constructor: check the
        // call arguments against its parameters (the `self` receiver is implicit).
        // Takes precedence over `@fieldwise_init`. On a **generic** struct, the type
        // parameters are solved by unifying `__init__`'s parameter types against the
        // argument types — exactly as the fieldwise path unifies field types.
        if let Some(sigs) = info.methods.get("__init__") {
            if info.decls.is_empty() {
                let mut matches = Vec::new();
                for sig in sigs {
                    if let Ok(scored) = self.score_method_call(
                        sig,
                        &sig.params,
                        sig.variadic.as_deref(),
                        sig.kw_variadic.as_deref(),
                        args,
                        kwargs,
                    ) {
                        matches.push(MethodCallResolution {
                            conversion_score: scored.rank,
                            slots: scored.slots,
                            keyword_overflow: scored.keyword_overflow,
                            keyword_element: sig.kw_variadic.as_deref().cloned(),
                            conventions: sig.conventions.clone(),
                            return_type: Ty::Struct(name.to_string(), Vec::new()),
                            raises: sig.raises,
                            error: sig.error.clone(),
                            mutates_receiver: false,
                            consumes_receiver: false,
                            lowered_name: (sigs.len() > 1)
                                .then(|| method_lowered_name(name, "__init__", sig)),
                            ref_params: sig.ref_params.clone(),
                            ref_return: None,
                            param_types: sig.params.clone(),
                        });
                    }
                }
                let selected = select_method_overload("__init__", matches).map_err(|kind| {
                    TypeError::BadCall {
                        func: name.to_string(),
                        reason: match kind {
                            OverloadSelect::NoMatch => {
                                "no constructor overload matches the supplied arguments"
                            }
                            OverloadSelect::Ambiguous => "ambiguous overloaded constructor call",
                        }
                        .to_string(),
                    }
                })?;
                if let Some(target) = &selected.lowered_name {
                    self.overload_targets
                        .borrow_mut()
                        .insert(span, target.clone());
                }
                self.record_selected_method_conversions("__init__", &selected, args, kwargs)?;
                for arg in args {
                    let ty = self.infer(arg)?;
                    self.check_consuming(arg, &ty, &format!("argument to '{name}'"))?;
                }
                for arg in kwargs {
                    let ty = self.infer(&arg.value)?;
                    self.check_consuming(
                        &arg.value,
                        &ty,
                        &format!("argument '{}' to '{name}'", arg.name),
                    )?;
                }
                return Ok(Ty::Struct(name.to_string(), Vec::new()));
            }
            if sigs.len() == 1 {
                let sig = &sigs[0];
                let params = sig.params.clone();
                let decls = info.decls.clone();
                let arg_tys = args
                    .iter()
                    .map(|a| self.infer(a))
                    .collect::<Result<Vec<_>, _>>()?;
                let (subst, tyargs) =
                    self.resolve_use_params(name, &decls, param_args, &params, &arg_tys)?;
                for (i, (aty, pty)) in arg_tys.iter().zip(&params).enumerate() {
                    let expected = substitute(pty, &subst);
                    if !coerces(aty, &expected) {
                        return Err(TypeError::TypeMismatch {
                            expected: expected.to_string(),
                            found: aty.to_string(),
                            context: format!("argument {} to '{}.__init__'", i + 1, name),
                        });
                    }
                    // A constructor argument is bound into `self` by value — consuming.
                    self.check_consuming(
                        &args[i],
                        aty,
                        &format!("argument {} to '{}'", i + 1, name),
                    )?;
                }
                return Ok(Ty::Struct(name.to_string(), tyargs));
            }
            let decls = info.decls.clone();
            let arg_tys = args
                .iter()
                .map(|a| self.infer(a))
                .collect::<Result<Vec<_>, _>>()?;
            let overloaded = sigs.len() > 1;
            let mut matches = Vec::new();
            for sig in sigs {
                let params = sig.params.clone();
                if params.len() != arg_tys.len() {
                    continue;
                }
                if let Ok((subst, tyargs)) =
                    self.resolve_use_params(name, &decls, param_args, &params, &arg_tys)
                {
                    let mut score = 0;
                    let mut ok = true;
                    for (aty, pty) in arg_tys.iter().zip(&params) {
                        let expected = substitute(pty, &subst);
                        if !coerces(aty, &expected) {
                            ok = false;
                            break;
                        }
                        if *aty != expected {
                            score += 1;
                        }
                    }
                    if ok {
                        matches.push((score, sig.clone(), tyargs));
                    }
                }
            }
            let best = matches.iter().map(|(score, ..)| *score).min();
            if let Some(best) = best {
                let mut best_matches = matches
                    .into_iter()
                    .filter(|(score, ..)| *score == best)
                    .collect::<Vec<_>>();
                if best_matches.len() != 1 {
                    return Err(TypeError::BadCall {
                        func: name.to_string(),
                        reason: "ambiguous overloaded constructor call".to_string(),
                    });
                }
                let (_, sig, tyargs) = best_matches.remove(0);
                for (i, aty) in arg_tys.iter().enumerate() {
                    // A constructor argument is bound into `self` by value — consuming.
                    self.check_consuming(
                        &args[i],
                        aty,
                        &format!("argument {} to '{}'", i + 1, name),
                    )?;
                }
                if overloaded {
                    self.overload_targets
                        .borrow_mut()
                        .insert(span, method_lowered_name(name, "__init__", &sig));
                }
                return Ok(Ty::Struct(name.to_string(), tyargs));
            }
            return Err(TypeError::BadCall {
                func: name.to_string(),
                reason: "no constructor overload matches the supplied arguments".to_string(),
            });
        }
        if info.methods.contains_key("__init__") {
            return Err(TypeError::ArityMismatch {
                name: name.to_string(),
                expected: info
                    .methods
                    .get("__init__")
                    .and_then(|sigs| sigs.first())
                    .map(|sig| sig.params.len())
                    .unwrap_or(0),
                got: args.len(),
            });
        }
        if !info.fieldwise_init {
            return Err(TypeError::NoConstructor(name.to_string()));
        }
        let decls = info.decls.clone();
        let field_tys: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
        if field_tys.len() != args.len() {
            return Err(TypeError::ArityMismatch {
                name: name.to_string(),
                expected: field_tys.len(),
                got: args.len(),
            });
        }
        let arg_tys = args
            .iter()
            .zip(&field_tys)
            .map(|(argument, field)| {
                if self.type_contains_reference(field) {
                    self.infer_storage_value(argument, field)
                } else {
                    self.infer(argument)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (subst, tyargs) =
            self.resolve_use_params(name, &decls, param_args, &field_tys, &arg_tys)?;
        for (i, (aty, fty)) in arg_tys.iter().zip(&field_tys).enumerate() {
            let expected = substitute(fty, &subst);
            if !Self::storage_value_coerces(aty, &expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: aty.to_string(),
                    context: format!("field {} of '{}'", i + 1, name),
                });
            }
            if self.type_contains_reference(&expected) {
                self.mark_reference_storage_uses(&args[i], &expected);
            }
            if matches!(expected, Ty::Ref(_)) {
                continue;
            }
            // A constructor stores each argument in a field by value — a consuming
            // position.
            self.check_consuming(&args[i], aty, &format!("field {} of '{}'", i + 1, name))?;
        }
        Ok(Ty::Struct(name.to_string(), tyargs))
    }

    /// Resolve a generic use site's parameters, returning a type-parameter
    /// substitution and the full argument list (types + values) for the struct's
    /// identity. When `param_args` is non-empty the parameters are supplied
    /// explicitly (positionally); otherwise the type parameters are inferred from
    /// `patterns`/`actuals` (a value parameter cannot be inferred).
    fn resolve_use_params(
        &self,
        name: &str,
        decls: &[ParamDecl],
        param_args: &[crate::ast::ParamArg],
        patterns: &[Ty],
        actuals: &[Ty],
    ) -> Result<(HashMap<String, Ty>, Vec<TyArg>), TypeError> {
        let mut subst: HashMap<String, Ty> = HashMap::new();
        if decls.is_empty() {
            if !param_args.is_empty() {
                return Err(TypeError::WrongTypeArgCount {
                    name: name.to_string(),
                    expected: 0,
                    got: param_args.len(),
                });
            }
            return Ok((subst, Vec::new()));
        }
        if !param_args.is_empty() {
            let mut bound: Vec<Vec<&crate::ast::ParamArg>> = vec![Vec::new(); decls.len()];
            let mut positional = 0;
            let mut saw_keyword = false;
            for argument in param_args {
                match argument {
                    crate::ast::ParamArg::Named {
                        name: keyword,
                        value,
                    } => {
                        saw_keyword = true;
                        let Some(index) = decls
                            .iter()
                            .position(|decl| decl.name().trim_start_matches('*') == keyword)
                        else {
                            return Err(TypeError::Unsupported(format!(
                                "generic '{name}' has no parameter named '{keyword}'"
                            )));
                        };
                        if !bound[index].is_empty() {
                            return Err(TypeError::Redeclaration(keyword.clone()));
                        }
                        bound[index].push(value);
                    }
                    positional_argument => {
                        if saw_keyword {
                            return Err(TypeError::Unsupported(
                                "positional compile-time argument follows a keyword argument"
                                    .to_string(),
                            ));
                        }
                        while positional < decls.len()
                            && !bound[positional].is_empty()
                            && !matches!(
                                decls[positional],
                                ParamDecl::Type { variadic: true, .. }
                                    | ParamDecl::Value { variadic: true, .. }
                            )
                        {
                            positional += 1;
                        }
                        if positional >= decls.len() {
                            return Err(TypeError::WrongTypeArgCount {
                                name: name.to_string(),
                                expected: decls.len(),
                                got: param_args.len(),
                            });
                        }
                        bound[positional].push(positional_argument);
                        if !matches!(
                            decls[positional],
                            ParamDecl::Type { variadic: true, .. }
                                | ParamDecl::Value { variadic: true, .. }
                        ) {
                            positional += 1;
                        }
                    }
                }
            }
            let mut tyargs = Vec::with_capacity(decls.len());
            let mut value_environment = HashMap::new();
            for (decl, arguments) in decls.iter().zip(bound) {
                let infer_only = matches!(
                    decl,
                    ParamDecl::Type {
                        infer_only: true,
                        ..
                    } | ParamDecl::Value {
                        infer_only: true,
                        ..
                    }
                );
                if infer_only && !arguments.is_empty() {
                    return Err(TypeError::Unsupported(format!(
                        "infer-only parameter '{}' cannot be supplied explicitly",
                        decl.name().trim_start_matches('*')
                    )));
                }
                let variadic = matches!(
                    decl,
                    ParamDecl::Type { variadic: true, .. }
                        | ParamDecl::Value { variadic: true, .. }
                );
                if variadic {
                    let values = arguments
                        .into_iter()
                        .map(|argument| self.resolve_param_arg(decl, argument))
                        .map(|result| match result? {
                            TyArg::Ty(ty) => Ok(CtValue::Type(Box::new(ty))),
                            TyArg::Val(value) => Ok(value),
                        })
                        .collect::<Result<Vec<_>, TypeError>>()?;
                    let value = CtValue::Tuple(values);
                    value_environment.insert(
                        decl.name().trim_start_matches('*').to_string(),
                        value.clone(),
                    );
                    tyargs.push(TyArg::Val(value));
                    continue;
                }
                let tyarg = if let Some(argument) = arguments.first() {
                    self.resolve_param_arg(decl, argument)?
                } else if let ParamDecl::Value {
                    default: Some(value),
                    ..
                } = decl
                {
                    TyArg::Val(value.evaluate(&value_environment).ok_or_else(|| {
                        TypeError::NotComptime(format!("default for parameter '{}'", decl.name()))
                    })?)
                } else if let ParamDecl::Type {
                    default: Some(ty), ..
                } = decl
                {
                    TyArg::Ty((**ty).clone())
                } else {
                    return Err(TypeError::CannotInferTypeParam {
                        name: name.to_string(),
                        param: decl.name().to_string(),
                    });
                };
                if let (ParamDecl::Type { name, .. }, TyArg::Ty(t)) = (decl, &tyarg) {
                    subst.insert(name.clone(), t.clone());
                }
                tyargs.push(tyarg);
                if let Some(TyArg::Val(value)) = tyargs.last() {
                    value_environment.insert(
                        decl.name().trim_start_matches('*').to_string(),
                        value.clone(),
                    );
                }
            }
            self.validate_generic_constraints(name, decls, &tyargs)?;
            return Ok((subst, tyargs));
        }
        // Inference: only type parameters, solved from the argument types.
        for (pat, act) in patterns.iter().zip(actuals) {
            if let Ty::Param { name, bounds } = pat
                && name.starts_with('*')
            {
                for bound in bounds {
                    if !self.conforms_to(act, bound) {
                        return Err(TypeError::TraitNotSatisfied {
                            param: name.clone(),
                            ty: act.to_string(),
                            trait_name: bound.clone(),
                            reason: self.trait_failure_reason(act, bound),
                        });
                    }
                }
                subst.entry(name.clone()).or_insert_with(|| pat.clone());
            } else {
                unify(pat, act, &mut subst)?;
            }
        }
        let inferred_packs: HashMap<String, Vec<CtValue>> = patterns
            .iter()
            .zip(actuals)
            .filter_map(|(pattern, actual)| match pattern {
                Ty::Param { name, .. } if name.starts_with('*') => {
                    Some((name.trim_start_matches('*').to_string(), actual.clone()))
                }
                _ => None,
            })
            .fold(HashMap::new(), |mut packs, (name, ty)| {
                packs
                    .entry(name)
                    .or_insert_with(Vec::new)
                    .push(CtValue::Type(Box::new(ty)));
                packs
            });
        let mut tyargs = Vec::with_capacity(decls.len());
        let mut value_environment = HashMap::new();
        for decl in decls {
            match decl {
                ParamDecl::Value {
                    name: pname,
                    default,
                    ..
                } => {
                    if let Some(value) = default {
                        let value = value.evaluate(&value_environment).ok_or_else(|| {
                            TypeError::NotComptime(format!("default for parameter '{}'", pname))
                        })?;
                        value_environment
                            .insert(pname.trim_start_matches('*').to_string(), value.clone());
                        tyargs.push(TyArg::Val(value));
                    } else {
                        return Err(TypeError::CannotInferTypeParam {
                            name: name.to_string(),
                            param: pname.clone(),
                        });
                    }
                }
                ParamDecl::Type {
                    name: pname,
                    bounds,
                    default,
                    variadic,
                    ..
                } => {
                    if *variadic {
                        tyargs.push(TyArg::Val(CtValue::Tuple(
                            inferred_packs
                                .get(pname.trim_start_matches('*'))
                                .cloned()
                                .unwrap_or_default(),
                        )));
                        continue;
                    }
                    let solved = subst
                        .get(pname)
                        .cloned()
                        .or_else(|| default.as_ref().map(|default| (**default).clone()))
                        .ok_or_else(|| TypeError::CannotInferTypeParam {
                            name: name.to_string(),
                            param: pname.clone(),
                        })?;
                    for bound in bounds {
                        if !self.conforms_to(&solved, bound) {
                            return Err(TypeError::TraitNotSatisfied {
                                param: pname.clone(),
                                ty: solved.to_string(),
                                trait_name: bound.clone(),
                                reason: self.trait_failure_reason(&solved, bound),
                            });
                        }
                    }
                    tyargs.push(TyArg::Ty(solved));
                }
            }
        }
        self.validate_generic_constraints(name, decls, &tyargs)?;
        Ok((subst, tyargs))
    }

    fn validate_generic_constraints(
        &self,
        name: &str,
        decls: &[ParamDecl],
        arguments: &[TyArg],
    ) -> Result<(), TypeError> {
        let environment: HashMap<&str, &TyArg> = decls
            .iter()
            .zip(arguments)
            .map(|(decl, argument)| (decl.name().trim_start_matches('*'), argument))
            .collect();
        for constraint in decls.iter().flat_map(|decl| match decl {
            ParamDecl::Type { constraints, .. } | ParamDecl::Value { constraints, .. } => {
                constraints.as_slice()
            }
        }) {
            if !self.eval_generic_constraint(constraint, &environment) {
                return Err(TypeError::BadCall {
                    func: name.to_string(),
                    reason: format!("generic constraint is not satisfied: {constraint:?}"),
                });
            }
        }
        Ok(())
    }

    fn eval_generic_constraint(
        &self,
        constraint: &GenericConstraint,
        environment: &HashMap<&str, &TyArg>,
    ) -> bool {
        use GenericConstraint::*;
        match constraint {
            Bool(value) => *value,
            Not(value) => !self.eval_generic_constraint(value, environment),
            And(left, right) => {
                self.eval_generic_constraint(left, environment)
                    && self.eval_generic_constraint(right, environment)
            }
            Or(left, right) => {
                self.eval_generic_constraint(left, environment)
                    || self.eval_generic_constraint(right, environment)
            }
            Conforms { param, trait_name } => environment
                .get(param.as_str())
                .and_then(|argument| match argument {
                    TyArg::Ty(ty) => Some(self.conforms_to(ty, trait_name)),
                    TyArg::Val(_) => None,
                })
                .unwrap_or(false),
            ConformsPack { param, trait_name } => environment
                .get(param.as_str())
                .and_then(|argument| {
                    match argument {
                    TyArg::Val(CtValue::Tuple(values)) => Some(values.iter().all(|value| {
                        matches!(value, CtValue::Type(ty) if self.conforms_to(ty, trait_name))
                    })),
                    _ => None,
                }
                })
                .unwrap_or(false),
            Eq(left, right) => {
                self.constraint_value(left, environment)
                    == self.constraint_value(right, environment)
            }
            Ne(left, right) => {
                self.constraint_value(left, environment)
                    != self.constraint_value(right, environment)
            }
            Lt(left, right) | Le(left, right) | Gt(left, right) | Ge(left, right) => {
                let (Some(TyArg::Val(CtValue::Int(left))), Some(TyArg::Val(CtValue::Int(right)))) = (
                    self.constraint_value(left, environment),
                    self.constraint_value(right, environment),
                ) else {
                    return false;
                };
                match constraint {
                    Lt(_, _) => left < right,
                    Le(_, _) => left <= right,
                    Gt(_, _) => left > right,
                    Ge(_, _) => left >= right,
                    _ => unreachable!(),
                }
            }
        }
    }

    fn constraint_value<'b>(
        &self,
        operand: &'b ConstraintOperand,
        environment: &HashMap<&str, &'b TyArg>,
    ) -> Option<TyArg> {
        match operand {
            ConstraintOperand::Param(name) => {
                environment.get(name.as_str()).map(|value| (*value).clone())
            }
            ConstraintOperand::Value(value) => Some(TyArg::Val(value.clone())),
            ConstraintOperand::Type(ty) => Some(TyArg::Ty(ty.clone())),
        }
    }

    /// Whether `ty` conforms to trait `tr`. Lifecycle marker built-ins are tied
    /// to observable ownership behavior; other built-ins remain recognized but
    /// shallow unless their feature has a dedicated checker path. A user trait is
    /// satisfied nominally: a struct must *declare* conformance, and a type
    /// parameter must carry `tr` among its bounds (so a bounded `T` can be
    /// forwarded to another `[U: tr]` parameter).
    fn conforms_to(&self, ty: &Ty, tr: &str) -> bool {
        if let Ty::Param { bounds, .. } = ty
            && bounds.iter().any(|bound| bound == tr)
        {
            return true;
        }
        if BUILTIN_TRAITS.contains(&tr) {
            return match tr {
                "AnyType" => true,
                "Copyable" => self.is_copyable(ty),
                "ImplicitlyCopyable" => self.is_implicitly_copyable(ty),
                "Movable" => self.is_movable(ty),
                "ImplicitlyDeletable" => self.is_implicitly_deletable(ty),
                "Hashable" => self.is_hashable(ty),
                "Writable" => match ty {
                    Ty::Struct(name, args) => self.struct_conformance_applies(name, args, tr),
                    Ty::Variant(alternatives) => alternatives
                        .iter()
                        .all(|alternative| self.conforms_to(alternative, tr)),
                    Ty::Param { bounds, .. } => bounds.iter().any(|bound| bound == tr),
                    Ty::Func { .. } | Ty::GenericFunc { .. } | Ty::Overload(_) => false,
                    _ => true,
                },
                "Writer" | "Hasher" => match ty {
                    Ty::Struct(name, args) => self.struct_conformance_applies(name, args, tr),
                    Ty::Param { bounds, .. } => bounds.iter().any(|bound| bound == tr),
                    _ => false,
                },
                "Indexer" => match ty {
                    Ty::Int | Ty::IntLiteral => true,
                    Ty::Struct(name, args) => self.struct_conformance_applies(name, args, tr),
                    Ty::Param { bounds, .. } => bounds.iter().any(|bound| bound == tr),
                    _ => false,
                },
                "Equatable" => has_equality_bound_or_concrete(self, ty),
                "Comparable" => self.is_comparable(ty),
                "Absable" | "Roundable" | "Powable" => is_numeric_like(ty),
                "Intable" => is_numeric_like(ty) || *ty == Ty::Bool,
                "Floatable" => is_numeric_like(ty),
                // Layout/backend markers and future operation traits stay shallow.
                _ => true,
            };
        }
        match ty {
            Ty::Struct(name, args) => self.struct_conformance_applies(name, args, tr),
            Ty::Param { bounds, .. } => bounds
                .iter()
                .any(|bound| bound == tr || self.trait_refines(bound, tr)),
            _ => false,
        }
    }

    fn struct_conformance_applies(&self, name: &str, args: &[TyArg], required: &str) -> bool {
        let Some(info) = self.structs.get(name) else {
            return false;
        };
        info.conforms.iter().any(|declared| {
            (declared == required || self.trait_refines(declared, required))
                && info
                    .conformance_conditions
                    .get(declared)
                    .is_none_or(|condition| self.eval_conformance_condition(info, args, condition))
        })
    }

    fn eval_conformance_condition(&self, info: &StructInfo, args: &[TyArg], expr: &Expr) -> bool {
        let arguments: HashMap<&str, &TyArg> = info
            .decls
            .iter()
            .zip(args)
            .map(|(decl, arg)| {
                let name = match decl {
                    ParamDecl::Type { name, .. } | ParamDecl::Value { name, .. } => name.as_str(),
                };
                (name, arg)
            })
            .collect();
        self.eval_conformance_predicate(expr, &arguments)
    }

    fn eval_conformance_predicate(&self, expr: &Expr, args: &HashMap<&str, &TyArg>) -> bool {
        match &expr.kind {
            ExprKind::Bool(value) => *value,
            ExprKind::Prefix(PrefixOp::Not, value) => !self.eval_conformance_predicate(value, args),
            ExprKind::Infix(InfixOp::And, left, right) => {
                self.eval_conformance_predicate(left, args)
                    && self.eval_conformance_predicate(right, args)
            }
            ExprKind::Infix(InfixOp::Or, left, right) => {
                self.eval_conformance_predicate(left, args)
                    || self.eval_conformance_predicate(right, args)
            }
            ExprKind::Infix(op, left, right)
                if matches!(
                    op,
                    InfixOp::Eq
                        | InfixOp::Ne
                        | InfixOp::Lt
                        | InfixOp::Le
                        | InfixOp::Gt
                        | InfixOp::Ge
                ) =>
            {
                let Some(left) = conformance_operand(left, args) else {
                    return false;
                };
                let Some(right) = conformance_operand(right, args) else {
                    return false;
                };
                match (op, left, right) {
                    (InfixOp::Eq, left, right) => left == right,
                    (InfixOp::Ne, left, right) => left != right,
                    (InfixOp::Lt, CtValue::Int(left), CtValue::Int(right)) => left < right,
                    (InfixOp::Le, CtValue::Int(left), CtValue::Int(right)) => left <= right,
                    (InfixOp::Gt, CtValue::Int(left), CtValue::Int(right)) => left > right,
                    (InfixOp::Ge, CtValue::Int(left), CtValue::Int(right)) => left >= right,
                    _ => false,
                }
            }
            ExprKind::Call {
                name,
                args: operands,
                kwargs,
                ..
            } if name == "conforms_to" && kwargs.is_empty() && operands.len() == 2 => {
                let ExprKind::Identifier(type_name) = &operands[0].kind else {
                    return false;
                };
                let ExprKind::Identifier(trait_name) = &operands[1].kind else {
                    return false;
                };
                matches!(args.get(type_name.as_str()), Some(TyArg::Ty(ty)) if self.conforms_to(ty, trait_name))
            }
            _ => false,
        }
    }

    fn trait_refines(&self, candidate: &str, required: &str) -> bool {
        self.traits.get(candidate).is_some_and(|info| {
            info.refines
                .iter()
                .any(|parent| parent == required || self.trait_refines(parent, required))
        })
    }

    /// Explain the first actionable reason a built-in bound failed. This is
    /// intentionally evidence-oriented: marker traits name the field that
    /// prevents fieldwise synthesis, while operation traits name the operation
    /// promised by the bound.
    fn trait_failure_reason(&self, ty: &Ty, tr: &str) -> Option<String> {
        let Ty::Struct(name, _) = ty else {
            return builtin_trait_operation(tr)
                .map(|operation| format!("missing required operation '{operation}'"));
        };
        let info = self.structs.get(name)?;
        let field_failure = |predicate: &dyn Fn(&Ty) -> bool| {
            info.fields
                .iter()
                .find(|(_, field_ty)| !predicate(field_ty))
                .map(|(field, field_ty)| {
                    format!("field '{field}' has type '{field_ty}', which is not {tr}")
                })
        };
        match tr {
            "Copyable" => field_failure(&|field_ty| self.is_copyable(field_ty)),
            "ImplicitlyCopyable" => {
                if info.methods.contains_key("__copyinit__") {
                    Some(
                        "defines '__copyinit__'; implicit copying requires fieldwise synthesis"
                            .to_string(),
                    )
                } else {
                    field_failure(&|field_ty| self.is_implicitly_copyable(field_ty))
                }
            }
            "ImplicitlyDeletable" => {
                field_failure(&|field_ty| self.is_implicitly_deletable(field_ty))
            }
            _ => builtin_trait_operation(tr)
                .map(|operation| format!("missing required operation '{operation}'")),
        }
    }

    /// Whether a value of this type may be **copied** (implicitly duplicated).
    /// Mojo is move-only by default: scalars and the built-in value types are
    /// Copyable, but a `struct` is Copyable only if it declares Copyable/
    /// ImplicitlyCopyable conformance **or defines `__copyinit__`**, and a type
    /// parameter only if bounded by Copyable/ImplicitlyCopyable.
    fn is_copyable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::List(element) | Ty::Set(element) => self.is_copyable(element),
            Ty::Dict(key, value) => self.is_copyable(key) && self.is_copyable(value),
            Ty::Tuple(elements) => elements.iter().all(|element| self.is_copyable(element)),
            Ty::Variant(alternatives) => alternatives
                .iter()
                .all(|alternative| self.is_copyable(alternative)),
            Ty::Struct(name, _) => self
                .structs
                .get(name)
                .map(|s| {
                    s.conforms
                        .iter()
                        .any(|c| matches!(c.as_str(), "Copyable" | "ImplicitlyCopyable"))
                        || s.methods.contains_key("__copyinit__")
                })
                .unwrap_or(true),
            Ty::Param { bounds, .. } => bounds
                .iter()
                .any(|b| matches!(b.as_str(), "Copyable" | "ImplicitlyCopyable")),
            // Scalars, `String`, `List`/`Tuple`/`Simd`/`Range`, `Error`, closures,
            // and `Self` are treated as copyable (element-wise copyability of
            // aggregates is not modeled).
            _ => true,
        }
    }

    /// Compiler-generated keyword collectors use the bundled self-hosted
    /// `StringDict`. Its current `List`/`DictEntry` implementation is copy-based,
    /// so accept only element types that it can store without duplicating a
    /// linear value. Reference-bearing values also wait for collector origin
    /// metadata instead of silently losing their loans at the call boundary.
    fn kwargs_collector_ty(&self, element: Ty, context: &str) -> Result<Ty, TypeError> {
        if !self.is_copyable(&element) {
            return Err(TypeError::TraitNotSatisfied {
                param: "V".to_string(),
                ty: element.to_string(),
                trait_name: "Copyable".to_string(),
                reason: Some(format!(
                    "{context} is materialized as StringDict[V], whose current storage is copy-based"
                )),
            });
        }
        if self.type_contains_reference(&element) {
            return Err(TypeError::Unsupported(format!(
                "{context} cannot contain references until keyword collectors carry origin metadata"
            )));
        }
        Ok(Ty::Struct(
            "StringDict".to_string(),
            vec![TyArg::Ty(element)],
        ))
    }

    /// `ImplicitlyCopyable` is stronger than `Copyable`: it means the type can be
    /// copied by the ordinary implicit copy path, not only by an explicit custom
    /// copy constructor. Structs opt in by declaring the marker, and fieldwise
    /// conformance requires all fields to be implicitly copyable.
    fn is_implicitly_copyable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::List(element) | Ty::Set(element) => self.is_implicitly_copyable(element),
            Ty::Dict(key, value) => {
                self.is_implicitly_copyable(key) && self.is_implicitly_copyable(value)
            }
            Ty::Tuple(elements) => elements
                .iter()
                .all(|element| self.is_implicitly_copyable(element)),
            Ty::Variant(alternatives) => alternatives
                .iter()
                .all(|alternative| self.is_implicitly_copyable(alternative)),
            Ty::Struct(name, _) => self.structs.get(name).is_some_and(|s| {
                s.conforms.iter().any(|c| c == "ImplicitlyCopyable")
                    && self.struct_implicitly_copyable_conformance_ok(name)
            }),
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == "ImplicitlyCopyable"),
            _ => true,
        }
    }

    fn is_movable(&self, _ty: &Ty) -> bool {
        // The current ownership model supports moving every initialized value.
        true
    }

    fn is_implicitly_deletable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::List(element) | Ty::Set(element) => self.is_implicitly_deletable(element),
            Ty::Dict(key, value) => {
                self.is_implicitly_deletable(key) && self.is_implicitly_deletable(value)
            }
            Ty::Tuple(elements) => elements
                .iter()
                .all(|element| self.is_implicitly_deletable(element)),
            Ty::Variant(alternatives) => alternatives
                .iter()
                .all(|alternative| self.is_implicitly_deletable(alternative)),
            Ty::Struct(name, args) => self.structs.get(name).is_none_or(|info| {
                if info.conforms.iter().any(|tr| tr == "ImplicitlyDeletable") {
                    self.struct_conformance_applies(name, args, "ImplicitlyDeletable")
                } else {
                    true
                }
            }),
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == "ImplicitlyDeletable"),
            _ => true,
        }
    }

    fn is_hashable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Variant(alternatives) => alternatives
                .iter()
                .all(|alternative| self.is_hashable(alternative)),
            Ty::Struct(name, _) => self.structs.get(name).is_some_and(|s| {
                s.conforms.iter().any(|c| c == "Hashable") || s.methods.contains_key("__hash__")
            }),
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == "Hashable"),
            _ => builtin_hashable_ty(ty),
        }
    }

    fn is_comparable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == "Comparable"),
            _ => is_numeric_like(ty),
        }
    }

    fn struct_copyable_conformance_ok(&self, name: &str) -> bool {
        let Some(info) = self.structs.get(name) else {
            return false;
        };
        info.methods.contains_key("__copyinit__")
            || info.fields.iter().all(|(_, ty)| self.is_copyable(ty))
    }

    fn struct_implicitly_copyable_conformance_ok(&self, name: &str) -> bool {
        let Some(info) = self.structs.get(name) else {
            return false;
        };
        !info.methods.contains_key("__copyinit__")
            && info
                .fields
                .iter()
                .all(|(_, ty)| self.is_implicitly_copyable(ty))
    }

    /// At a **consuming** position (binding a value to a new place, passing it by
    /// value, returning it, …): a non-Copyable value that is a *place* (names an
    /// existing binding) is being copied — reject it unless it was transferred with
    /// `^` (which is a move, not a place). `context` names the site for the error.
    fn check_consuming(&self, expr: &Expr, ty: &Ty, context: &str) -> Result<(), TypeError> {
        // A `^` transfer is `Expr::Transfer`, not a place, so it is naturally
        // exempt. A fresh temporary (a call result, a literal, an operator) is not a
        // place either — moving it is free.
        if is_place_expr(expr) && !self.is_copyable(ty) {
            return Err(TypeError::NonCopyable {
                ty: ty.to_string(),
                context: context.to_string(),
            });
        }
        Ok(())
    }

    /// Bind `name` in the innermost scope. Repeated function declarations form an
    /// overload set when their call shapes differ; other same-scope repeats remain
    /// redeclarations.
    fn declare(&mut self, name: &str, ty: Ty) -> Result<(), TypeError> {
        self.declare_with_mutability(name, ty, true)
    }

    fn declare_immutable(&mut self, name: &str, ty: Ty) -> Result<(), TypeError> {
        self.declare_with_mutability(name, ty, false)
    }

    fn declare_with_mutability(
        &mut self,
        name: &str,
        ty: Ty,
        mutable: bool,
    ) -> Result<(), TypeError> {
        let nested_scope = self.scopes.len() > 1;
        let scope = self.scopes.last_mut().ok_or_else(|| {
            TypeError::InvariantViolation("checker scope stack is empty".to_string())
        })?;
        if let Some(existing) = scope.get_mut(name) {
            if let Some(mut candidates) = overload_candidates(existing, &ty) {
                if nested_scope {
                    return Err(TypeError::Unsupported("overloaded nested def".to_string()));
                }
                if candidates
                    .iter()
                    .any(|candidate| same_callable_signature(candidate, &ty))
                {
                    return Err(TypeError::Redeclaration(name.to_string()));
                }
                candidates.push(ty);
                *existing = Ty::Overload(candidates);
                return Ok(());
            }
            return Err(TypeError::Redeclaration(name.to_string()));
        }
        scope.insert(name.to_string(), ty);
        self.mutable_scopes
            .last_mut()
            .ok_or_else(|| {
                TypeError::InvariantViolation("checker mutability scope stack is empty".to_string())
            })?
            .insert(name.to_string(), mutable);
        let owner = crate::origin::OwnerId(self.next_owner);
        self.next_owner = self.next_owner.checked_add(1).ok_or_else(|| {
            TypeError::InvariantViolation("checker exhausted binding identities".to_string())
        })?;
        self.owner_scopes
            .last_mut()
            .ok_or_else(|| {
                TypeError::InvariantViolation("checker owner scope stack is empty".to_string())
            })?
            .insert(name.to_string(), owner);
        Ok(())
    }

    fn declare_function_implicit(&mut self, name: &str, ty: Ty) -> Result<(), TypeError> {
        let scope_index = self
            .function_bases
            .last()
            .copied()
            .unwrap_or(self.scopes.len().saturating_sub(1));
        if self.scopes[scope_index].contains_key(name) {
            return Err(TypeError::Redeclaration(name.to_string()));
        }
        self.scopes[scope_index].insert(name.to_string(), ty);
        self.mutable_scopes[scope_index].insert(name.to_string(), true);
        let owner = crate::origin::OwnerId(self.next_owner);
        self.next_owner = self.next_owner.checked_add(1).ok_or_else(|| {
            TypeError::InvariantViolation("checker exhausted binding identities".to_string())
        })?;
        self.owner_scopes[scope_index].insert(name.to_string(), owner);
        Ok(())
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.mutable_scopes.push(HashMap::new());
        self.owner_scopes.push(HashMap::new());
        self.aggregate_origin_scopes.push(HashMap::new());
        self.reference_parameter_scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if let Some(owners) = self.owner_scopes.last() {
            let ids: HashSet<_> = owners.values().copied().collect();
            self.uninitialized
                .borrow_mut()
                .retain(|owner| !ids.contains(owner));
        }
        self.scopes.pop();
        self.mutable_scopes.pop();
        self.owner_scopes.pop();
        self.aggregate_origin_scopes.pop();
        self.reference_parameter_scopes.pop();
    }

    fn is_binding_mutable(&self, name: &str) -> bool {
        self.mutable_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .unwrap_or(true)
    }

    /// The type to declare for an **inferred** binding — `var x = e` (no
    /// annotation) or a var-less `x = e`. A numeric literal materializes to its
    /// default kind (`default_literal`); a value that cannot live in a named
    /// binding is rejected: a closure (`ClosureEscape`, matching `return`/reassign)
    /// or the non-first-class `range` (which has no annotation and only belongs in
    /// a `for` header).
    fn inferred_binding_ty(&self, value_ty: &Ty, name: &str) -> Result<Ty, TypeError> {
        match value_ty {
            Ty::Func { .. } | Ty::GenericFunc { .. } | Ty::Overload(_) => {
                Err(TypeError::ClosureEscape)
            }
            Ty::Range => Err(TypeError::TypeMismatch {
                expected: "a storable type".to_string(),
                found: "range".to_string(),
                context: format!("inferred type of '{}'", name),
            }),
            other => Ok(default_literal(other)),
        }
    }

    /// Look up `name`, walking outward through the scope chain (lexical lookup).
    fn lookup(&self, name: &str) -> Option<&Ty> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn binding_scope(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rposition(|scope| scope.contains_key(name))
    }

    fn check_capture_access(&self, name: &str, writing: bool) -> Result<(), TypeError> {
        let contexts = self.capture_contexts.borrow();
        let Some(policy) = contexts.last() else {
            return Ok(());
        };
        let Some(scope) = self.binding_scope(name) else {
            return Ok(());
        };
        // Locals/parameters of this closure and module globals are not captures.
        if scope >= policy.base || scope == 0 || name == policy.function_name {
            return Ok(());
        }
        let kind = policy
            .entries
            .get(name)
            .copied()
            .or_else(|| policy.default_read.then_some(crate::ast::CaptureKind::Read));
        match (kind, writing) {
            (Some(crate::ast::CaptureKind::Mut), _)
            | (Some(crate::ast::CaptureKind::Move), false)
            | (Some(crate::ast::CaptureKind::Read), false) => Ok(()),
            (Some(_), true) => Err(TypeError::ImmutableBinding(name.to_string())),
            (None, _) => Err(TypeError::Unsupported(format!(
                "nested function must explicitly capture '{name}' with unified {{...}}"
            ))),
        }
    }

    fn lookup_owner(&self, name: &str) -> Option<crate::origin::OwnerId> {
        self.owner_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn owner_is_mutable(&self, owner: crate::origin::OwnerId) -> bool {
        self.owner_scopes
            .iter()
            .zip(&self.mutable_scopes)
            .rev()
            .find_map(|(owners, mutability)| {
                owners
                    .iter()
                    .find(|(_, id)| **id == owner)
                    .and_then(|(name, _)| mutability.get(name).copied())
            })
            .unwrap_or(false)
    }

    /// Convert a source place into the stable, projection-sensitive identity
    /// used by checked origins. Index values are intentionally abstracted: the
    /// loan checker must conservatively treat arbitrary indices as overlapping.
    fn origin_place(&self, expr: &Expr) -> Result<crate::origin::OriginPlace, TypeError> {
        use crate::origin::{OriginPlace, OriginSeg};
        match &expr.kind {
            ExprKind::Identifier(name) => {
                let root = self
                    .lookup_owner(name)
                    .ok_or_else(|| TypeError::UndefinedVariable(name.clone()))?;
                Ok(OriginPlace {
                    root,
                    path: Vec::new(),
                })
            }
            ExprKind::Member { object, field } => {
                let mut place = self.origin_place(object)?;
                place.path.push(OriginSeg::Field(field.clone()));
                Ok(place)
            }
            ExprKind::Index { object, .. } => {
                let mut place = self.origin_place(object)?;
                place.path.push(OriginSeg::AnyIndex);
                Ok(place)
            }
            ExprKind::TypeApply { name, .. }
                if self
                    .variant_operations
                    .borrow()
                    .get(&expr.source_span())
                    .is_some_and(|operation| {
                        matches!(
                            operation,
                            crate::checked::SemanticAdjustment::VariantProject { .. }
                        )
                    }) =>
            {
                let root = self
                    .lookup_owner(name)
                    .ok_or_else(|| TypeError::UndefinedVariable(name.clone()))?;
                // The origin algebra currently has no tag projection. Borrowing
                // a payload therefore loans the whole Variant, which is safe and
                // prevents changing its active alternative while the ref lives.
                Ok(OriginPlace {
                    root,
                    path: Vec::new(),
                })
            }
            _ => Err(TypeError::Unsupported(
                "reference binding to a non-place expression".to_string(),
            )),
        }
    }

    fn lookup_aggregate_origins(&self, name: &str) -> Vec<crate::origin::Origin> {
        self.aggregate_origin_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
            .unwrap_or_default()
    }

    fn set_aggregate_origins(&mut self, name: &str, origins: Vec<crate::origin::Origin>) {
        let Some(scope) = self.binding_scope(name) else {
            return;
        };
        if origins.is_empty() {
            self.aggregate_origin_scopes[scope].remove(name);
        } else {
            self.aggregate_origin_scopes[scope].insert(name.to_string(), origins);
        }
    }

    fn register_reference_parameter(&mut self, name: &str, referent: Ty, mutable: bool) {
        let Some(scope) = self.binding_scope(name) else {
            return;
        };
        let Some(owner) = self.lookup_owner(name) else {
            return;
        };
        self.reference_parameter_scopes[scope].insert(
            name.to_string(),
            crate::origin::RefTy {
                referent: Box::new(referent),
                origin: crate::origin::Origin::Place(crate::origin::OriginPlace {
                    root: owner,
                    path: Vec::new(),
                }),
                mutability: if mutable {
                    crate::origin::Mutability::Mutable
                } else {
                    crate::origin::Mutability::Immutable
                },
            },
        );
    }

    fn lookup_reference_parameter(&self, name: &str) -> Option<crate::origin::RefTy> {
        self.reference_parameter_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    fn type_contains_reference(&self, ty: &Ty) -> bool {
        fn contains(checker: &Checker, ty: &Ty, seen: &mut HashSet<String>) -> bool {
            match ty {
                Ty::Ref(_) => true,
                Ty::List(element) | Ty::Set(element) | Ty::Pointer { element, .. } => {
                    contains(checker, element, seen)
                }
                Ty::Dict(key, value) => {
                    contains(checker, key, seen) || contains(checker, value, seen)
                }
                Ty::Tuple(elements) => elements
                    .iter()
                    .any(|element| contains(checker, element, seen)),
                Ty::Variant(alternatives) => alternatives
                    .iter()
                    .any(|alternative| contains(checker, alternative, seen)),
                Ty::Struct(name, args) => {
                    let key = ty.to_string();
                    if !seen.insert(key.clone()) {
                        return false;
                    }
                    let result = checker.structs.get(name).is_some_and(|info| {
                        let subst = struct_subst(&info.decls, args);
                        info.fields
                            .iter()
                            .map(|(_, field)| substitute(field, &subst))
                            .any(|field| contains(checker, &field, seen))
                    });
                    seen.remove(&key);
                    result
                }
                _ => false,
            }
        }
        contains(self, ty, &mut HashSet::new())
    }

    fn type_contains_unsafe_any_pointer(ty: &Ty) -> bool {
        match ty {
            Ty::Pointer {
                origin: crate::origin::PointerOrigin::UnsafeAny { .. },
                ..
            } => true,
            Ty::Pointer { element, .. } | Ty::List(element) | Ty::Set(element) => {
                Self::type_contains_unsafe_any_pointer(element)
            }
            Ty::Dict(key, value) => {
                Self::type_contains_unsafe_any_pointer(key)
                    || Self::type_contains_unsafe_any_pointer(value)
            }
            Ty::Tuple(elements) | Ty::Variant(elements) => {
                elements.iter().any(Self::type_contains_unsafe_any_pointer)
            }
            _ => false,
        }
    }

    /// Origins retained by a value expression. This follows only value flow;
    /// ordinary arithmetic and reads cannot invent a stored reference handle.
    fn aggregate_origins(&self, expression: &Expr) -> Vec<crate::origin::Origin> {
        use crate::origin::Origin;

        fn append_unique(into: &mut Vec<Origin>, values: impl IntoIterator<Item = Origin>) {
            for value in values {
                if !into.contains(&value) {
                    into.push(value);
                }
            }
        }

        match &expression.kind {
            ExprKind::Identifier(name) => {
                let aggregate = self.lookup_aggregate_origins(name);
                if !aggregate.is_empty() {
                    return aggregate;
                }
                match self.lookup(name) {
                    Some(Ty::Ref(reference)) => vec![reference.origin.clone()],
                    _ => self
                        .lookup_reference_parameter(name)
                        .map(|reference| vec![reference.origin])
                        .unwrap_or_default(),
                }
            }
            ExprKind::Member { object, .. } => {
                let aggregate = self.aggregate_origins(object);
                if !aggregate.is_empty() {
                    aggregate
                } else {
                    self.infer_reference_value(expression)
                        .map(|reference| vec![reference.origin])
                        .unwrap_or_default()
                }
            }
            ExprKind::Transfer(inner) | ExprKind::Named { value: inner, .. } => {
                self.aggregate_origins(inner)
            }
            ExprKind::ListLit(values) | ExprKind::TupleLit(values) => {
                let mut result = Vec::new();
                for value in values {
                    append_unique(&mut result, self.aggregate_origins(value));
                }
                result
            }
            ExprKind::IfExpr {
                then_branch,
                else_branch,
                ..
            } => {
                let mut result = self.aggregate_origins(then_branch);
                append_unique(&mut result, self.aggregate_origins(else_branch));
                result
            }
            ExprKind::Call {
                name, args, kwargs, ..
            } => {
                let mut result = Vec::new();
                if let Some(info) = self.structs.get(name) {
                    if info.fieldwise_init {
                        let fields: Vec<Ty> =
                            info.fields.iter().map(|(_, ty)| ty.clone()).collect();
                        for (field, argument) in fields.iter().zip(args) {
                            if matches!(field, Ty::Ref(_)) {
                                if let Some(reference) = self.infer_reference_value(argument) {
                                    append_unique(&mut result, [reference.origin]);
                                }
                            } else {
                                append_unique(&mut result, self.aggregate_origins(argument));
                            }
                        }
                    } else if let Some(signature) =
                        info.methods.get("__init__").and_then(|signatures| {
                            signatures.iter().find(|sig| sig.params.len() == args.len())
                        })
                    {
                        let refs = signature.ref_params.clone();
                        for (index, argument) in args.iter().enumerate() {
                            if refs.get(index).is_some_and(Option::is_some) {
                                if let Ok(place) = self.origin_place(argument) {
                                    append_unique(&mut result, [Origin::Place(place)]);
                                }
                            } else {
                                append_unique(&mut result, self.aggregate_origins(argument));
                            }
                        }
                    }
                }
                if result.is_empty() {
                    for argument in args {
                        append_unique(&mut result, self.aggregate_origins(argument));
                    }
                    for argument in kwargs {
                        append_unique(&mut result, self.aggregate_origins(&argument.value));
                    }
                }
                result
            }
            ExprKind::Invoke { args, kwargs, .. } | ExprKind::MethodCall { args, kwargs, .. } => {
                let mut result = Vec::new();
                for argument in args {
                    append_unique(&mut result, self.aggregate_origins(argument));
                }
                for argument in kwargs {
                    append_unique(&mut result, self.aggregate_origins(&argument.value));
                }
                result
            }
            _ => Vec::new(),
        }
    }

    fn aggregate_origin_escapes(&self, origin: &crate::origin::Origin) -> bool {
        use crate::origin::Origin;
        let Some((base, allowed)) = self.aggregate_escape_contexts.last() else {
            return false;
        };
        match origin {
            Origin::Place(place) => {
                let scope = self
                    .owner_scopes
                    .iter()
                    .position(|owners| owners.values().any(|candidate| *candidate == place.root));
                scope.is_some_and(|scope| scope >= *base && !allowed.contains(&place.root))
            }
            Origin::Union(origins) => origins
                .iter()
                .any(|origin| self.aggregate_origin_escapes(origin)),
            Origin::Param(_) | Origin::Static | Origin::Untracked { .. } => false,
        }
    }

    /// Require `expr` to have type `Bool` (used for `if`/`while` conditions).
    fn expect_bool(&self, expr: &Expr, context: &str) -> Result<(), TypeError> {
        let ty = self.infer(expr)?;
        if ty == Ty::Bool {
            Ok(())
        } else {
            Err(TypeError::TypeMismatch {
                expected: "Bool".to_string(),
                found: ty.to_string(),
                context: context.to_string(),
            })
        }
    }

    /// Check a comprehension in its own lexical scope and cache its result type
    /// for ordinary read-only expression inference. Clauses are visited in
    /// source order, so a later iterable/filter sees earlier bindings while the
    /// produced key/value sees all generator bindings.
    fn check_comprehension(&mut self, expression: &Expr) -> Result<(), TypeError> {
        let ExprKind::Comprehension {
            kind,
            key,
            value,
            clauses,
        } = &expression.kind
        else {
            return Ok(());
        };

        let scope_base = self.scopes.len();
        let mut bindings = Vec::new();
        let result = (|| {
            for clause in clauses {
                match clause {
                    crate::ast::ComprehensionClause::For {
                        var,
                        reference,
                        owned,
                        iter,
                    } => {
                        self.register_named_bindings(iter)?;
                        let iter_ty = self.infer(iter)?;
                        let (elem_ty, protocol) = self.iteration_protocol(&iter_ty, *owned)?;
                        self.iteration_protocols
                            .borrow_mut()
                            .insert(iter.source_span(), protocol);
                        if *reference {
                            return Err(TypeError::Unsupported(
                                "reference bindings in collection comprehensions are not implemented; use an explicit `for ref` loop"
                                    .to_string(),
                            ));
                        }
                        if *owned && !matches!(iter.kind, ExprKind::Transfer(_)) {
                            return Err(TypeError::Unsupported(
                                "an owned comprehension binding requires a transferred iterable (`for var x in values^`)"
                                    .to_string(),
                            ));
                        }
                        if !*owned && matches!(iter.kind, ExprKind::Transfer(_)) {
                            return Err(TypeError::Unsupported(
                                "a transferred comprehension iterable requires an explicit `var` binding"
                                    .to_string(),
                            ));
                        }
                        if !*owned && !*reference && !self.is_copyable(&elem_ty) {
                            return Err(TypeError::NonCopyable {
                                ty: elem_ty.to_string(),
                                context:
                                    "immutable comprehension iteration; use `for var ... in ...^`"
                                        .to_string(),
                            });
                        }
                        let binding_ty = elem_ty;
                        // A generator binder scopes everything to its right, but
                        // not its own iterable. Giving every generator a lexical
                        // scope also permits a later generator to shadow the same
                        // spelling without changing an outer local.
                        self.push_scope();
                        self.declare_with_mutability(var, binding_ty, *owned)?;
                        let owner = self.lookup_owner(var).ok_or_else(|| {
                            TypeError::InvariantViolation(format!(
                                "comprehension binder '{var}' has no stable owner"
                            ))
                        })?;
                        bindings.push(crate::checked::CheckedComprehensionBinding {
                            name: var.clone(),
                            owner,
                            ty: self.lookup(var).cloned().ok_or_else(|| {
                                TypeError::InvariantViolation(format!(
                                    "comprehension binder '{var}' has no checked type"
                                ))
                            })?,
                            mutable: *owned,
                        });
                    }
                    crate::ast::ComprehensionClause::If(condition) => {
                        self.register_named_bindings(condition)?;
                        self.expect_bool(condition, "comprehension filter")?;
                    }
                }
            }

            if let Some(key) = key {
                self.register_named_bindings(key)?;
            }
            self.register_named_bindings(value)?;
            let value_ty = default_literal(&self.infer(value)?);
            self.check_consuming(value, &value_ty, "collection comprehension element")?;
            let result_ty = match kind {
                crate::ast::CollectionKind::List => Ty::List(Box::new(value_ty)),
                crate::ast::CollectionKind::Set => {
                    if !self.is_hashable(&value_ty) {
                        return Err(TypeError::TraitNotSatisfied {
                            param: "T".to_string(),
                            ty: value_ty.to_string(),
                            trait_name: "Hashable".to_string(),
                            reason: self.trait_failure_reason(&value_ty, "Hashable"),
                        });
                    }
                    Ty::Set(Box::new(value_ty))
                }
                crate::ast::CollectionKind::Dict => {
                    let key = key.as_ref().expect("dictionary comprehension has a key");
                    let key_ty = default_literal(&self.infer(key)?);
                    self.check_consuming(key, &key_ty, "dictionary comprehension key")?;
                    if !self.is_hashable(&key_ty) {
                        return Err(TypeError::TraitNotSatisfied {
                            param: "K".to_string(),
                            ty: key_ty.to_string(),
                            trait_name: "Hashable".to_string(),
                            reason: self.trait_failure_reason(&key_ty, "Hashable"),
                        });
                    }
                    Ty::Dict(Box::new(key_ty), Box::new(value_ty))
                }
            };
            self.expression_types
                .borrow_mut()
                .insert(expression.source_span(), result_ty);
            self.comprehension_bindings
                .borrow_mut()
                .insert(expression.source_span(), bindings.clone());
            Ok(())
        })();
        while self.scopes.len() > scope_base {
            self.pop_scope();
        }
        result
    }

    fn register_named_bindings(&mut self, expression: &Expr) -> Result<(), TypeError> {
        if matches!(expression.kind, ExprKind::Comprehension { .. }) {
            return self.check_comprehension(expression);
        }
        if let ExprKind::Named { name, value } = &expression.kind {
            self.register_named_bindings(value)?;
            let found = self.infer(value)?;
            let base = self
                .function_bases
                .last()
                .copied()
                .unwrap_or(self.scopes.len().saturating_sub(1));
            let existing = self.scopes[base..]
                .iter()
                .rev()
                .find_map(|scope| scope.get(name))
                .cloned();
            if let Some(existing) = existing {
                if !self.value_coerces(&found, &existing) {
                    return Err(TypeError::TypeMismatch {
                        expected: existing.to_string(),
                        found: found.to_string(),
                        context: format!("walrus assignment to '{name}'"),
                    });
                }
            } else {
                let declared = self.inferred_binding_ty(&found, name)?;
                self.declare_function_implicit(name, declared)?;
            }
            return Ok(());
        }
        match &expression.kind {
            ExprKind::Prefix(_, value) | ExprKind::Transfer(value) => {
                self.register_named_bindings(value)?
            }
            ExprKind::Infix(_, left, right)
            | ExprKind::Index {
                object: left,
                index: right,
            } => {
                self.register_named_bindings(left)?;
                self.register_named_bindings(right)?;
            }
            ExprKind::Call { args, kwargs, .. } => {
                for argument in args {
                    self.register_named_bindings(argument)?;
                }
                for argument in kwargs {
                    self.register_named_bindings(&argument.value)?;
                }
            }
            ExprKind::Invoke {
                callee,
                args,
                kwargs,
                ..
            } => {
                self.register_named_bindings(callee)?;
                for argument in args {
                    self.register_named_bindings(argument)?;
                }
                for argument in kwargs {
                    self.register_named_bindings(&argument.value)?;
                }
            }
            ExprKind::Member { object, .. } => self.register_named_bindings(object)?,
            ExprKind::MethodCall {
                object,
                args,
                kwargs,
                ..
            } => {
                self.register_named_bindings(object)?;
                for argument in args {
                    self.register_named_bindings(argument)?;
                }
                for argument in kwargs {
                    self.register_named_bindings(&argument.value)?;
                }
            }
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
                ..
            } => {
                self.register_named_bindings(object)?;
                for bound in [lower, upper, step].into_iter().flatten() {
                    self.register_named_bindings(bound)?;
                }
            }
            ExprKind::MultiIndex { object, args } => {
                self.register_named_bindings(object)?;
                for argument in args {
                    match argument {
                        crate::ast::SubscriptArg::Index(value) => {
                            self.register_named_bindings(value)?
                        }
                        crate::ast::SubscriptArg::Slice {
                            lower, upper, step, ..
                        } => {
                            for bound in [lower, upper, step].into_iter().flatten() {
                                self.register_named_bindings(bound)?;
                            }
                        }
                    }
                }
            }
            ExprKind::ListLit(values) | ExprKind::TupleLit(values) => {
                for value in values {
                    self.register_named_bindings(value)?;
                }
            }
            ExprKind::BraceLit(entries) => {
                for (key, value) in entries {
                    self.register_named_bindings(key)?;
                    if let Some(value) = value {
                        self.register_named_bindings(value)?;
                    }
                }
            }
            ExprKind::IfExpr {
                cond,
                then_branch,
                else_branch,
            } => {
                self.register_named_bindings(cond)?;
                self.register_named_bindings(then_branch)?;
                self.register_named_bindings(else_branch)?;
            }
            ExprKind::Compare { first, rest } => {
                self.register_named_bindings(first)?;
                for (_, value) in rest {
                    self.register_named_bindings(value)?;
                }
            }
            ExprKind::TString { parts, .. } => {
                for part in parts {
                    if let TStringPart::Expr(value) = part {
                        self.register_named_bindings(value)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn predeclare_implicit_assignments(&mut self, statements: &[Stmt]) -> Result<(), TypeError> {
        for statement in statements {
            match &statement.kind {
                StmtKind::Assign { name, value } if self.lookup(name).is_none() => {
                    let found = self.infer(value)?;
                    let declared = self.inferred_binding_ty(&found, name)?;
                    self.declare_function_implicit(name, declared)?;
                    if let Some(owner) = self.lookup_owner(name) {
                        self.uninitialized.borrow_mut().insert(owner);
                    }
                }
                StmtKind::If { branches, orelse } => {
                    for (_, body) in branches {
                        self.predeclare_implicit_assignments(body)?;
                    }
                    if let Some(body) = orelse {
                        self.predeclare_implicit_assignments(body)?;
                    }
                }
                StmtKind::While { body, .. } | StmtKind::For { body, .. } => {
                    self.predeclare_implicit_assignments(body)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn infer(&self, expr: &Expr) -> Result<Ty, TypeError> {
        let result = self.infer_impl(expr);
        if let Ok(ty) = &result {
            self.expression_types
                .borrow_mut()
                .insert(expr.source_span(), ty.clone());
            if let ExprKind::Call {
                name,
                param_args,
                args,
                ..
            } = &expr.kind
            {
                let dimensions = if name == "SIMD" {
                    self.simd_dims(param_args).ok().map(|(dtype, width)| {
                        let width = if width == -1 {
                            i64::try_from(args.len()).unwrap_or(0)
                        } else {
                            width
                        };
                        (dtype, width)
                    })
                } else {
                    Dtype::from_scalar_alias(name).map(|dtype| (dtype, 1))
                };
                if let Some(dimensions) = dimensions {
                    self.simd_constructions
                        .borrow_mut()
                        .insert(expr.source_span(), dimensions);
                }
            }
            let place_ty = match &expr.kind {
                ExprKind::Identifier(name) => self.lookup(name).cloned(),
                ExprKind::Member { object, field } => self.infer(object).ok().and_then(|base| {
                    let Ty::Struct(name, arguments) = base else {
                        return None;
                    };
                    let info = self.structs.get(&name)?;
                    let (_, field_ty) = info
                        .fields
                        .iter()
                        .find(|(candidate, _)| candidate == field)?;
                    Some(substitute(field_ty, &struct_subst(&info.decls, &arguments)))
                }),
                ExprKind::Index { object, index } => self
                    .index_storage_ty(object, index)
                    .or_else(|| Some(ty.clone())),
                ExprKind::TypeApply { .. }
                    if self
                        .variant_operations
                        .borrow()
                        .get(&expr.source_span())
                        .is_some_and(|operation| {
                            matches!(
                                operation,
                                crate::checked::SemanticAdjustment::VariantProject { .. }
                            )
                        }) =>
                {
                    Some(ty.clone())
                }
                _ => None,
            };
            if let Some(place_ty) = place_ty {
                self.expression_place_types
                    .borrow_mut()
                    .insert(expr.source_span(), place_ty);
            }
        }
        result
    }

    /// Type of a reference *handle* in a context that stores or forwards one.
    /// Ordinary expression inference intentionally reads through references.
    fn infer_reference_value(&self, expr: &Expr) -> Option<crate::origin::RefTy> {
        match &expr.kind {
            ExprKind::Identifier(name) => match self.lookup(name) {
                Some(Ty::Ref(reference)) => Some(reference.clone()),
                _ => self.lookup_reference_parameter(name),
            },
            ExprKind::Member { object, field } => {
                let object_ty = self.infer(object).ok()?;
                let Ty::Struct(name, _) = object_ty else {
                    return None;
                };
                self.structs
                    .get(&name)?
                    .fields
                    .iter()
                    .find_map(|(candidate, ty)| {
                        (candidate == field).then_some(ty).and_then(|ty| match ty {
                            Ty::Ref(reference) => Some(reference.clone()),
                            _ => None,
                        })
                    })
            }
            ExprKind::Index { object, index } => match self.index_storage_ty(object, index)? {
                Ty::Ref(reference) => Some(reference),
                _ => None,
            },
            _ => None,
        }
    }

    /// Infer the type stored by `expr` when the surrounding context expects a
    /// reference-bearing value.  Normal expression inference intentionally reads
    /// through a `ref`; aggregate construction instead needs to preserve the
    /// handle.  Keeping this contextual and recursive avoids changing ordinary
    /// expression semantics for tuples/lists which merely contain references.
    fn infer_storage_value(&self, expr: &Expr, expected: &Ty) -> Result<Ty, TypeError> {
        match expected {
            Ty::Ref(_) => self
                .infer_reference_value(expr)
                .map(Ty::Ref)
                .ok_or_else(|| TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: self
                        .infer(expr)
                        .map_or_else(|_| "<error>".to_string(), |ty| ty.to_string()),
                    context: "reference-valued aggregate element".to_string(),
                }),
            Ty::Tuple(expected_elements) => {
                let values = match &expr.kind {
                    ExprKind::TupleLit(values) => Some(values.as_slice()),
                    ExprKind::Call { name, args, .. } if name == "Tuple" => Some(args.as_slice()),
                    _ => None,
                };
                if let Some(values) = values {
                    if values.len() != expected_elements.len() {
                        return Err(TypeError::ArityMismatch {
                            name: "Tuple".to_string(),
                            expected: expected_elements.len(),
                            got: values.len(),
                        });
                    }
                    return values
                        .iter()
                        .zip(expected_elements)
                        .map(|(value, expected)| self.infer_storage_value(value, expected))
                        .collect::<Result<Vec<_>, _>>()
                        .map(Ty::Tuple);
                }
                self.infer(expr)
            }
            Ty::List(expected_element) => {
                let values = match &expr.kind {
                    ExprKind::ListLit(values) => Some(values.as_slice()),
                    ExprKind::Call { name, args, .. } if name == "List" => Some(args.as_slice()),
                    _ => None,
                };
                if let Some(values) = values {
                    for value in values {
                        let actual = self.infer_storage_value(value, expected_element)?;
                        if !Self::storage_value_coerces(&actual, expected_element) {
                            return Err(TypeError::TypeMismatch {
                                expected: expected_element.to_string(),
                                found: actual.to_string(),
                                context: "reference-valued list element".to_string(),
                            });
                        }
                    }
                    return Ok(Ty::List(expected_element.clone()));
                }
                self.infer(expr)
            }
            _ => self.infer(expr),
        }
    }

    /// Storage compatibility is ordinary coercion plus recursive reference
    /// compatibility.  A tracked origin parameter accepts a tracked place; an
    /// explicitly untracked field must only receive the same untracked kind.
    fn storage_value_coerces(from: &Ty, to: &Ty) -> bool {
        match (from, to) {
            (Ty::Ref(actual), Ty::Ref(expected)) => {
                coerces(&actual.referent, &expected.referent)
                    && (expected.mutability != crate::origin::Mutability::Mutable
                        || actual.mutability == crate::origin::Mutability::Mutable)
                    && match &expected.origin {
                        crate::origin::Origin::Untracked { mutable } => matches!(
                            &actual.origin,
                            crate::origin::Origin::Untracked {
                                mutable: actual_mutability
                            } if actual_mutability == mutable
                        ),
                        _ => !matches!(actual.origin, crate::origin::Origin::Untracked { .. }),
                    }
            }
            (Ty::Tuple(actual), Ty::Tuple(expected)) => {
                actual.len() == expected.len()
                    && actual
                        .iter()
                        .zip(expected)
                        .all(|(actual, expected)| Self::storage_value_coerces(actual, expected))
            }
            (Ty::List(actual), Ty::List(expected)) => Self::storage_value_coerces(actual, expected),
            _ => coerces(from, to),
        }
    }

    /// Mark every syntax leaf that must lower to a reference handle rather than
    /// an ordinary read-through value.  MIR consumes these checked adjustments;
    /// it never has to rediscover aggregate reference semantics from source AST.
    fn mark_reference_storage_uses(&self, expr: &Expr, expected: &Ty) {
        match expected {
            Ty::Ref(reference) => {
                self.reference_value_uses.borrow_mut().insert(
                    expr.source_span(),
                    reference.mutability == crate::origin::Mutability::Mutable,
                );
            }
            Ty::Tuple(expected_elements) => {
                let values = match &expr.kind {
                    ExprKind::TupleLit(values) => Some(values.as_slice()),
                    ExprKind::Call { name, args, .. } if name == "Tuple" => Some(args.as_slice()),
                    _ => None,
                };
                if let Some(values) = values {
                    for (value, expected) in values.iter().zip(expected_elements) {
                        self.mark_reference_storage_uses(value, expected);
                    }
                }
            }
            Ty::List(expected_element) => {
                let values = match &expr.kind {
                    ExprKind::ListLit(values) => Some(values.as_slice()),
                    ExprKind::Call { name, args, .. } if name == "List" => Some(args.as_slice()),
                    _ => None,
                };
                if let Some(values) = values {
                    for value in values {
                        self.mark_reference_storage_uses(value, expected_element);
                    }
                }
            }
            _ => {}
        }
    }

    fn infer_impl(&self, expr: &Expr) -> Result<Ty, TypeError> {
        match &expr.kind {
            ExprKind::Int(_) => Ok(Ty::IntLiteral),
            ExprKind::Float(_) => Ok(Ty::FloatLiteral),
            ExprKind::Bool(_) => Ok(Ty::Bool),
            ExprKind::Str(_) => Ok(Ty::String),
            ExprKind::None => Ok(Ty::None),
            ExprKind::Uninitialized => Err(TypeError::InvariantViolation(
                "uninitialized marker reached expression inference".to_string(),
            )),
            ExprKind::TypeValue(_) => Err(TypeError::Unsupported(
                "function types as compile-time values".to_string(),
            )),
            ExprKind::Spread(_) => Err(TypeError::Unsupported(
                "call spread outside a specialized type pack".to_string(),
            )),
            ExprKind::Invoke {
                callee,
                param_args,
                args,
                kwargs,
            } => {
                if let Some(result) =
                    self.infer_variant_invoke(expr.source_span(), callee, param_args, args, kwargs)
                {
                    return result;
                }
                let callable = self.infer(callee)?;
                let (ret, _, error) =
                    self.infer_callable_ty("<callable>", callable, param_args, args, kwargs)?;
                if let Some(error) = error.filter(|ty| *ty != Ty::Never) {
                    self.record_call_effect(expr.source_span(), error.clone());
                    self.require_error("call through a raising callable", error)?;
                }
                Ok(ret)
            }
            ExprKind::BraceLit(entries) => {
                if entries.is_empty() {
                    return Err(TypeError::Unsupported(
                        "an empty '{}' display needs a Dict[K, V] type annotation".to_string(),
                    ));
                }
                let dictionary = entries[0].1.is_some();
                if entries
                    .iter()
                    .any(|(_, value)| value.is_some() != dictionary)
                {
                    return Err(TypeError::Unsupported(
                        "set elements and dictionary key/value pairs cannot be mixed".to_string(),
                    ));
                }
                let keys = entries
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                let key_ty = self.infer_list_elem(&keys)?;
                for key in &keys {
                    self.check_consuming(key, &key_ty, "collection display element")?;
                }
                if !self.is_hashable(&key_ty) {
                    return Err(TypeError::TraitNotSatisfied {
                        param: "K".to_string(),
                        ty: key_ty.to_string(),
                        trait_name: "Hashable".to_string(),
                        reason: self.trait_failure_reason(&key_ty, "Hashable"),
                    });
                }
                if !dictionary {
                    return Ok(Ty::Set(Box::new(key_ty)));
                }
                let values = entries
                    .iter()
                    .filter_map(|(_, value)| value.clone())
                    .collect::<Vec<_>>();
                let value_ty = self.infer_list_elem(&values)?;
                for value in &values {
                    self.check_consuming(value, &value_ty, "dictionary display value")?;
                }
                Ok(Ty::Dict(Box::new(key_ty), Box::new(value_ty)))
            }
            ExprKind::Comprehension { .. } => self
                .expression_types
                .borrow()
                .get(&expr.source_span())
                .cloned()
                .ok_or_else(|| {
                    TypeError::InvariantViolation(
                        "comprehension reached inference before scoped checking".to_string(),
                    )
                }),
            ExprKind::Identifier(name) => {
                self.check_capture_access(name, false)?;
                if let Some(owner) = self.lookup_owner(name) {
                    self.expression_bindings
                        .borrow_mut()
                        .insert(expr.source_span(), owner);
                }
                if self
                    .lookup_owner(name)
                    .is_some_and(|owner| self.uninitialized.borrow().contains(&owner))
                {
                    return Err(TypeError::Unsupported(format!(
                        "variable '{name}' may be uninitialized"
                    )));
                }
                self.lookup(name)
                    .map(|ty| match ty {
                        Ty::Ref(reference) => (*reference.referent).clone(),
                        other => other.clone(),
                    })
                    .ok_or_else(|| TypeError::UndefinedVariable(name.clone()))
            }
            ExprKind::Prefix(op, operand) => self.infer_prefix(*op, operand),
            ExprKind::Infix(op, left, right) => self.infer_infix(*op, left, right),
            ExprKind::Call {
                name,
                param_args,
                args,
                kwargs,
            } => self.infer_call(expr.source_span(), name, param_args, args, kwargs),
            ExprKind::Member { object, field } => self.infer_member(object, field),
            ExprKind::MethodCall {
                object,
                method,
                args,
                kwargs,
            } => self.infer_method_call(expr.source_span(), object, method, args, kwargs),
            ExprKind::Index { object, index } => self.infer_index(object, index),
            // Transfer is identity for typing (ownership move is not modeled).
            ExprKind::Transfer(inner) => self.infer(inner),
            ExprKind::ListLit(elems) => Ok(Ty::List(Box::new(self.infer_list_elem(elems)?))),
            // A tuple literal keeps each element's own type (heterogeneous).
            ExprKind::TupleLit(elems) => {
                let tys = elems
                    .iter()
                    .map(|e| self.infer(e))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Ty::Tuple(tys))
            }
            // Walrus `name := value` types as `value`; MIR marks execution as
            // unsupported. The name is not bound here — `infer` is read-only — so
            // a program that *uses* the walrus-bound name later won't type-check.
            ExprKind::Named { value, .. } => self.infer(value),
            // Ternary `a if cond else b`: `cond` must be `Bool`; the branches must
            // have a common type (the result type).
            ExprKind::IfExpr {
                cond,
                then_branch,
                else_branch,
            } => {
                let ct = self.infer(cond)?;
                if ct != Ty::Bool {
                    return Err(TypeError::TypeMismatch {
                        expected: "Bool".to_string(),
                        found: ct.to_string(),
                        context: "conditional-expression condition".to_string(),
                    });
                }
                let tt = self.infer(then_branch)?;
                let et = self.infer(else_branch)?;
                common_branch_ty(&tt, &et).ok_or_else(|| TypeError::TypeMismatch {
                    expected: tt.to_string(),
                    found: et.to_string(),
                    context: "conditional-expression branches".to_string(),
                })
            }
            // Chained comparison `a < b < c`: each adjacent pair must compare to a
            // `Bool` (same rules as a single comparison); the result is `Bool`.
            ExprKind::Compare { first, rest } => {
                let mut left: &Expr = first;
                for (op, right) in rest {
                    if self.infer_infix(*op, left, right)? != Ty::Bool {
                        return Err(TypeError::BadOperator {
                            op: infix_symbol(*op).to_string(),
                            operands: "a chained comparison must compare to Bool".to_string(),
                        });
                    }
                    left = right;
                }
                Ok(Ty::Bool)
            }
            // Slice `object[lower:upper:step]` on a `List`/`String`: each present
            // bound must be `Int`; the result is the same sequence type.
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
                explicit_step,
            } => self.infer_slice_subscript(
                expr.source_span(),
                object,
                lower.as_deref(),
                upper.as_deref(),
                step.as_deref(),
                *explicit_step,
            ),
            ExprKind::MultiIndex { object, args } => {
                self.infer_multi_subscript(expr.source_span(), object, args)
            }
            ExprKind::TString { parts, .. } => {
                for part in parts {
                    if let TStringPart::Expr(value) = part {
                        let ty = self.infer(value)?;
                        if !self.conforms_to(&ty, "Writable") {
                            return Err(TypeError::TraitNotSatisfied {
                                param: "interpolation".to_string(),
                                ty: ty.to_string(),
                                trait_name: "Writable".to_string(),
                                reason: self.trait_failure_reason(&ty, "Writable"),
                            });
                        }
                    }
                }
                Ok(Ty::String)
            }
            // A parameterized type is not a runtime value; it is only valid as a
            // static-method receiver (`UnsafePointer[T].alloc(…)`), typed in
            // `infer_method_call`.
            ExprKind::TypeApply { name, args } => {
                if let Some(Ty::Variant(alternatives)) = self.lookup(name).cloned() {
                    self.check_capture_access(name, false)?;
                    let (index, result) = self.variant_alternative(&alternatives, args)?;
                    if let Some(owner) = self.lookup_owner(name) {
                        self.expression_bindings
                            .borrow_mut()
                            .insert(expr.source_span(), owner);
                    }
                    self.variant_operations.borrow_mut().insert(
                        expr.source_span(),
                        crate::checked::SemanticAdjustment::VariantProject {
                            alternatives,
                            index,
                        },
                    );
                    Ok(result)
                } else {
                    Err(TypeError::TypeMismatch {
                        expected: "a value".to_string(),
                        found: format!("the type '{name}[…]'"),
                        context: "a parameterized type is not a value".to_string(),
                    })
                }
            }
        }
    }

    /// Recognize compiler-known parameterized `Variant` operations. The parser
    /// preserves their type arguments on the invoke; checked metadata records
    /// every selected tag and whether the runtime operation is checked or unsafe.
    fn infer_variant_invoke(
        &self,
        span: SourceSpan,
        callee: &Expr,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Option<Result<Ty, TypeError>> {
        let ExprKind::Member { object, field } = &callee.kind else {
            return None;
        };
        let object_ty = match self.infer(object) {
            Ok(ty) => ty,
            Err(error) => return Some(Err(error)),
        };
        let Ty::Variant(alternatives) = object_ty else {
            return None;
        };
        if !matches!(
            field.as_str(),
            "isa"
                | "is_type_supported"
                | "set"
                | "take"
                | "unsafe_take"
                | "replace"
                | "unsafe_replace"
        ) {
            return None;
        }
        Some((|| {
            if !kwargs.is_empty() {
                return Err(TypeError::BadCall {
                    func: format!("Variant.{field}"),
                    reason: "keyword arguments are not supported".to_string(),
                });
            }
            match field.as_str() {
                "isa" => {
                    let (index, _) = self.variant_alternative(&alternatives, param_args)?;
                    if !args.is_empty() {
                        return Err(TypeError::ArityMismatch {
                            name: "Variant.isa".to_string(),
                            expected: 0,
                            got: args.len(),
                        });
                    }
                    self.variant_operations.borrow_mut().insert(
                        span,
                        crate::checked::SemanticAdjustment::VariantIs {
                            alternatives,
                            index,
                        },
                    );
                    Ok(Ty::Bool)
                }
                "is_type_supported" => {
                    if param_args.len() != 1 {
                        return Err(TypeError::WrongTypeArgCount {
                            name: "Variant.is_type_supported".to_string(),
                            expected: 1,
                            got: param_args.len(),
                        });
                    }
                    if !args.is_empty() {
                        return Err(TypeError::ArityMismatch {
                            name: "Variant.is_type_supported".to_string(),
                            expected: 0,
                            got: args.len(),
                        });
                    }
                    let requested =
                        self.type_param_argument(&param_args[0], "Variant.is_type_supported")?;
                    self.variant_operations.borrow_mut().insert(
                        span,
                        crate::checked::SemanticAdjustment::VariantTypeSupported {
                            supported: alternatives.contains(&requested),
                        },
                    );
                    Ok(Ty::Bool)
                }
                "set" => {
                    let (index, alternative) =
                        self.variant_alternative(&alternatives, param_args)?;
                    if args.len() != 1 {
                        return Err(TypeError::ArityMismatch {
                            name: "Variant.set".to_string(),
                            expected: 1,
                            got: args.len(),
                        });
                    }
                    self.check_place(object)?;
                    let actual = self.infer(&args[0])?;
                    if !self.record_implicit_conversion(&args[0], &actual, &alternative)? {
                        return Err(TypeError::TypeMismatch {
                            expected: alternative.to_string(),
                            found: actual.to_string(),
                            context: "argument to 'Variant.set'".to_string(),
                        });
                    }
                    self.check_consuming(&args[0], &actual, "argument to 'Variant.set'")?;
                    self.variant_operations.borrow_mut().insert(
                        span,
                        crate::checked::SemanticAdjustment::VariantSet {
                            alternatives,
                            index,
                        },
                    );
                    Ok(Ty::None)
                }
                "take" | "unsafe_take" => {
                    let (index, alternative) =
                        self.variant_alternative(&alternatives, param_args)?;
                    if !args.is_empty() {
                        return Err(TypeError::ArityMismatch {
                            name: format!("Variant.{field}"),
                            expected: 0,
                            got: args.len(),
                        });
                    }
                    if !is_place_expr(object) {
                        return Err(TypeError::BadCall {
                            func: format!("Variant.{field}"),
                            reason: "consuming receiver must be an owned place".to_string(),
                        });
                    }
                    self.variant_operations.borrow_mut().insert(
                        span,
                        crate::checked::SemanticAdjustment::VariantTake {
                            alternatives,
                            index,
                            checked: field == "take",
                        },
                    );
                    Ok(alternative)
                }
                "replace" | "unsafe_replace" => {
                    if param_args.len() != 2 {
                        return Err(TypeError::WrongTypeArgCount {
                            name: format!("Variant.{field}"),
                            expected: 2,
                            got: param_args.len(),
                        });
                    }
                    if args.len() != 1 {
                        return Err(TypeError::ArityMismatch {
                            name: format!("Variant.{field}"),
                            expected: 1,
                            got: args.len(),
                        });
                    }
                    let input = self.type_param_argument(&param_args[0], "Variant.replace")?;
                    let output = self.type_param_argument(&param_args[1], "Variant.replace")?;
                    let input_index = alternatives
                        .iter()
                        .position(|alternative| alternative == &input)
                        .ok_or_else(|| TypeError::TypeMismatch {
                            expected: format!("one of {}", Ty::Variant(alternatives.clone())),
                            found: input.to_string(),
                            context: "Variant replacement input type".to_string(),
                        })?;
                    let output_index = alternatives
                        .iter()
                        .position(|alternative| alternative == &output)
                        .ok_or_else(|| TypeError::TypeMismatch {
                            expected: format!("one of {}", Ty::Variant(alternatives.clone())),
                            found: output.to_string(),
                            context: "Variant replacement output type".to_string(),
                        })?;
                    self.check_place(object)?;
                    if field == "replace" && !self.is_implicitly_deletable(&input) {
                        return Err(TypeError::TraitNotSatisfied {
                            param: "Tin".to_string(),
                            ty: input.to_string(),
                            trait_name: "ImplicitlyDeletable".to_string(),
                            reason: Some(
                                "checked replacement must be able to delete the incoming value if the active tag mismatches"
                                    .to_string(),
                            ),
                        });
                    }
                    let actual = self.infer(&args[0])?;
                    if !self.record_implicit_conversion(&args[0], &actual, &input)? {
                        return Err(TypeError::TypeMismatch {
                            expected: input.to_string(),
                            found: actual.to_string(),
                            context: format!("argument to 'Variant.{field}'"),
                        });
                    }
                    self.check_consuming(
                        &args[0],
                        &actual,
                        &format!("argument to 'Variant.{field}'"),
                    )?;
                    self.variant_operations.borrow_mut().insert(
                        span,
                        crate::checked::SemanticAdjustment::VariantReplace {
                            alternatives,
                            input_index,
                            output_index,
                            checked: field == "replace",
                        },
                    );
                    Ok(output)
                }
                _ => unreachable!("checked Variant operation"),
            }
        })())
    }

    fn variant_alternative(
        &self,
        alternatives: &[Ty],
        args: &[crate::ast::ParamArg],
    ) -> Result<(usize, Ty), TypeError> {
        if args.len() != 1 {
            return Err(TypeError::WrongTypeArgCount {
                name: "Variant operation".to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        let requested = self.type_param_argument(&args[0], "Variant operation")?;
        alternatives
            .iter()
            .position(|alternative| alternative == &requested)
            .map(|index| (index, requested.clone()))
            .ok_or_else(|| TypeError::TypeMismatch {
                expected: format!("one of {}", Ty::Variant(alternatives.to_vec())),
                found: requested.to_string(),
                context: "Variant operation type".to_string(),
            })
    }

    /// Infer a collection display against an expected collection type. Empty
    /// displays need this context to choose their family, and non-empty numeric
    /// displays need it to materialize elements (for example `{1, 2}` as
    /// `Set[Float64]`) instead of merely coercing the aggregate shell.
    ///
    /// Candidate scoring uses `record = false`; after overload selection the
    /// checked path repeats this with `record = true`, retaining the chosen root
    /// type and any element conversions for HIR/MIR.
    fn infer_with_expected(
        &self,
        expression: &Expr,
        expected: &Ty,
        record: bool,
    ) -> Result<Ty, TypeError> {
        let elements: Option<Vec<(&Expr, &Ty, &'static str)>> = match (&expression.kind, expected) {
            (ExprKind::ListLit(values), Ty::List(element)) => Some(
                values
                    .iter()
                    .map(|value| (value, element.as_ref(), "collection display element"))
                    .collect(),
            ),
            (ExprKind::BraceLit(entries), Ty::Set(element))
                if entries.iter().all(|(_, value)| value.is_none()) =>
            {
                Some(
                    entries
                        .iter()
                        .map(|(value, _)| (value, element.as_ref(), "collection display element"))
                        .collect(),
                )
            }
            (ExprKind::BraceLit(entries), Ty::Dict(key, value))
                if entries.is_empty() || entries.iter().all(|(_, value)| value.is_some()) =>
            {
                Some(
                    entries
                        .iter()
                        .flat_map(|(actual_key, actual_value)| {
                            [
                                (actual_key, key.as_ref(), "dictionary display key"),
                                (
                                    actual_value
                                        .as_ref()
                                        .expect("contextual dictionary entry has a value"),
                                    value.as_ref(),
                                    "dictionary display value",
                                ),
                            ]
                        })
                        .collect(),
                )
            }
            _ => None,
        };

        let Some(elements) = elements else {
            return self.infer(expression);
        };
        match expected {
            Ty::Set(element) if !self.is_hashable(element) => {
                return Err(TypeError::TraitNotSatisfied {
                    param: "T".to_string(),
                    ty: element.to_string(),
                    trait_name: "Hashable".to_string(),
                    reason: self.trait_failure_reason(element, "Hashable"),
                });
            }
            Ty::Dict(key, _) if !self.is_hashable(key) => {
                return Err(TypeError::TraitNotSatisfied {
                    param: "K".to_string(),
                    ty: key.to_string(),
                    trait_name: "Hashable".to_string(),
                    reason: self.trait_failure_reason(key, "Hashable"),
                });
            }
            _ => {}
        }

        for (value, element, context) in elements {
            let actual = self.infer_with_expected(value, element, record)?;
            let compatible = if record {
                self.record_implicit_conversion(value, &actual, element)?
            } else {
                self.value_coerces(&actual, element)
                    || self.implicit_conversion_target(&actual, element)?.is_some()
            };
            if !compatible {
                return Err(TypeError::TypeMismatch {
                    expected: element.to_string(),
                    found: actual.to_string(),
                    context: context.to_string(),
                });
            }
            self.check_consuming(value, &actual, context)?;
        }
        if record {
            self.expression_types
                .borrow_mut()
                .insert(expression.source_span(), expected.clone());
        }
        Ok(expected.clone())
    }

    /// Infer the common element type of a non-empty list of expressions: numeric
    /// elements unify (widening literals), non-numeric elements must match; the
    /// result is materialized (a literal → its concrete default).
    fn infer_list_elem(&self, elems: &[Expr]) -> Result<Ty, TypeError> {
        let mut acc: Option<Ty> = None;
        for e in elems {
            let ty = self.infer(e)?;
            acc = Some(match acc {
                None => ty,
                Some(cur) => common_elem(&cur, &ty).ok_or_else(|| TypeError::TypeMismatch {
                    expected: cur.to_string(),
                    found: ty.to_string(),
                    context: "list element".to_string(),
                })?,
            });
        }
        // A non-empty literal always sets `acc`; empty is handled by the caller.
        let element = acc.ok_or_else(|| {
            TypeError::InvariantViolation("empty list reached non-empty inference".to_string())
        })?;
        Ok(default_literal(&element))
    }

    /// Type a `List` construction: `List[T](args)` (explicit element type) or
    /// `List(args)` (element type inferred from the arguments — non-empty).
    fn infer_list_construction(
        &self,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        if !param_args.is_empty() {
            let Ty::List(elem) = self.list_type(param_args)? else {
                return Err(TypeError::InvariantViolation(
                    "List type construction did not produce a list".to_string(),
                ));
            };
            for (i, arg) in args.iter().enumerate() {
                let aty = if self.type_contains_reference(&elem) {
                    self.infer_storage_value(arg, &elem)?
                } else {
                    self.infer(arg)?
                };
                if !Self::storage_value_coerces(&aty, &elem) {
                    return Err(TypeError::TypeMismatch {
                        expected: elem.to_string(),
                        found: aty.to_string(),
                        context: format!("element {} of List", i + 1),
                    });
                }
                self.mark_reference_storage_uses(arg, &elem);
            }
            return Ok(Ty::List(elem));
        }
        if args.is_empty() {
            return Err(TypeError::CannotInferTypeParam {
                name: "List".to_string(),
                param: "T".to_string(),
            });
        }
        Ok(Ty::List(Box::new(self.infer_list_elem(args)?)))
    }

    /// Type `Tuple(args...)` (element types inferred) and
    /// `Tuple[T1, ..., Tn](args...)` (fixed, element-wise checked).
    fn infer_tuple_construction(
        &self,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        if param_args.is_empty() {
            return args
                .iter()
                .map(|arg| self.infer(arg))
                .collect::<Result<Vec<_>, _>>()
                .map(Ty::Tuple);
        }
        let Ty::Tuple(elements) = self.tuple_type(param_args)? else {
            return Err(TypeError::InvariantViolation(
                "Tuple type construction did not produce a tuple".to_string(),
            ));
        };
        if elements.len() != args.len() {
            return Err(TypeError::ArityMismatch {
                name: "Tuple".to_string(),
                expected: elements.len(),
                got: args.len(),
            });
        }
        for (index, (argument, expected)) in args.iter().zip(&elements).enumerate() {
            let actual = if self.type_contains_reference(expected) {
                self.infer_storage_value(argument, expected)?
            } else {
                self.infer(argument)?
            };
            if !Self::storage_value_coerces(&actual, expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: actual.to_string(),
                    context: format!("element {} of Tuple", index + 1),
                });
            }
            self.mark_reference_storage_uses(argument, expected);
        }
        Ok(Ty::Tuple(elements))
    }

    fn infer_variant_construction(
        &self,
        span: SourceSpan,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Ty, TypeError> {
        if !kwargs.is_empty() {
            return Err(TypeError::BadCall {
                func: "Variant".to_string(),
                reason: "keyword arguments are not supported".to_string(),
            });
        }
        if args.len() != 1 {
            return Err(TypeError::ArityMismatch {
                name: "Variant".to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        let Ty::Variant(alternatives) = self.variant_type(param_args)? else {
            return Err(TypeError::InvariantViolation(
                "Variant type construction did not produce a variant".to_string(),
            ));
        };
        let actual = self.infer(&args[0])?;
        let exact: Vec<_> = alternatives
            .iter()
            .enumerate()
            .filter(|(_, alternative)| **alternative == actual)
            .collect();
        // A bare literal first materializes to its ordinary scalar type. Current
        // Mojo therefore chooses `Int` for `Variant[Int, UInt](1)` instead of
        // treating both numeric conversions as equally good.
        let materialized = default_literal(&actual);
        let materialized_exact: Vec<_> = alternatives
            .iter()
            .enumerate()
            .filter(|(_, alternative)| **alternative == materialized)
            .collect();
        let candidates: Vec<_> = if !exact.is_empty() {
            exact
        } else if !materialized_exact.is_empty() {
            materialized_exact
        } else {
            alternatives
                .iter()
                .enumerate()
                .filter(|(_, alternative)| self.value_coerces(&actual, alternative))
                .collect()
        };
        let [(index, selected)] = candidates.as_slice() else {
            return Err(TypeError::BadCall {
                func: "Variant".to_string(),
                reason: if candidates.is_empty() {
                    format!("'{actual}' is not one of its declared alternatives")
                } else {
                    format!("'{actual}' matches more than one declared alternative")
                },
            });
        };
        if !self.record_implicit_conversion(&args[0], &actual, selected)? {
            return Err(TypeError::TypeMismatch {
                expected: selected.to_string(),
                found: actual.to_string(),
                context: "Variant payload".to_string(),
            });
        }
        self.variant_operations.borrow_mut().insert(
            span,
            crate::checked::SemanticAdjustment::ConstructVariant {
                alternatives: alternatives.clone(),
                index: *index,
            },
        );
        Ok(Ty::Variant(alternatives))
    }

    /// Validate an assignment **place** and return the type stored there. A place
    /// is a chain of field (`.x`) and index (`[i]`) accesses over a root that
    /// must be a mutable location: any variable, or `self` in a `mut self`
    /// method. Recursing on the object of each step verifies the whole chain is
    /// rooted at a mutable place (so `foo().x = e` or `self.x` in a read-only
    /// method are rejected). SIMD lane writes are not supported yet.
    fn check_place(&self, place: &Expr) -> Result<Ty, TypeError> {
        let result = self.check_place_impl(place);
        if let Ok(ty) = &result {
            self.expression_types
                .borrow_mut()
                .insert(place.source_span(), ty.clone());
            if let ExprKind::Identifier(name) = &place.kind
                && let Some(owner) = self.lookup_owner(name)
            {
                self.expression_bindings
                    .borrow_mut()
                    .insert(place.source_span(), owner);
            }
            let storage_ty = self.place_storage_ty(place).or_else(|| Some(ty.clone()));
            if let Some(storage_ty) = storage_ty {
                self.expression_place_types
                    .borrow_mut()
                    .insert(place.source_span(), storage_ty);
            }
        }
        result
    }

    fn place_storage_ty(&self, place: &Expr) -> Option<Ty> {
        match &place.kind {
            ExprKind::Identifier(name) => self.lookup(name).cloned(),
            ExprKind::Member { object, field } => self.infer(object).ok().and_then(|base| {
                let Ty::Struct(name, arguments) = base else {
                    return None;
                };
                let info = self.structs.get(&name)?;
                let (_, field_ty) = info
                    .fields
                    .iter()
                    .find(|(candidate, _)| candidate == field)?;
                Some(substitute(field_ty, &struct_subst(&info.decls, &arguments)))
            }),
            ExprKind::Index { object, index } => self.index_storage_ty(object, index),
            _ => None,
        }
    }

    /// Type physically stored at an index place, before the usual read-through
    /// rule for a reference element.  Tuple indices are compile-time constants,
    /// while homogeneous list/pointer storage has one element type.
    fn index_storage_ty(&self, object: &Expr, index: &Expr) -> Option<Ty> {
        match self.infer(object).ok()? {
            Ty::Tuple(elements) => {
                let index = usize::try_from(self.eval_ct(index).ok()?).ok()?;
                elements.get(index).cloned()
            }
            Ty::List(element) | Ty::Pointer { element, .. } => Some(*element),
            Ty::Dict(_, value) => Some(*value),
            Ty::Simd { dtype, .. } => Some(simd_ty(dtype, 1)),
            _ => None,
        }
    }

    fn check_place_impl(&self, place: &Expr) -> Result<Ty, TypeError> {
        match &place.kind {
            ExprKind::Identifier(name) => {
                if name == "self" && !self.self_mutable {
                    return Err(TypeError::ImmutableSelf);
                }
                if !self.is_binding_mutable(name) {
                    return Err(TypeError::ImmutableBinding(name.clone()));
                }
                self.lookup(name)
                    .map(|ty| match ty {
                        Ty::Ref(reference) => (*reference.referent).clone(),
                        other => other.clone(),
                    })
                    .ok_or_else(|| TypeError::UndefinedVariable(name.clone()))
            }
            ExprKind::Member { object, field } => {
                // The object must itself be a writable place (a struct value).
                self.check_place(object)?;
                // Reuse the field-typing logic (validates the field exists).
                self.infer_member(object, field)
            }
            ExprKind::Index { object, index } => {
                let obj_ty = self.check_place(object)?;
                // A user struct with `__setitem__(mut self, i, v)` is index-assignable:
                // `c[i] = e` → `c.__setitem__(i, e)`. The index must coerce to the
                // first parameter; the *target* type (what `e` must be) is the second.
                if let Ty::Struct(sname, targs) = &obj_ty {
                    let info = self.structs.get(sname).ok_or_else(|| {
                        TypeError::InvariantViolation(format!(
                            "struct '{sname}' was not registered"
                        ))
                    })?;
                    let sig = info
                        .methods
                        .get("__setitem__")
                        .and_then(|sigs| sigs.iter().find(|sig| sig.params.len() == 2))
                        .ok_or_else(|| TypeError::NotIndexable(obj_ty.to_string()))?;
                    if !matches!(sig.self_convention, Some(crate::ast::ArgConvention::Mut)) {
                        return Err(TypeError::TypeMismatch {
                            expected: "a 'mut self' __setitem__".to_string(),
                            found: "read-only self".to_string(),
                            context: format!("index assignment on '{sname}'"),
                        });
                    }
                    let subst = struct_subst(&info.decls, targs);
                    let params: Vec<Ty> =
                        sig.params.iter().map(|t| substitute(t, &subst)).collect();
                    if params.len() != 2 {
                        return Err(TypeError::ArityMismatch {
                            name: "__setitem__".to_string(),
                            expected: 2,
                            got: params.len(),
                        });
                    }
                    let idx_ty = self.infer(index)?;
                    if !coerces(&idx_ty, &params[0]) {
                        return Err(TypeError::TypeMismatch {
                            expected: params[0].to_string(),
                            found: idx_ty.to_string(),
                            context: "argument to '__setitem__'".to_string(),
                        });
                    }
                    return Ok(params[1].clone());
                }
                let elem = match &obj_ty {
                    Ty::List(elem) => (**elem).clone(),
                    Ty::Dict(key, value) => {
                        let idx_ty = self.infer(index)?;
                        if !coerces(&idx_ty, key) {
                            return Err(TypeError::TypeMismatch {
                                expected: key.to_string(),
                                found: idx_ty.to_string(),
                                context: "dictionary key".to_string(),
                            });
                        }
                        return Ok((**value).clone());
                    }
                    // A pointer store `ptr[i] = e`: the target is the pointee type.
                    Ty::Pointer { element, .. } => (**element).clone(),
                    // A SIMD lane write `v[i] = e`: the target is the width-1 scalar.
                    Ty::Simd { dtype, .. } => simd_ty(*dtype, 1),
                    _ => return Err(TypeError::NotIndexable(obj_ty.to_string())),
                };
                let idx_ty = self.infer(index)?;
                if !self.is_index_type(&idx_ty) {
                    return Err(TypeError::TypeMismatch {
                        expected: "Indexer".to_string(),
                        found: idx_ty.to_string(),
                        context: "index".to_string(),
                    });
                }
                Ok(match elem {
                    Ty::Ref(reference) => *reference.referent,
                    other => other,
                })
            }
            ExprKind::Slice {
                object,
                lower,
                upper,
                step,
                explicit_step,
            } => {
                let object_type = self.check_place(object)?;
                self.check_slice_bounds(lower.as_deref(), upper.as_deref(), step.as_deref())?;
                let kind = if *explicit_step {
                    SliceKind::StridedSlice
                } else {
                    SliceKind::ContiguousSlice
                };
                let descriptor = Ty::Struct(kind.type_name().to_string(), Vec::new());
                let resolution = self.resolve_struct_setitem(&object_type, &[descriptor])?;
                if let Some(target) = resolution.lowered_name {
                    self.overload_targets
                        .borrow_mut()
                        .insert(place.source_span(), target);
                }
                self.variant_operations.borrow_mut().insert(
                    place.source_span(),
                    crate::checked::SemanticAdjustment::SliceDescriptors {
                        descriptors: vec![Some(kind)],
                        set_value_keyword: resolution.value_keyword,
                    },
                );
                Ok(resolution.return_type)
            }
            ExprKind::MultiIndex { object, args } => {
                let object_type = self.check_place(object)?;
                let mut argument_types = Vec::with_capacity(args.len());
                let mut descriptors = Vec::with_capacity(args.len());
                for argument in args {
                    match argument {
                        SubscriptArg::Index(value) => {
                            argument_types.push(self.infer(value)?);
                            descriptors.push(None);
                        }
                        SubscriptArg::Slice {
                            lower,
                            upper,
                            step,
                            explicit_step,
                        } => {
                            self.check_slice_bounds(
                                lower.as_deref(),
                                upper.as_deref(),
                                step.as_deref(),
                            )?;
                            let kind = if *explicit_step {
                                SliceKind::StridedSlice
                            } else {
                                SliceKind::ContiguousSlice
                            };
                            argument_types
                                .push(Ty::Struct(kind.type_name().to_string(), Vec::new()));
                            descriptors.push(Some(kind));
                        }
                    }
                }
                let resolution = self.resolve_struct_setitem(&object_type, &argument_types)?;
                if let Some(target) = resolution.lowered_name {
                    self.overload_targets
                        .borrow_mut()
                        .insert(place.source_span(), target);
                }
                self.variant_operations.borrow_mut().insert(
                    place.source_span(),
                    crate::checked::SemanticAdjustment::SliceDescriptors {
                        descriptors,
                        set_value_keyword: resolution.value_keyword,
                    },
                );
                Ok(resolution.return_type)
            }
            ExprKind::TypeApply { name, args } => {
                self.check_capture_access(name, true)?;
                if !self.is_binding_mutable(name) {
                    return Err(TypeError::ImmutableBinding(name.clone()));
                }
                let Ty::Variant(alternatives) = self
                    .lookup(name)
                    .cloned()
                    .ok_or_else(|| TypeError::UndefinedVariable(name.clone()))?
                else {
                    return Err(TypeError::InvalidAssignTarget(name.clone()));
                };
                let (index, alternative) = self.variant_alternative(&alternatives, args)?;
                self.variant_operations.borrow_mut().insert(
                    place.source_span(),
                    crate::checked::SemanticAdjustment::VariantProject {
                        alternatives,
                        index,
                    },
                );
                if let Some(owner) = self.lookup_owner(name) {
                    self.expression_bindings
                        .borrow_mut()
                        .insert(place.source_span(), owner);
                }
                Ok(alternative)
            }
            other => Err(TypeError::InvalidAssignTarget(format!("{:?}", other))),
        }
    }

    fn check_slice_bounds(
        &self,
        lower: Option<&Expr>,
        upper: Option<&Expr>,
        step: Option<&Expr>,
    ) -> Result<(), TypeError> {
        for bound in [lower, upper, step].into_iter().flatten() {
            let found = self.infer(bound)?;
            if !coerces(&found, &Ty::Int) {
                return Err(TypeError::TypeMismatch {
                    expected: "Int".to_string(),
                    found: found.to_string(),
                    context: "slice bound".to_string(),
                });
            }
        }
        Ok(())
    }

    fn infer_slice_subscript(
        &self,
        span: SourceSpan,
        object: &Expr,
        lower: Option<&Expr>,
        upper: Option<&Expr>,
        step: Option<&Expr>,
        explicit_step: bool,
    ) -> Result<Ty, TypeError> {
        self.check_slice_bounds(lower, upper, step)?;
        let kind = if explicit_step {
            SliceKind::StridedSlice
        } else {
            SliceKind::ContiguousSlice
        };
        let object_type = self.infer(object)?;
        let result = match &object_type {
            Ty::List(_) => object_type.clone(),
            Ty::String => Ty::String,
            Ty::Struct(..) => {
                let actual = Ty::Struct(kind.type_name().to_string(), Vec::new());
                let resolution = self.resolve_struct_subscript(&object_type, &[actual])?;
                if let Some(target) = resolution.lowered_name {
                    self.overload_targets
                        .borrow_mut()
                        .insert(span.clone(), target);
                }
                resolution.return_type
            }
            _ => return Err(TypeError::NotIndexable(object_type.to_string())),
        };
        self.variant_operations.borrow_mut().insert(
            span,
            crate::checked::SemanticAdjustment::SliceDescriptors {
                descriptors: vec![Some(kind)],
                set_value_keyword: false,
            },
        );
        Ok(result)
    }

    fn infer_multi_subscript(
        &self,
        span: SourceSpan,
        object: &Expr,
        arguments: &[SubscriptArg],
    ) -> Result<Ty, TypeError> {
        let mut actual_types = Vec::with_capacity(arguments.len());
        let mut descriptors = Vec::with_capacity(arguments.len());
        for argument in arguments {
            match argument {
                SubscriptArg::Index(value) => {
                    actual_types.push(self.infer(value)?);
                    descriptors.push(None);
                }
                SubscriptArg::Slice {
                    lower,
                    upper,
                    step,
                    explicit_step,
                } => {
                    self.check_slice_bounds(lower.as_deref(), upper.as_deref(), step.as_deref())?;
                    let kind = if *explicit_step {
                        SliceKind::StridedSlice
                    } else {
                        SliceKind::ContiguousSlice
                    };
                    actual_types.push(Ty::Struct(kind.type_name().to_string(), Vec::new()));
                    descriptors.push(Some(kind));
                }
            }
        }
        let object_type = self.infer(object)?;
        if !matches!(object_type, Ty::Struct(..)) {
            return Err(TypeError::NotIndexable(object_type.to_string()));
        }
        let resolution = self.resolve_struct_subscript(&object_type, &actual_types)?;
        if let Some(target) = resolution.lowered_name {
            self.overload_targets
                .borrow_mut()
                .insert(span.clone(), target);
        }
        self.variant_operations.borrow_mut().insert(
            span,
            crate::checked::SemanticAdjustment::SliceDescriptors {
                descriptors,
                set_value_keyword: false,
            },
        );
        Ok(resolution.return_type)
    }

    fn resolve_struct_subscript(
        &self,
        receiver: &Ty,
        arguments: &[Ty],
    ) -> Result<SubscriptResolution, TypeError> {
        let Ty::Struct(name, type_arguments) = receiver else {
            return Err(TypeError::NotIndexable(receiver.to_string()));
        };
        let info = self
            .structs
            .get(name)
            .ok_or_else(|| TypeError::NotIndexable(receiver.to_string()))?;
        let signatures = info
            .methods
            .get("__getitem__")
            .ok_or_else(|| TypeError::NotIndexable(receiver.to_string()))?;
        let substitution = struct_subst(&info.decls, type_arguments);
        let mut matches = Vec::new();
        for signature in signatures.iter().filter(|signature| signature.has_self) {
            if !signature.decls.is_empty() {
                continue;
            }
            let parameters: Vec<_> = signature
                .params
                .iter()
                .map(|parameter| substitute(parameter, &substitution))
                .collect();
            let variadic = signature
                .variadic
                .as_deref()
                .map(|parameter| substitute(parameter, &substitution));
            let matched = match match_call_slots(
                &signature.names,
                &signature.required,
                signature.positional_only,
                signature.keyword_only,
                arguments.len(),
                &[],
                CallVariadics {
                    positional: variadic.is_some(),
                    keyword: false,
                },
            ) {
                Ok(matched) => matched,
                Err(_) => continue,
            };
            let mut score = 0;
            let mut compatible = true;
            for (parameter, slot) in parameters.iter().zip(&matched.slots) {
                let ArgSlot::Positional(position) = slot else {
                    compatible = false;
                    break;
                };
                let actual = &arguments[*position];
                if !coerces(actual, parameter) {
                    compatible = false;
                    break;
                }
                score += conversion_count(actual, parameter);
            }
            if compatible && let Some(element) = &variadic {
                for position in matched.positional_overflow {
                    let actual = &arguments[position];
                    if !coerces(actual, element) {
                        compatible = false;
                        break;
                    }
                    score += conversion_count(actual, element);
                }
            }
            if compatible {
                matches.push((score, signature, substitute(&signature.ret, &substitution)));
            }
        }
        matches.sort_by_key(|(score, _, _)| *score);
        let Some((best, signature, return_type)) = matches.first() else {
            return Err(TypeError::NotIndexable(receiver.to_string()));
        };
        if matches.get(1).is_some_and(|(score, _, _)| score == best) {
            return Err(TypeError::BadCall {
                func: format!("{name}.__getitem__"),
                reason: "ambiguous subscript overload".to_string(),
            });
        }
        Ok(SubscriptResolution {
            return_type: return_type.clone(),
            lowered_name: (signatures.len() > 1)
                .then(|| method_lowered_name(name, "__getitem__", signature)),
            value_keyword: false,
        })
    }

    /// Resolve `receiver[indices...] = value`. The assignment value is the final
    /// regular parameter for a fixed-arity `__setitem__`. A variadic setitem uses
    /// Mojo's `*indices, *, value: T` shape, so lowering must pass the value through
    /// the keyword-only slot while the source indices fill the variadic pack.
    fn resolve_struct_setitem(
        &self,
        receiver: &Ty,
        arguments: &[Ty],
    ) -> Result<SubscriptResolution, TypeError> {
        let Ty::Struct(name, type_arguments) = receiver else {
            return Err(TypeError::NotIndexable(receiver.to_string()));
        };
        let info = self
            .structs
            .get(name)
            .ok_or_else(|| TypeError::NotIndexable(receiver.to_string()))?;
        let signatures = info
            .methods
            .get("__setitem__")
            .ok_or_else(|| TypeError::NotIndexable(receiver.to_string()))?;
        let substitution = struct_subst(&info.decls, type_arguments);
        let mut matches = Vec::new();
        let mut saw_read_only = false;

        for signature in signatures
            .iter()
            .filter(|signature| signature.has_self && signature.decls.is_empty())
        {
            if !matches!(
                signature.self_convention,
                Some(crate::ast::ArgConvention::Mut)
            ) {
                saw_read_only = true;
                continue;
            }
            let parameters: Vec<_> = signature
                .params
                .iter()
                .map(|parameter| substitute(parameter, &substitution))
                .collect();
            if parameters.is_empty() {
                continue;
            }
            let variadic = signature
                .variadic
                .as_deref()
                .map(|parameter| substitute(parameter, &substitution));

            let (value_index, value_keyword, fixed_index_count) = if variadic.is_some() {
                let Some(value_index) = signature.names.iter().position(|name| name == "value")
                else {
                    continue;
                };
                let fixed_index_count = signature.variadic_index.unwrap_or(0);
                // The currently published variadic operator shape has only a
                // keyword-only `value` parameter after `*indices`.
                if value_index < fixed_index_count || parameters.len() != fixed_index_count + 1 {
                    continue;
                }
                (value_index, true, fixed_index_count)
            } else {
                (parameters.len() - 1, false, parameters.len() - 1)
            };
            if arguments.len() < fixed_index_count
                || (variadic.is_none() && arguments.len() != fixed_index_count)
            {
                continue;
            }

            let mut score = 0;
            let mut compatible = true;
            for (actual, expected) in arguments
                .iter()
                .take(fixed_index_count)
                .zip(parameters.iter().take(fixed_index_count))
            {
                if !coerces(actual, expected) {
                    compatible = false;
                    break;
                }
                score += conversion_count(actual, expected);
            }
            if compatible && let Some(element) = &variadic {
                for actual in arguments.iter().skip(fixed_index_count) {
                    if !coerces(actual, element) {
                        compatible = false;
                        break;
                    }
                    score += conversion_count(actual, element);
                }
            }
            if compatible {
                matches.push((
                    overload_rank(score, variadic.is_some(), parameters.len(), false),
                    signature,
                    parameters[value_index].clone(),
                    value_keyword,
                ));
            }
        }

        matches.sort_by_key(|(score, _, _, _)| *score);
        let Some((best, signature, value_type, value_keyword)) = matches.first() else {
            if saw_read_only {
                return Err(TypeError::TypeMismatch {
                    expected: "a 'mut self' __setitem__".to_string(),
                    found: "read-only self".to_string(),
                    context: format!("index assignment on '{name}'"),
                });
            }
            return Err(TypeError::NotIndexable(receiver.to_string()));
        };
        if matches.get(1).is_some_and(|(score, _, _, _)| score == best) {
            return Err(TypeError::BadCall {
                func: format!("{name}.__setitem__"),
                reason: "ambiguous subscript-assignment overload".to_string(),
            });
        }
        Ok(SubscriptResolution {
            return_type: value_type.clone(),
            lowered_name: (signatures.len() > 1)
                .then(|| method_lowered_name(name, "__setitem__", signature)),
            value_keyword: *value_keyword,
        })
    }

    /// Type a subscript over tuples, SIMD, lists, pointers, or a user-defined
    /// `__getitem__` implementation.
    fn infer_index(&self, object: &Expr, index: &Expr) -> Result<Ty, TypeError> {
        let obj_ty = self.infer(object)?;
        // A tuple is heterogeneous, so its index must be a **compile-time** `Int`
        // constant — the result type is that element's type.
        if let Ty::Tuple(elems) = &obj_ty {
            let i = self.eval_ct(index).map_err(|_| TypeError::TypeMismatch {
                expected: "a compile-time Int index".to_string(),
                found: "a runtime value".to_string(),
                context: "tuple index".to_string(),
            })?;
            if i < 0 || i as usize >= elems.len() {
                return Err(TypeError::TypeMismatch {
                    expected: format!("a tuple index in 0..{}", elems.len()),
                    found: i.to_string(),
                    context: "tuple index".to_string(),
                });
            }
            return Ok(match &elems[i as usize] {
                Ty::Ref(reference) => (*reference.referent).clone(),
                element => element.clone(),
            });
        }
        // A user struct with `__getitem__` is subscriptable: `c[i]` →
        // `c.__getitem__(i)`, typed by the method (the index need not be `Int`).
        if matches!(obj_ty, Ty::Struct(..)) {
            let idx_ty = self.infer(index)?;
            if let Some(r) = self.struct_dunder(&obj_ty, "__getitem__", &[&idx_ty]) {
                return r;
            }
            return Err(TypeError::NotIndexable(obj_ty.to_string()));
        }
        // An opaque type parameter is indexable when one of its trait bounds
        // promises `__getitem__`. The requirement supplies both the index and
        // result type; runtime dispatch remains the ordinary concrete dunder call
        // after type erasure.
        if let Ty::Param { bounds, .. } = &obj_ty {
            let idx_ty = self.infer(index)?;
            if let Some(signature) = self.lookup_trait_method(bounds, "__getitem__", 1) {
                let expected = substitute_self(&signature.params[0], &obj_ty);
                if !coerces(&idx_ty, &expected) {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: idx_ty.to_string(),
                        context: "index".to_string(),
                    });
                }
                return Ok(substitute_self(&signature.ret, &obj_ty));
            }
            return Err(TypeError::NotIndexable(obj_ty.to_string()));
        }
        // The result of indexing: a SIMD lane, a List element, or a pointer pointee.
        let result = match &obj_ty {
            Ty::Simd { dtype, .. } => simd_ty(*dtype, 1),
            Ty::List(elem) => (**elem).clone(),
            Ty::Dict(key, value) => {
                let idx_ty = self.infer(index)?;
                if !coerces(&idx_ty, key) {
                    return Err(TypeError::TypeMismatch {
                        expected: key.to_string(),
                        found: idx_ty.to_string(),
                        context: "dictionary key".to_string(),
                    });
                }
                return Ok((**value).clone());
            }
            Ty::Pointer { element, .. } => (**element).clone(),
            _ => return Err(TypeError::NotIndexable(obj_ty.to_string())),
        };
        let idx_ty = self.infer(index)?;
        if !self.is_index_type(&idx_ty) {
            return Err(TypeError::TypeMismatch {
                expected: "Indexer".to_string(),
                found: idx_ty.to_string(),
                context: "index".to_string(),
            });
        }
        Ok(match result {
            Ty::Ref(reference) => *reference.referent,
            other => other,
        })
    }

    /// Whether a value can be normalized to the VM's index representation.
    /// Numeric literals/Int use the identity path; an opaque `Indexer` or a
    /// concrete conformer supplies `__mlir_index__() -> Int`.
    fn is_index_type(&self, ty: &Ty) -> bool {
        coerces(ty, &Ty::Int)
            || matches!(ty, Ty::Param { bounds, .. } if bounds.iter().any(|bound| bound == "Indexer"))
            || matches!(ty, Ty::Struct(..))
                && self.struct_dunder(ty, "__mlir_index__", &[]) == Some(Ok(Ty::Int))
    }

    /// Type a field access `object.field`. On a generic struct value the field
    /// type has the struct's type arguments substituted in (`Pair[Int].left :
    /// Int`).
    fn infer_member(&self, object: &Expr, field: &str) -> Result<Ty, TypeError> {
        // `Self.n` reads the enclosing struct's value parameter (an `Int`).
        if let ExprKind::Identifier(s) = &object.kind
            && s == "Self"
        {
            if let Some(self_ty) = &self.self_ty {
                self.expression_types
                    .borrow_mut()
                    .insert(object.source_span(), self_ty.clone());
            }
            return match self.self_decls.iter().find(|d| d.name() == field) {
                Some(ParamDecl::Value { .. }) => Ok(Ty::Int),
                _ => Err(TypeError::UnknownSelfParam(field.to_string())),
            };
        }
        // `T.size` where `T` is a generic type parameter and a bound trait
        // requires `comptime size: Int`: expression-level access to an associated
        // compile-time value. Type-valued associated members remain type-position
        // only (`T.Element` in annotations).
        if let ExprKind::Identifier(name) = &object.kind
            && let Some(bounds) = self.lookup_tparam(name)
            && let Some(ty) = self.lookup_trait_assoc_value_ty(&bounds, field)
        {
            self.expression_types.borrow_mut().insert(
                object.source_span(),
                Ty::Param {
                    name: name.clone(),
                    bounds,
                },
            );
            return Ok(ty);
        }
        let obj_ty = self.infer(object)?;
        if matches!(&obj_ty, Ty::Struct(name, args) if matches!(name.as_str(), "Slice" | "ContiguousSlice" | "StridedSlice") && args.is_empty())
            && matches!(field, "start" | "end" | "step")
        {
            return Ok(Ty::Struct("Optional".to_string(), vec![TyArg::Ty(Ty::Int)]));
        }
        if let Ty::Struct(sname, targs) = &obj_ty {
            let info = self.structs.get(sname).ok_or_else(|| {
                TypeError::InvariantViolation(format!("struct '{sname}' was not registered"))
            })?;
            if let Some((_, fty)) = info.fields.iter().find(|(n, _)| n == field) {
                let subst = struct_subst(&info.decls, targs);
                return Ok(match substitute(fty, &subst) {
                    Ty::Ref(reference) => *reference.referent,
                    value => value,
                });
            }
        }
        Err(TypeError::NoSuchField {
            object_type: obj_ty.to_string(),
            field: field.to_string(),
        })
    }

    /// Type a method call `object.method(args)`. On a generic struct value the
    /// method's parameter and return types are substituted at the receiver's
    /// type arguments; on a bounded type parameter (`x: T` with `T: SomeTrait`)
    /// the method is resolved from the bound trait's requirement, with `Self`
    /// substituted to `T`.
    fn infer_method_call(
        &self,
        span: SourceSpan,
        object: &Expr,
        method: &str,
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Ty, TypeError> {
        // A **static** method on a parameterized built-in type — the receiver is a
        // type, not a value (`UnsafePointer[T].alloc(n)`). Handled before inferring
        // the object (which would reject a bare `TypeApply`).
        if let ExprKind::TypeApply { name, args: targs } = &object.kind {
            reject_kwargs(kwargs)?;
            return self.infer_static_method(name, targs, method, args);
        }
        if let ExprKind::Identifier(sname) = &object.kind
            && let Some(info) = self.structs.get(sname)
            && let Some(signatures) = info.methods.get(method)
        {
            let mut matches = Vec::new();
            for sig in signatures.iter().filter(|sig| !sig.has_self) {
                let (params, variadic, kw_variadic, method_subst, method_arguments) = match self
                    .instantiate_method_generics(
                        &format!("{sname}.{method}"),
                        sig,
                        &sig.params,
                        sig.variadic.as_deref(),
                        sig.kw_variadic.as_deref(),
                        args,
                        kwargs,
                    ) {
                    Ok(instantiated) => instantiated,
                    Err(_) => continue,
                };
                if !self.method_constraints_apply(sig, &method_arguments) {
                    continue;
                }
                if let Ok(scored) = self.score_method_call(
                    sig,
                    &params,
                    variadic.as_ref(),
                    kw_variadic.as_ref(),
                    args,
                    kwargs,
                ) {
                    matches.push(MethodCallResolution {
                        conversion_score: scored.rank,
                        slots: scored.slots,
                        keyword_overflow: scored.keyword_overflow,
                        keyword_element: kw_variadic.clone(),
                        conventions: sig.conventions.clone(),
                        return_type: substitute(&sig.ret, &method_subst),
                        raises: sig.raises,
                        error: sig
                            .error
                            .as_ref()
                            .map(|error| Box::new(substitute(error, &method_subst))),
                        mutates_receiver: false,
                        consumes_receiver: false,
                        lowered_name: (signatures.len() > 1)
                            .then(|| method_lowered_name(sname, method, sig)),
                        ref_params: sig.ref_params.clone(),
                        ref_return: sig.ref_return.clone(),
                        param_types: params,
                    });
                }
            }
            if !matches.is_empty() {
                let selected =
                    select_method_overload(method, matches).map_err(|kind| TypeError::BadCall {
                        func: format!("{sname}.{method}"),
                        reason: match kind {
                            OverloadSelect::NoMatch => "no overload matches the supplied arguments",
                            OverloadSelect::Ambiguous => "ambiguous overloaded call",
                        }
                        .to_string(),
                    })?;
                self.record_selected_method_conversions(method, &selected, args, kwargs)?;
                if let Some(target) = selected.lowered_name {
                    self.overload_targets
                        .borrow_mut()
                        .insert(span.clone(), target);
                }
                if selected.raises {
                    let error = selected.error.as_deref().cloned().unwrap_or(Ty::Error);
                    self.record_call_effect(span.clone(), error.clone());
                    self.require_error(
                        format!("call to raising method '{sname}.{method}'"),
                        error,
                    )?;
                }
                return Ok(selected.return_type);
            }
        }
        let obj_ty = self.infer(object)?;
        if matches!(&obj_ty, Ty::Struct(name, args) if matches!(name.as_str(), "Slice" | "ContiguousSlice" | "StridedSlice") && args.is_empty())
        {
            reject_kwargs(kwargs)?;
            if method != "indices" {
                return Err(TypeError::NoSuchMethod {
                    object_type: obj_ty.to_string(),
                    method: method.to_string(),
                });
            }
            let types = self.builtin_args("Slice.indices", 1, args)?;
            if !coerces(&types[0], &Ty::Int) {
                return Err(TypeError::TypeMismatch {
                    expected: "Int".to_string(),
                    found: types[0].to_string(),
                    context: "Slice.indices length".to_string(),
                });
            }
            return Ok(Ty::Tuple(vec![Ty::Int, Ty::Int, Ty::Int]));
        }
        if matches!(&obj_ty, Ty::Struct(name, args) if name == "Optional" && matches!(args.as_slice(), [TyArg::Ty(Ty::Int)]))
        {
            reject_kwargs(kwargs)?;
            return match method {
                "is_some" if args.is_empty() => Ok(Ty::Bool),
                "or_else" => {
                    let types = self.builtin_args("Optional.or_else", 1, args)?;
                    if coerces(&types[0], &Ty::Int) {
                        Ok(Ty::Int)
                    } else {
                        Err(TypeError::TypeMismatch {
                            expected: "Int".to_string(),
                            found: types[0].to_string(),
                            context: "Optional.or_else default".to_string(),
                        })
                    }
                }
                _ => Err(TypeError::NoSuchMethod {
                    object_type: obj_ty.to_string(),
                    method: method.to_string(),
                }),
            };
        }
        if self.conforms_to(&obj_ty, "Writer") && method == "write" {
            reject_kwargs(kwargs)?;
            self.check_place(object)?;
            self.infer_print(args)?;
            return Ok(Ty::None);
        }
        if matches!(&obj_ty, Ty::Param { bounds, .. } if bounds.iter().any(|bound| bound == "Hasher"))
            && method == "update"
        {
            reject_kwargs(kwargs)?;
            self.check_place(object)?;
            let tys = self.builtin_args("Hasher.update", 1, args)?;
            if !self.conforms_to(&tys[0], "Hashable") {
                return Err(TypeError::TraitNotSatisfied {
                    param: "T".to_string(),
                    ty: tys[0].to_string(),
                    trait_name: "Hashable".to_string(),
                    reason: self.trait_failure_reason(&tys[0], "Hashable"),
                });
            }
            return Ok(Ty::None);
        }
        if obj_ty == Ty::String && method == "format" {
            reject_kwargs(kwargs)?;
            self.infer_print(args)?;
            return Ok(Ty::String);
        }
        // Built-in `List` methods (mutating; require a plain variable receiver).
        if let Ty::List(elem) = &obj_ty {
            reject_kwargs(kwargs)?;
            return self.infer_list_method(object, method, elem, args);
        }
        if let Ty::Set(elem) = &obj_ty {
            reject_kwargs(kwargs)?;
            return match method {
                "add" => {
                    self.check_place(object)?;
                    let values = self.builtin_args("Set.add", 1, args)?;
                    if !coerces(&values[0], elem) {
                        return Err(TypeError::TypeMismatch {
                            expected: elem.to_string(),
                            found: values[0].to_string(),
                            context: "Set.add value".to_string(),
                        });
                    }
                    self.check_consuming(&args[0], &values[0], "Set.add value")?;
                    Ok(Ty::None)
                }
                _ => Err(TypeError::NoSuchMethod {
                    object_type: obj_ty.to_string(),
                    method: method.to_string(),
                }),
            };
        }
        if let Ty::Tuple(elements) = &obj_ty {
            reject_kwargs(kwargs)?;
            return self.infer_tuple_method(method, elements, args);
        }
        // Built-in `UnsafePointer` methods (`free`).
        if let Ty::Pointer { element: elem, .. } = &obj_ty {
            reject_kwargs(kwargs)?;
            return self.infer_pointer_method(method, elem, args);
        }
        // Resolve the method to a concrete signature (params + return + whether
        // it mutates `self`) for this receiver, substituting the receiver's type
        // arguments (struct) or `Self` (a bounded type parameter's trait method).
        let resolved: Result<Option<MethodCallResolution>, OverloadSelect> = match &obj_ty {
            Ty::Struct(sname, targs) => {
                let info = self.structs.get(sname).ok_or_else(|| {
                    TypeError::InvariantViolation(format!("struct '{sname}' was not registered"))
                })?;
                match info.methods.get(method) {
                    Some(sigs) => {
                        let overloaded = sigs.len() > 1;
                        let subst = struct_subst(&info.decls, targs);
                        let mut matches = Vec::new();
                        for sig in sigs {
                            let receiver_params: Vec<Ty> =
                                sig.params.iter().map(|t| substitute(t, &subst)).collect();
                            let receiver_variadic =
                                sig.variadic.as_ref().map(|ty| substitute(ty, &subst));
                            let receiver_kw_variadic =
                                sig.kw_variadic.as_ref().map(|ty| substitute(ty, &subst));
                            let Ok((
                                params,
                                variadic,
                                kw_variadic,
                                method_subst,
                                mut method_arguments,
                            )) = self.instantiate_method_generics(
                                &format!("{sname}.{method}"),
                                sig,
                                &receiver_params,
                                receiver_variadic.as_ref(),
                                receiver_kw_variadic.as_ref(),
                                args,
                                kwargs,
                            )
                            else {
                                continue;
                            };
                            for (decl, argument) in info.decls.iter().zip(targs) {
                                method_arguments.insert(
                                    decl.name().trim_start_matches('*').to_string(),
                                    argument.clone(),
                                );
                            }
                            if !self.method_constraints_apply(sig, &method_arguments) {
                                continue;
                            }
                            if let Ok(scored) = self.score_method_call(
                                sig,
                                &params,
                                variadic.as_ref(),
                                kw_variadic.as_ref(),
                                args,
                                kwargs,
                            ) {
                                matches.push(MethodCallResolution {
                                    conversion_score: scored.rank,
                                    slots: scored.slots,
                                    keyword_overflow: scored.keyword_overflow,
                                    keyword_element: kw_variadic.clone(),
                                    conventions: sig.conventions.clone(),
                                    return_type: substitute(
                                        &substitute(&sig.ret, &subst),
                                        &method_subst,
                                    ),
                                    raises: sig.raises,
                                    error: sig.error.as_ref().map(|error| {
                                        Box::new(substitute(
                                            &substitute(error, &subst),
                                            &method_subst,
                                        ))
                                    }),
                                    mutates_receiver: matches!(
                                        sig.self_convention,
                                        Some(
                                            crate::ast::ArgConvention::Mut
                                                | crate::ast::ArgConvention::Ref
                                        )
                                    ),
                                    consumes_receiver: matches!(
                                        sig.self_convention,
                                        Some(
                                            crate::ast::ArgConvention::Var
                                                | crate::ast::ArgConvention::Deinit
                                        )
                                    ),
                                    lowered_name: overloaded
                                        .then(|| method_lowered_name(sname, method, sig)),
                                    ref_params: sig.ref_params.clone(),
                                    ref_return: sig.ref_return.clone(),
                                    param_types: params,
                                });
                            }
                        }
                        select_method_overload(method, matches).map(Some)
                    }
                    None => Ok(None),
                }
            }
            Ty::Param { bounds, .. } => {
                let signatures = self.lookup_trait_methods(bounds, method, args.len());
                if signatures.is_empty() {
                    return Err(TypeError::NoSuchMethod {
                        object_type: obj_ty.to_string(),
                        method: method.to_string(),
                    });
                }
                let mut matches = Vec::new();
                for sig in signatures {
                    let receiver_params: Vec<_> = sig
                        .params
                        .iter()
                        .map(|ty| substitute_self(ty, &obj_ty))
                        .collect();
                    let receiver_variadic = sig
                        .variadic
                        .as_deref()
                        .map(|ty| substitute_self(ty, &obj_ty));
                    let receiver_kw_variadic = sig
                        .kw_variadic
                        .as_deref()
                        .map(|ty| substitute_self(ty, &obj_ty));
                    let Ok((params, variadic, kw_variadic, method_subst, method_arguments)) = self
                        .instantiate_method_generics(
                            &format!("{obj_ty}.{method}"),
                            &sig,
                            &receiver_params,
                            receiver_variadic.as_ref(),
                            receiver_kw_variadic.as_ref(),
                            args,
                            kwargs,
                        )
                    else {
                        continue;
                    };
                    if !self.method_constraints_apply(&sig, &method_arguments) {
                        continue;
                    }
                    let Ok(scored) = self.score_method_call(
                        &sig,
                        &params,
                        variadic.as_ref(),
                        kw_variadic.as_ref(),
                        args,
                        kwargs,
                    ) else {
                        continue;
                    };
                    matches.push(MethodCallResolution {
                        conversion_score: scored.rank,
                        slots: scored.slots,
                        keyword_overflow: scored.keyword_overflow,
                        keyword_element: kw_variadic.clone(),
                        conventions: sig.conventions.clone(),
                        return_type: self.resolve_assoc_ty(&substitute(
                            &substitute_self(&sig.ret, &obj_ty),
                            &method_subst,
                        )),
                        raises: sig.raises,
                        error: sig.error.as_ref().map(|error| {
                            Box::new(self.resolve_assoc_ty(&substitute(
                                &substitute_self(error, &obj_ty),
                                &method_subst,
                            )))
                        }),
                        mutates_receiver: matches!(
                            sig.self_convention,
                            Some(crate::ast::ArgConvention::Mut | crate::ast::ArgConvention::Ref)
                        ),
                        consumes_receiver: matches!(
                            sig.self_convention,
                            Some(
                                crate::ast::ArgConvention::Var | crate::ast::ArgConvention::Deinit
                            )
                        ),
                        lowered_name: Some(method_lowered_name("__trait_dispatch", method, &sig)),
                        ref_params: sig.ref_params.clone(),
                        ref_return: sig.ref_return.clone(),
                        param_types: params,
                    });
                }
                select_method_overload(method, matches).map(Some)
            }
            // `x.__hash__()` on a concrete built-in hashable type (`Int`, `String`,
            // …) is an intrinsic returning `UInt` — lets a key struct combine
            // `self.field.__hash__()` values (roadmap milestone 6).
            _ if method == "__hash__"
                && args.is_empty()
                && (builtin_hashable_ty(&obj_ty)
                    || matches!(&obj_ty, Ty::Variant(alternatives) if alternatives.iter().all(|alternative| self.is_hashable(alternative)))) =>
            {
                Ok(Some(MethodCallResolution {
                    conversion_score: 0,
                    slots: vec![],
                    keyword_overflow: vec![],
                    keyword_element: None,
                    conventions: vec![],
                    return_type: Ty::UInt,
                    raises: false,
                    error: None,
                    mutates_receiver: false,
                    consumes_receiver: false,
                    lowered_name: None,
                    ref_params: vec![],
                    ref_return: None,
                    param_types: vec![],
                }))
            }
            _ => Ok(None),
        };
        let resolved = match resolved {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                return Err(TypeError::NoSuchMethod {
                    object_type: obj_ty.to_string(),
                    method: method.to_string(),
                });
            }
            Err(OverloadSelect::NoMatch) => {
                return Err(TypeError::BadCall {
                    func: method.to_string(),
                    reason: "no overload matches the supplied arguments".to_string(),
                });
            }
            Err(OverloadSelect::Ambiguous) => {
                return Err(TypeError::BadCall {
                    func: method.to_string(),
                    reason: "ambiguous overloaded method call".to_string(),
                });
            }
        };
        self.record_selected_method_conversions(method, &resolved, args, kwargs)?;
        if resolved.raises {
            let error = resolved.error.as_deref().cloned().unwrap_or(Ty::Error);
            self.record_call_effect(span.clone(), error.clone());
            self.require_error(format!("call to raising method '{method}'"), error)?;
        }
        if let Some(target) = resolved.lowered_name {
            self.overload_targets
                .borrow_mut()
                .insert(span.clone(), target);
        }
        // A `mut self` method mutates its receiver, so the receiver must be a
        // writable place (the mutation is written back to it): a variable, a
        // field/index chain, or `self` in a `mut self` method.
        if resolved.mutates_receiver {
            self.check_place(object)?;
        }
        if resolved.consumes_receiver && !matches!(object.kind, ExprKind::Transfer(_)) {
            return Err(TypeError::NonCopyable {
                ty: obj_ty.to_string(),
                context: format!(
                    "consuming receiver of method '{method}' must be transferred with '^'"
                ),
            });
        }
        if resolved.consumes_receiver
            && let Ty::Struct(name, _) = &obj_ty
            && self
                .structs
                .get(name)
                .is_some_and(|info| info.explicit_destructors.contains_key(method))
        {
            self.explicit_destroy_calls.borrow_mut().insert(span);
        }
        for (index, slot) in resolved.slots.iter().enumerate() {
            let expression = match slot {
                ArgSlot::Positional(position) => &args[*position],
                ArgSlot::Keyword(position) => &kwargs[*position].value,
                ArgSlot::Default => continue,
            };
            let ty = self.infer_with_expected(
                expression,
                resolved
                    .param_types
                    .get(index)
                    .expect("selected method slot has a parameter type"),
                true,
            )?;
            if matches!(
                resolved.conventions.get(index),
                Some(Some(ArgConvention::Var | ArgConvention::Deinit))
            ) {
                self.check_consuming(
                    expression,
                    &ty,
                    &format!("argument {} to method '{}'", index + 1, method),
                )?;
            }
        }
        let (effective_conventions, _) = self.solve_call_origins(
            &resolved.slots,
            &resolved.conventions,
            &resolved.ref_params,
            resolved.ref_return.as_ref(),
            args,
            kwargs,
        )?;
        let copied_reads = resolved
            .slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let expression = match slot {
                    ArgSlot::Positional(position) => &args[*position],
                    ArgSlot::Keyword(position) => &kwargs[*position].value,
                    ArgSlot::Default => return Ok(false),
                };
                let convention = effective_conventions.get(index).copied().flatten();
                Ok(
                    !matches!(convention, Some(ArgConvention::Mut | ArgConvention::Ref))
                        && self.is_copyable(
                            &self.infer_with_expected(
                                expression,
                                resolved
                                    .param_types
                                    .get(index)
                                    .expect("selected method slot has a parameter type"),
                                true,
                            )?,
                        ),
                )
            })
            .collect::<Result<Vec<_>, TypeError>>()?;
        check_call_aliasing(
            &resolved.slots,
            &effective_conventions,
            &copied_reads,
            args,
            kwargs,
        )?;
        if let Some(signature) = &resolved.ref_return {
            let actual: Vec<_> = resolved
                .slots
                .iter()
                .map(|slot| match slot {
                    ArgSlot::Positional(position) => self.origin_place(&args[*position]).ok(),
                    ArgSlot::Keyword(position) => self.origin_place(&kwargs[*position].value).ok(),
                    ArgSlot::Default => None,
                })
                .map(|place| place.map(crate::origin::Origin::Place))
                .collect();
            let self_place = self.origin_place(object)?;
            let self_owner = Some(self_place.root);
            let origin = substitute_sig_origin_with_self(&signature.origin, &actual, self_owner);
            let mutable = match signature.mutability {
                crate::origin::SigMutability::Immutable => crate::origin::Mutability::Immutable,
                crate::origin::SigMutability::Mutable => crate::origin::Mutability::Mutable,
                _ if self.owner_is_mutable(self_place.root) => crate::origin::Mutability::Mutable,
                _ => crate::origin::Mutability::Immutable,
            };
            return Ok(Ty::Ref(crate::origin::RefTy {
                referent: Box::new(resolved.return_type),
                origin,
                mutability: mutable,
            }));
        }
        Ok(resolved.return_type)
    }

    /// Apply the implicit conversions selected while scoring one concrete method
    /// overload. Keyword-overflow arguments are materialized into the callee's
    /// `StringDict`, so their conversions must be recorded just like conversions
    /// for ordinary parameter slots.
    fn record_selected_method_conversions(
        &self,
        method: &str,
        resolved: &MethodCallResolution,
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<(), TypeError> {
        for (index, slot) in resolved.slots.iter().enumerate() {
            let expression = match slot {
                ArgSlot::Positional(position) => &args[*position],
                ArgSlot::Keyword(position) => &kwargs[*position].value,
                ArgSlot::Default => continue,
            };
            if let Some(expected) = resolved.param_types.get(index) {
                let actual = self.infer_with_expected(expression, expected, true)?;
                if !self.record_implicit_conversion(expression, &actual, expected)? {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: actual.to_string(),
                        context: format!("argument {} to method '{method}'", index + 1),
                    });
                }
            }
        }
        if let Some(expected) = &resolved.keyword_element {
            for &position in &resolved.keyword_overflow {
                let expression = &kwargs[position].value;
                let actual = self.infer(expression)?;
                if !self.record_implicit_conversion(expression, &actual, expected)? {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: actual.to_string(),
                        context: format!(
                            "keyword '{}' collected by method '{method}'",
                            kwargs[position].name
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn score_method_call(
        &self,
        signature: &MethodSig,
        params: &[Ty],
        variadic: Option<&Ty>,
        kw_variadic: Option<&Ty>,
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<MethodCallScore, TypeError> {
        let forwarded_element = self.forwarded_kwargs_element("method", kwargs)?;
        if forwarded_element.is_some() && kw_variadic.is_none() {
            return Err(TypeError::BadCall {
                func: "method".to_string(),
                reason: "`**kwargs^` requires a callee with a `**kwargs` collector".to_string(),
            });
        }
        let keyword_names: Vec<_> = kwargs
            .iter()
            .filter(|argument| !argument.is_forwarded())
            .map(|arg| arg.name.as_str())
            .collect();
        let matched = match_call_slots(
            &signature.names,
            &signature.required,
            signature.positional_only,
            signature.keyword_only,
            args.len(),
            &keyword_names,
            CallVariadics {
                positional: variadic.is_some(),
                keyword: kw_variadic.is_some(),
            },
        )
        .map_err(|error| error.into_type_error("method"))?;
        let (slots, overflow) = (matched.slots, matched.positional_overflow);
        let mut score = 0;
        for (index, slot) in slots.iter().enumerate() {
            let expression = match slot {
                ArgSlot::Positional(position) => &args[*position],
                ArgSlot::Keyword(position) => &kwargs[*position].value,
                ArgSlot::Default => continue,
            };
            let actual = self.infer_with_expected(expression, &params[index], false)?;
            if !self.value_coerces(&actual, &params[index])
                && self
                    .implicit_conversion_target(&actual, &params[index])?
                    .is_none()
            {
                return Err(TypeError::TypeMismatch {
                    expected: params[index].to_string(),
                    found: actual.to_string(),
                    context: "method overload candidate".to_string(),
                });
            }
            score += conversion_count(&actual, &params[index]);
        }
        if let Some(element) = variadic {
            for position in overflow {
                let actual = self.infer(&args[position])?;
                if !coerces(&actual, element) {
                    return Err(TypeError::TypeMismatch {
                        expected: element.to_string(),
                        found: actual.to_string(),
                        context: "variadic method argument".to_string(),
                    });
                }
                score += conversion_count(&actual, element);
            }
        }
        let keyword_overflow = matched.keyword_overflow;
        if let Some(element) = kw_variadic {
            for &position in &keyword_overflow {
                let expression = &kwargs[position].value;
                let actual = self.infer(expression)?;
                if !self.value_coerces(&actual, element)
                    && self.implicit_conversion_target(&actual, element)?.is_none()
                {
                    return Err(TypeError::TypeMismatch {
                        expected: element.to_string(),
                        found: actual.to_string(),
                        context: "keyword variadic method argument".to_string(),
                    });
                }
                self.check_consuming(
                    expression,
                    &actual,
                    &format!("keyword '{}' collected by method", kwargs[position].name),
                )?;
                score += conversion_count(&actual, element);
            }
            if let Some(actual) = forwarded_element
                && actual != *element
            {
                return Err(TypeError::TypeMismatch {
                    expected: format!("StringDict[{element}]"),
                    found: format!("StringDict[{actual}]"),
                    context: "forwarded keyword arguments to method".to_string(),
                });
            }
        }
        Ok(MethodCallScore {
            rank: overload_rank(score, variadic.is_some() || kw_variadic.is_some(), 0, false),
            slots,
            keyword_overflow,
        })
    }

    fn instantiate_method_generics(
        &self,
        name: &str,
        signature: &MethodSig,
        params: &[Ty],
        variadic: Option<&Ty>,
        kw_variadic: Option<&Ty>,
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<MethodInstantiation, TypeError> {
        if signature.decls.is_empty() {
            return Ok((
                params.to_vec(),
                variadic.cloned(),
                kw_variadic.cloned(),
                HashMap::new(),
                HashMap::new(),
            ));
        }
        let forwarded_element = self.forwarded_kwargs_element(name, kwargs)?;
        if forwarded_element.is_some() && kw_variadic.is_none() {
            return Err(TypeError::BadCall {
                func: name.to_string(),
                reason: "`**kwargs^` requires a callee with a `**kwargs` collector".to_string(),
            });
        }
        let keyword_names: Vec<_> = kwargs
            .iter()
            .filter(|argument| !argument.is_forwarded())
            .map(|arg| arg.name.as_str())
            .collect();
        let matched = match_call_slots(
            &signature.names,
            &signature.required,
            signature.positional_only,
            signature.keyword_only,
            args.len(),
            &keyword_names,
            CallVariadics {
                positional: variadic.is_some(),
                keyword: kw_variadic.is_some(),
            },
        )
        .map_err(|error| error.into_type_error(name))?;
        let mut patterns = Vec::new();
        let mut actuals = Vec::new();
        for (index, slot) in matched.slots.iter().enumerate() {
            let expression = match slot {
                ArgSlot::Positional(position) => &args[*position],
                ArgSlot::Keyword(position) => &kwargs[*position].value,
                ArgSlot::Default => continue,
            };
            patterns.push(params[index].clone());
            actuals.push(self.infer(expression)?);
        }
        if let Some(element) = variadic {
            for position in matched.positional_overflow {
                patterns.push(element.clone());
                actuals.push(self.infer(&args[position])?);
            }
        }
        if let Some(element) = kw_variadic {
            for position in matched.keyword_overflow {
                patterns.push(element.clone());
                actuals.push(self.infer(&kwargs[position].value)?);
            }
            if let Some(actual) = forwarded_element {
                patterns.push(element.clone());
                actuals.push(actual);
            }
        }
        let (subst, tyargs) =
            self.resolve_use_params(name, &signature.decls, &[], &patterns, &actuals)?;
        let arguments = signature
            .decls
            .iter()
            .zip(tyargs)
            .map(|(decl, argument)| (decl.name().trim_start_matches('*').to_string(), argument))
            .collect();
        Ok((
            params.iter().map(|ty| substitute(ty, &subst)).collect(),
            variadic.map(|ty| substitute(ty, &subst)),
            kw_variadic.map(|ty| substitute(ty, &subst)),
            subst,
            arguments,
        ))
    }

    fn method_constraints_apply(
        &self,
        signature: &MethodSig,
        arguments: &HashMap<String, TyArg>,
    ) -> bool {
        let borrowed: HashMap<&str, &TyArg> = arguments
            .iter()
            .map(|(name, argument)| (name.as_str(), argument))
            .collect();
        signature
            .availability
            .iter()
            .all(|constraint| self.eval_generic_constraint(constraint, &borrowed))
    }

    /// Type a static method on a parameterized built-in type. Currently only
    /// `UnsafePointer[T].alloc(count: Int) -> UnsafePointer[T]`.
    fn infer_static_method(
        &self,
        tyname: &str,
        targs: &[crate::ast::ParamArg],
        method: &str,
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        if tyname != "UnsafePointer" {
            return Err(TypeError::NoSuchMethod {
                object_type: format!("{tyname}[…]"),
                method: method.to_string(),
            });
        }
        let ptr_ty = self.pointer_type(targs)?;
        match method {
            "alloc" | "alloc_aligned" => {
                let expected = if method == "alloc" { 1 } else { 2 };
                if args.len() != expected {
                    return Err(TypeError::ArityMismatch {
                        name: method.to_string(),
                        expected,
                        got: args.len(),
                    });
                }
                for argument in args {
                    let aty = self.infer(argument)?;
                    if !coerces(&aty, &Ty::Int) {
                        return Err(TypeError::TypeMismatch {
                            expected: "Int".to_string(),
                            found: aty.to_string(),
                            context: format!("argument to 'UnsafePointer.{method}'"),
                        });
                    }
                }
                Ok(ptr_ty)
            }
            "dangling" => {
                if !args.is_empty() {
                    return Err(TypeError::ArityMismatch {
                        name: method.to_string(),
                        expected: 0,
                        got: args.len(),
                    });
                }
                Ok(ptr_ty)
            }
            _ => Err(TypeError::NoSuchMethod {
                object_type: ptr_ty.to_string(),
                method: method.to_string(),
            }),
        }
    }

    /// Type an `UnsafePointer[T]` instance method. Currently only `free()` → `None`
    /// (indexed load/store go through `infer_index` / `check_place`).
    fn infer_pointer_method(
        &self,
        method: &str,
        elem: &Ty,
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        match method {
            "free" => {
                if !args.is_empty() {
                    return Err(TypeError::ArityMismatch {
                        name: "free".to_string(),
                        expected: 0,
                        got: args.len(),
                    });
                }
                Ok(Ty::None)
            }
            _ => Err(TypeError::NoSuchMethod {
                object_type: Ty::Pointer {
                    element: Box::new(elem.clone()),
                    origin: crate::origin::PointerOrigin::Legacy,
                }
                .to_string(),
                method: method.to_string(),
            }),
        }
    }

    /// If `recv` is a `struct` defining dunder method `name`, type the implicit
    /// call `recv.name(args…)` — the operator / subscript / builtin dispatch that
    /// turns a user struct into a first-class value type. Checks arity and argument
    /// coercion and returns the (type-argument-substituted) result type. Returns
    /// `None` when `recv` isn't a struct or has no such method, so the caller falls
    /// back to its own operator/builtin error.
    fn struct_dunder(&self, recv: &Ty, name: &str, args: &[&Ty]) -> Option<Result<Ty, TypeError>> {
        let Ty::Struct(sname, targs) = recv else {
            return None;
        };
        let info = self.structs.get(sname)?;
        let sig = info
            .methods
            .get(name)?
            .iter()
            .find(|sig| sig.params.len() == args.len())?;
        let subst = struct_subst(&info.decls, targs);
        let params: Vec<Ty> = sig.params.iter().map(|t| substitute(t, &subst)).collect();
        if params.len() != args.len() {
            return Some(Err(TypeError::ArityMismatch {
                name: name.to_string(),
                expected: params.len(),
                got: args.len(),
            }));
        }
        for (arg, expected) in args.iter().zip(&params) {
            if !coerces(arg, expected) {
                return Some(Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: arg.to_string(),
                    context: format!("argument to '{name}'"),
                }));
            }
        }
        Some(Ok(substitute(&sig.ret, &subst)))
    }

    /// Resolve a loop's complete iterator protocol.  In particular, owned
    /// iteration selects `__iter__(var self)` and never silently falls back to a
    /// borrowed `__iter__`.  The selected symbols cross the checked boundary so
    /// HIR/MIR/VM do not repeat overload selection.
    fn iteration_protocol(
        &self,
        ty: &Ty,
        owned: bool,
    ) -> Result<(Ty, crate::checked::IterationProtocol), TypeError> {
        use crate::checked::{IterationMode, IterationProtocol};
        let mode = if owned {
            IterationMode::Owned
        } else {
            IterationMode::Borrowed
        };
        let builtin = |element| {
            (
                element,
                IterationProtocol {
                    mode,
                    prepare: Vec::new(),
                    has_next: None,
                    next: None,
                },
            )
        };
        match ty {
            Ty::Range => Ok(builtin(Ty::Int)),
            Ty::List(elem) | Ty::Set(elem) => Ok(builtin((**elem).clone())),
            Ty::Dict(key, _) => Ok(builtin((**key).clone())),
            Ty::Struct(..) => self.struct_iteration_protocol(ty, mode, 0),
            Ty::Param { bounds, .. } => {
                let required = if owned { "IterableOwned" } else { "Iterable" };
                if !bounds.iter().any(|bound| bound == required)
                    && self.lookup_trait_assoc_type(bounds, "Element").is_none()
                {
                    return Err(TypeError::TypeMismatch {
                        expected: format!("a type conforming to {required}"),
                        found: ty.to_string(),
                        context: "for-loop iterable".to_string(),
                    });
                }
                if owned && !bounds.iter().any(|bound| bound == "IterableOwned") {
                    return Err(TypeError::TraitNotSatisfied {
                        param: "T".to_string(),
                        ty: ty.to_string(),
                        trait_name: "IterableOwned".to_string(),
                        reason: Some(
                            "owned iteration requires an ownership-consuming iterator".to_string(),
                        ),
                    });
                }
                Ok((
                    Ty::Assoc {
                        base: Box::new(ty.clone()),
                        name: "Element".to_string(),
                    },
                    IterationProtocol {
                        mode,
                        prepare: vec!["__trait_dispatch.__iter__".to_string()],
                        has_next: Some("__iterator_dispatch.__len__".to_string()),
                        next: Some("__iterator_dispatch.__next__".to_string()),
                    },
                ))
            }
            other => Err(TypeError::TypeMismatch {
                expected: if owned {
                    "range, a builtin collection, or a type with __iter__(var self)"
                } else {
                    "range, a builtin collection, or a type with borrowed __iter__"
                }
                .to_string(),
                found: other.to_string(),
                context: "for-loop iterable".to_string(),
            }),
        }
    }

    fn struct_iteration_protocol(
        &self,
        c_ty: &Ty,
        mode: crate::checked::IterationMode,
        depth: usize,
    ) -> Result<(Ty, crate::checked::IterationProtocol), TypeError> {
        use crate::checked::IterationMode;
        let no_method = |ty: &Ty, m: &str| TypeError::NoSuchMethod {
            object_type: ty.to_string(),
            method: m.to_string(),
        };
        if depth >= 8 {
            return Err(TypeError::Unsupported(
                "iterator normalization exceeded eight __iter__ steps".to_string(),
            ));
        }
        let Ty::Struct(cname, ctargs) = c_ty else {
            return Err(no_method(c_ty, "__iter__"));
        };
        let cinfo = self.structs.get(cname).ok_or_else(|| {
            TypeError::InvariantViolation(format!("struct '{cname}' was not registered"))
        })?;
        let candidates = cinfo
            .methods
            .get("__iter__")
            .ok_or_else(|| no_method(c_ty, "__iter__"))?;
        let mut matching = candidates.iter().filter(|sig| {
            sig.params.is_empty()
                && match mode {
                    IterationMode::Owned => {
                        sig.self_convention == Some(crate::ast::ArgConvention::Var)
                    }
                    IterationMode::Borrowed => matches!(
                        sig.self_convention,
                        None | Some(
                            crate::ast::ArgConvention::Read | crate::ast::ArgConvention::Ref
                        )
                    ),
                }
        });
        let iter_sig = matching.next().ok_or_else(|| TypeError::TypeMismatch {
            expected: match mode {
                IterationMode::Owned => "an '__iter__(var self)' method",
                IterationMode::Borrowed => "a borrowed '__iter__' method",
            }
            .to_string(),
            found: format!("{}.__iter__", c_ty),
            context: "for-loop iterator selection".to_string(),
        })?;
        if matching.next().is_some() {
            return Err(TypeError::BadCall {
                func: format!("{cname}.__iter__"),
                reason: "ambiguous iterator receiver convention".to_string(),
            });
        }
        let prepare_symbol = if candidates.len() > 1 {
            method_lowered_name(cname, "__iter__", iter_sig)
        } else {
            format!("{cname}.__iter__")
        };
        let it_ty = substitute(&iter_sig.ret, &struct_subst(&cinfo.decls, ctargs));
        if let Ty::List(elem) | Ty::Set(elem) = &it_ty {
            return Ok((
                (**elem).clone(),
                crate::checked::IterationProtocol {
                    mode,
                    prepare: vec![prepare_symbol],
                    has_next: None,
                    next: None,
                },
            ));
        }
        if let Ty::Dict(key, _) = &it_ty {
            return Ok((
                (**key).clone(),
                crate::checked::IterationProtocol {
                    mode,
                    prepare: vec![prepare_symbol],
                    has_next: None,
                    next: None,
                },
            ));
        }
        // The iterator must itself be a struct with `__next__` and `__len__`.
        let bad_iter = || TypeError::TypeMismatch {
            expected: "List or an iterator struct (with __next__ and __len__)".to_string(),
            found: it_ty.to_string(),
            context: "__iter__ return type".to_string(),
        };
        let Ty::Struct(iname, itargs) = &it_ty else {
            return Err(bad_iter());
        };
        let iinfo = self.structs.get(iname).ok_or_else(bad_iter)?;
        if !iinfo.methods.contains_key("__next__") && iinfo.methods.contains_key("__iter__") {
            let (element, mut nested) = self.struct_iteration_protocol(&it_ty, mode, depth + 1)?;
            nested.prepare.insert(0, prepare_symbol);
            return Ok((element, nested));
        }
        let isubst = struct_subst(&iinfo.decls, itargs);
        // `__len__(self) -> Int` — bounded iteration (loop while `len(it) > 0`).
        let len_sig = iinfo
            .methods
            .get("__len__")
            .and_then(|sigs| sigs.iter().find(|sig| sig.params.is_empty()))
            .ok_or_else(|| no_method(&it_ty, "__len__"))?;
        let len_ret = substitute(&len_sig.ret, &isubst);
        if len_ret != Ty::Int {
            return Err(TypeError::TypeMismatch {
                expected: "Int".to_string(),
                found: len_ret.to_string(),
                context: "return type of iterator '__len__'".to_string(),
            });
        }
        // `__next__(mut self) -> Element` — advances, so it must mutate `self`.
        let next_sig = iinfo
            .methods
            .get("__next__")
            .and_then(|sigs| sigs.iter().find(|sig| sig.params.is_empty()))
            .ok_or_else(|| no_method(&it_ty, "__next__"))?;
        if !next_sig.params.is_empty() {
            return Err(TypeError::ArityMismatch {
                name: "__next__".to_string(),
                expected: 0,
                got: next_sig.params.len(),
            });
        }
        if !matches!(
            next_sig.self_convention,
            Some(crate::ast::ArgConvention::Mut)
        ) {
            return Err(TypeError::TypeMismatch {
                expected: "a 'mut self' __next__".to_string(),
                found: "read-only self".to_string(),
                context: "iterator '__next__'".to_string(),
            });
        }
        Ok((
            substitute(&next_sig.ret, &isubst),
            crate::checked::IterationProtocol {
                mode,
                prepare: vec![prepare_symbol],
                has_next: Some(
                    if iinfo
                        .methods
                        .get("__len__")
                        .is_some_and(|methods| methods.len() > 1)
                    {
                        method_lowered_name(iname, "__len__", len_sig)
                    } else {
                        format!("{iname}.__len__")
                    },
                ),
                next: Some(
                    if iinfo
                        .methods
                        .get("__next__")
                        .is_some_and(|methods| methods.len() > 1)
                    {
                        method_lowered_name(iname, "__next__", next_sig)
                    } else {
                        format!("{iname}.__next__")
                    },
                ),
            },
        ))
    }

    /// Type a `List` method call. The **mutating** methods (`append`, `insert`,
    /// `remove`, `pop`, `clear`, `reverse`, `extend`) require a plain variable
    /// receiver (so they can mutate its binding in place); the **query** methods
    /// (`count`, `index`) work on any list. `remove`/`count`/`index` require an
    /// equatable element type.
    fn infer_list_method(
        &self,
        object: &Expr,
        method: &str,
        elem: &Ty,
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        let no_such = || TypeError::NoSuchMethod {
            object_type: Ty::List(Box::new(elem.clone())).to_string(),
            method: method.to_string(),
        };
        let mutating = matches!(
            method,
            "append" | "insert" | "remove" | "pop" | "clear" | "reverse" | "extend"
        );
        // A mutating method mutates its receiver, so the receiver must be a
        // writable place (a variable or a field/index chain rooted at one) —
        // not a temporary. Reading `check_place` validates exactly that.
        if mutating && self.check_place(object).is_err() {
            return Err(TypeError::MutationRequiresVariable(method.to_string()));
        }
        // `remove`/`count`/`index` compare elements, so require an equatable type.
        if matches!(method, "remove" | "count" | "index") && !is_list_equatable(elem) {
            return Err(TypeError::TypeMismatch {
                expected: "an equatable element type".to_string(),
                found: elem.to_string(),
                context: format!("'{}'", method),
            });
        }
        // Require the argument at position `i` to coerce to the element type.
        let expect_elem = |tys: &[Ty], i: usize| -> Result<(), TypeError> {
            if coerces(&tys[i], elem) {
                Ok(())
            } else {
                Err(TypeError::TypeMismatch {
                    expected: elem.to_string(),
                    found: tys[i].to_string(),
                    context: format!("argument to '{}'", method),
                })
            }
        };
        match method {
            "append" => {
                let tys = self.builtin_args("append", 1, args)?;
                expect_elem(&tys, 0)?;
                Ok(Ty::None)
            }
            "insert" => {
                let tys = self.builtin_args("insert", 2, args)?;
                if !coerces(&tys[0], &Ty::Int) {
                    return Err(TypeError::TypeMismatch {
                        expected: "Int".to_string(),
                        found: tys[0].to_string(),
                        context: "insert index".to_string(),
                    });
                }
                expect_elem(&tys, 1)?;
                Ok(Ty::None)
            }
            "remove" => {
                let tys = self.builtin_args("remove", 1, args)?;
                expect_elem(&tys, 0)?;
                Ok(Ty::None)
            }
            "pop" => {
                // `pop()` (last) or `pop(i)` — an optional `Int` index.
                if args.len() > 1 {
                    return Err(TypeError::ArityMismatch {
                        name: "pop".into(),
                        expected: 1,
                        got: args.len(),
                    });
                }
                if let Some(a) = args.first() {
                    let ity = self.infer(a)?;
                    if !coerces(&ity, &Ty::Int) {
                        return Err(TypeError::TypeMismatch {
                            expected: "Int".to_string(),
                            found: ity.to_string(),
                            context: "pop index".to_string(),
                        });
                    }
                }
                Ok(elem.clone())
            }
            "clear" | "reverse" => {
                self.builtin_args(method, 0, args)?;
                Ok(Ty::None)
            }
            "extend" => {
                let tys = self.builtin_args("extend", 1, args)?;
                if tys[0] != Ty::List(Box::new(elem.clone())) {
                    return Err(TypeError::TypeMismatch {
                        expected: Ty::List(Box::new(elem.clone())).to_string(),
                        found: tys[0].to_string(),
                        context: "argument to 'extend'".to_string(),
                    });
                }
                Ok(Ty::None)
            }
            "count" | "index" => {
                let tys = self.builtin_args(method, 1, args)?;
                expect_elem(&tys, 0)?;
                Ok(Ty::Int)
            }
            _ => Err(no_such()),
        }
    }

    /// Type the value-producing Tuple helpers in the current builtin surface.
    fn infer_tuple_method(
        &self,
        method: &str,
        elements: &[Ty],
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        match method {
            "reverse" => {
                self.builtin_args("reverse", 0, args)?;
                Ok(Ty::Tuple(elements.iter().rev().cloned().collect()))
            }
            "concat" => {
                let tys = self.builtin_args("concat", 1, args)?;
                let Ty::Tuple(other) = &tys[0] else {
                    return Err(TypeError::TypeMismatch {
                        expected: "a Tuple".to_string(),
                        found: tys[0].to_string(),
                        context: "argument to 'concat'".to_string(),
                    });
                };
                let mut result = elements.to_vec();
                result.extend(other.iter().cloned());
                Ok(Ty::Tuple(result))
            }
            _ => Err(TypeError::NoSuchMethod {
                object_type: Ty::Tuple(elements.to_vec()).to_string(),
                method: method.to_string(),
            }),
        }
    }

    /// Find every `method` required by the given trait `bounds`. Keeping the
    /// full candidate set is important: bounded calls use the same named-argument
    /// binder, generic specialization, overload ranking, and effect selection as
    /// concrete method calls.
    fn lookup_trait_methods(&self, bounds: &[String], method: &str, argc: usize) -> Vec<MethodSig> {
        let mut methods = Vec::new();
        // The built-in `Hashable` trait contributes `__hash__(self) -> UInt`
        // (roadmap milestone 6). A user trait cannot shadow a built-in name, so this is
        // unambiguous.
        if method == "__hash__" && argc == 0 && bounds.iter().any(|b| b == "Hashable") {
            methods.push(MethodSig::intrinsic(vec![], Ty::UInt));
        }
        // The built-in numeric-rounding traits contribute a `-> Self` dunder
        // (roadmap milestone 7), used by the self-hosted `math` module: `Floorable`/
        // `Ceilable`/`Truncable` a nullary `__floor__`/`__ceil__`/`__trunc__`,
        // and `CeilDivable`/`CeilDivableRaising` a unary `__ceildiv__(Self)`.
        let accepts = math_dunder_bound(method, argc);
        if !accepts.is_empty() && bounds.iter().any(|b| accepts.contains(&b.as_str())) {
            let params = if argc == 1 {
                vec![Ty::SelfType]
            } else {
                vec![]
            };
            methods.push(MethodSig::intrinsic(params, Ty::SelfType));
        }
        for bound in bounds {
            let Some(signatures) = self
                .traits
                .get(bound)
                .and_then(|info| info.methods.get(method))
            else {
                continue;
            };
            for signature in signatures {
                if !methods.contains(signature) {
                    methods.push(signature.clone());
                }
            }
        }
        methods
    }

    /// Find one exact-arity trait method for expression forms that have not yet
    /// been generalized to full call syntax (currently subscripting).
    fn lookup_trait_method(
        &self,
        bounds: &[String],
        method: &str,
        argc: usize,
    ) -> Option<MethodSig> {
        self.lookup_trait_methods(bounds, method, argc)
            .into_iter()
            .find(|signature| signature.params.len() == argc)
    }

    /// Find a type-valued associated comptime member required by any of the
    /// given trait bounds. Built-in bounds contribute none.
    fn lookup_trait_assoc_type(&self, bounds: &[String], member: &str) -> Option<Vec<String>> {
        bounds
            .iter()
            .filter_map(|b| self.traits.get(b))
            .find_map(|info| match info.comptime_members.get(member) {
                Some(CtMemberReq::Type { bounds }) => Some(bounds.clone()),
                _ => None,
            })
    }

    /// Find a value-valued associated comptime member required by a bound trait.
    fn lookup_trait_assoc_value_ty(&self, bounds: &[String], member: &str) -> Option<Ty> {
        bounds
            .iter()
            .filter_map(|b| self.traits.get(b))
            .find_map(|info| match info.comptime_members.get(member) {
                Some(CtMemberReq::Value(ty)) => Some(ty.clone()),
                _ => None,
            })
    }

    fn infer_prefix(&self, op: PrefixOp, operand: &Expr) -> Result<Ty, TypeError> {
        let t = self.infer(operand)?;
        match (op, &t) {
            // Negation preserves the (possibly literal) numeric type, except UInt.
            (PrefixOp::Neg, Ty::Int | Ty::Float64 | Ty::IntLiteral | Ty::FloatLiteral) => Ok(t),
            (PrefixOp::Not, Ty::Bool) => Ok(Ty::Bool),
            _ => Err(TypeError::BadOperator {
                op: prefix_symbol(op).to_string(),
                operands: t.to_string(),
            }),
        }
    }

    fn infer_infix(&self, op: InfixOp, left: &Expr, right: &Expr) -> Result<Ty, TypeError> {
        let lt = self.infer(left)?;
        let rt = self.infer(right)?;
        use InfixOp::*;

        // Membership `in` / `not in` — the right operand is a container.
        if matches!(op, In | NotIn) {
            return self.infer_membership(op, &lt, &rt);
        }
        // SIMD operators are elementwise (handled before the scalar-numeric path).
        if matches!(lt, Ty::Simd { .. }) || matches!(rt, Ty::Simd { .. }) {
            return self.infer_simd_infix(op, &lt, &rt);
        }
        if let Ty::Pointer { element, .. } = &lt {
            match (op, &rt) {
                (Add | Sub, Ty::Int | Ty::IntLiteral) => return Ok(lt.clone()),
                (Sub, Ty::Pointer { element: other, .. }) if element == other => {
                    return Ok(Ty::Int);
                }
                (Eq | Ne, Ty::Pointer { element: other, .. }) if element == other => {
                    return Ok(Ty::Bool);
                }
                _ => {}
            }
        }

        // Tuple comparisons are structural. Equality accepts independently
        // equatable element packs (different element types simply compare
        // unequal); ordering requires a lexicographically compatible prefix.
        if let (Ty::Tuple(left), Ty::Tuple(right)) = (&lt, &rt) {
            // Current Tuple comparison methods take `other: Self`: different
            // arities or element packs are not comparable merely because the VM
            // could walk both vectors. Literal element coercion may still make
            // the two tuple types the same contextual `Self`.
            let same_self = coerces(&lt, &rt) || coerces(&rt, &lt);
            let supported = match op {
                Eq | Ne => {
                    same_self && tuple_elements_equatable(left) && tuple_elements_equatable(right)
                }
                Lt | Gt | Le | Ge => same_self && tuple_order_compatible(left, right),
                _ => false,
            };
            if supported {
                return Ok(Ty::Bool);
            }
        }
        if let (Ty::Variant(left), Ty::Variant(right)) = (&lt, &rt)
            && left == right
            && matches!(op, Eq | Ne)
            && left
                .iter()
                .all(|alternative| has_equality_bound_or_concrete(self, alternative))
        {
            return Ok(Ty::Bool);
        }

        // `common` is the unified numeric type when both operands are numeric
        // (literals coerced as needed), else None.
        let common = common_numeric(&lt, &rt);
        let result = match op {
            // Short-circuiting boolean logic requires `Bool` operands.
            And | Or if lt == Ty::Bool && rt == Ty::Bool => Some(Ty::Bool),
            // `+` concatenates String, or adds numbers (result = common type).
            Add if lt == Ty::String && rt == Ty::String => Some(Ty::String),
            // `**` between equal opaque type parameters bounded by `Powable`
            // (`__pow__(self, Self) -> Self`); the concrete impl runs after
            // erasure. Checked before the numeric arm since a `Param` isn't
            // numeric (so `common` is None here).
            Pow if lt == rt && param_has_bound(&lt, "Powable") => Some(lt.clone()),
            // Arithmetic that preserves the operand type.
            Add | Sub | Mul | FloorDiv | Mod | Pow => common,
            Shl | Shr | BitAnd | BitOr | BitXor
                if common
                    .as_ref()
                    .is_some_and(|ty| matches!(ty, Ty::Int | Ty::UInt | Ty::IntLiteral)) =>
            {
                common.map(|ty| default_literal(&ty))
            }
            // True division always yields Float64 (for any numeric operands).
            Div if common.is_some() => Some(Ty::Float64),
            // Ordering between numbers, or between equal opaque type parameters
            // whose bound promises an ordering (`T: Comparable`).
            Lt | Gt | Le | Ge if common.is_some() || (lt == rt && has_order_bound(&lt)) => {
                Some(Ty::Bool)
            }
            // Equality: between numbers (any common type), or equal non-numeric
            // scalars (Bool/String/None).
            Eq | Ne
                if common.is_some()
                    || (lt == rt
                        && (is_scalar(&lt)
                            || has_equality_bound(&lt)
                            || matches!(&lt, Ty::Set(element) if is_list_equatable(element))
                            || matches!(&lt, Ty::Dict(key, value) if is_list_equatable(key) && is_list_equatable(value)))) =>
            {
                Some(Ty::Bool)
            }
            _ => None,
        };
        if let Some(ty) = result {
            return Ok(ty);
        }
        // Operator overloading: `a OP b` on a user struct dispatches to the left
        // operand's dunder method (`a.__add__(b)`, `a.__eq__(b)`, …).
        if let Some(dunder) = op.dunder()
            && let Some(r) = self.struct_dunder(&lt, dunder, &[&rt])
        {
            return r;
        }
        Err(TypeError::BadOperator {
            op: infix_symbol(op).to_string(),
            operands: format!("{} and {}", lt, rt),
        })
    }

    /// Type a membership test `x in c` / `x not in c` → `Bool`. The container is
    /// a `List[T]`, heterogeneous `Tuple`, or `String` (substring test).
    fn infer_membership(&self, op: InfixOp, lt: &Ty, rt: &Ty) -> Result<Ty, TypeError> {
        let ok = match rt {
            Ty::List(elem) => coerces(lt, elem) && is_list_equatable(elem),
            Ty::Set(elem) => coerces(lt, elem) && is_list_equatable(elem),
            Ty::Dict(key, _) => coerces(lt, key) && is_list_equatable(key),
            Ty::Tuple(_) => match lt {
                Ty::Tuple(elements) => tuple_elements_equatable(elements),
                other => is_list_equatable(other),
            },
            Ty::String => *lt == Ty::String,
            _ => false,
        };
        if ok {
            return Ok(Ty::Bool);
        }
        // `x in c` on a user struct dispatches to the container's `__contains__`
        // (`c.__contains__(x)`), which must return `Bool`.
        if let Some(r) = self.struct_dunder(rt, "__contains__", &[lt]) {
            return r.and_then(|ret| require_dunder_ret(ret, &Ty::Bool, "__contains__"));
        }
        Err(TypeError::BadOperator {
            op: infix_symbol(op).to_string(),
            operands: format!("{} and {}", lt, rt),
        })
    }

    /// Type an elementwise SIMD operator. Both operands must be the same SIMD
    /// type, except a numeric *literal* splats to the other operand's type.
    /// Arithmetic keeps the operand type; comparisons return a `bool` mask.
    fn infer_simd_infix(&self, op: InfixOp, lt: &Ty, rt: &Ty) -> Result<Ty, TypeError> {
        use InfixOp::*;
        let bad = || TypeError::BadOperator {
            op: infix_symbol(op).to_string(),
            operands: format!("{} and {}", lt, rt),
        };
        // Determine the common SIMD type, allowing a numeric literal on one side.
        let simd = match (lt, rt) {
            (
                Ty::Simd {
                    dtype: d1,
                    width: w1,
                },
                Ty::Simd {
                    dtype: d2,
                    width: w2,
                },
            ) if d1 == d2 && w1 == w2 => Ty::Simd {
                dtype: *d1,
                width: *w1,
            },
            (Ty::Simd { dtype, width }, other) | (other, Ty::Simd { dtype, width })
                if splats_to(other, *dtype) =>
            {
                Ty::Simd {
                    dtype: *dtype,
                    width: *width,
                }
            }
            _ => return Err(bad()),
        };
        let Ty::Simd { dtype, width } = simd else {
            return Err(TypeError::InvariantViolation(
                "SIMD operator inference produced a non-SIMD type".to_string(),
            ));
        };
        match op {
            // Elementwise arithmetic on numeric lanes preserves the type.
            Add | Sub | Mul if dtype != Dtype::Bool => Ok(simd_ty(dtype, width)),
            // True division is defined on float lanes only.
            Div if dtype.is_float() => Ok(simd_ty(dtype, width)),
            // Equality on any lanes; ordering on numeric lanes — a bool mask.
            Eq | Ne => Ok(simd_ty(Dtype::Bool, width)),
            Lt | Gt | Le | Ge if dtype != Dtype::Bool => Ok(simd_ty(Dtype::Bool, width)),
            _ => Err(bad()),
        }
    }

    /// Type `SIMD[DType.<dt>, width](args)`: `width` element arguments, or a
    /// single argument that splats across all lanes; each must fit the dtype.
    fn infer_simd_construction(
        &self,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        let (dtype, mut width) = self.simd_dims(param_args)?;
        if width == -1 {
            width = i64::try_from(args.len()).unwrap_or(0);
            if width < 1 || (width & (width - 1)) != 0 {
                return Err(TypeError::BadSimdWidth(width.to_string()));
            }
        }
        self.check_simd_args(dtype, width, args)?;
        Ok(simd_ty(dtype, width))
    }

    /// Type a scalar-alias construction `Int32(x)` = `SIMD[DType.int32, 1](x)`.
    fn infer_simd_alias_construction(
        &self,
        dtype: Dtype,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        if !param_args.is_empty() {
            return Err(TypeError::WrongTypeArgCount {
                name: dtype.scalar_alias().unwrap_or("SIMD").to_string(),
                expected: 0,
                got: param_args.len(),
            });
        }
        self.check_simd_args(dtype, 1, args)?;
        Ok(Ty::Simd { dtype, width: 1 })
    }

    /// Check the element arguments of a SIMD construction: either `width` of them
    /// (one per lane) or exactly one (splatted), each fitting `dtype`.
    fn check_simd_args(&self, dtype: Dtype, width: i64, args: &[Expr]) -> Result<(), TypeError> {
        if args.len() != width as usize && args.len() != 1 {
            return Err(TypeError::SimdArity {
                width,
                got: args.len(),
            });
        }
        for arg in args {
            let aty = self.infer(arg)?;
            if !splats_to(&aty, dtype) {
                return Err(TypeError::TypeMismatch {
                    expected: format!("a DType.{} element", dtype.name()),
                    found: aty.to_string(),
                    context: "SIMD element".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Type the built-in `Error(msg)` constructor: one `String` argument.
    fn infer_error_construction(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        if args.len() != 1 {
            return Err(TypeError::ArityMismatch {
                name: "Error".to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        let aty = self.infer(&args[0])?;
        if aty != Ty::String {
            return Err(TypeError::TypeMismatch {
                expected: "String".to_string(),
                found: aty.to_string(),
                context: "argument to 'Error'".to_string(),
            });
        }
        Ok(Ty::Error)
    }

    fn infer_slice_construction(&self, name: &str, args: &[Expr]) -> Result<Ty, TypeError> {
        let valid_arity = match name {
            "Slice" => matches!(args.len(), 2 | 3),
            "slice" => matches!(args.len(), 1..=3),
            _ => false,
        };
        if !valid_arity {
            return Err(TypeError::ArityMismatch {
                name: name.to_string(),
                expected: if name == "Slice" { 2 } else { 1 },
                got: args.len(),
            });
        }
        for argument in args {
            let found = self.infer(argument)?;
            if found != Ty::None && !coerces(&found, &Ty::Int) {
                return Err(TypeError::TypeMismatch {
                    expected: "Int or None".to_string(),
                    found: found.to_string(),
                    context: format!("argument to '{name}'"),
                });
            }
        }
        Ok(Ty::Struct("Slice".to_string(), Vec::new()))
    }

    fn infer_call(
        &self,
        span: SourceSpan,
        name: &str,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Ty, TypeError> {
        if is_variant_name(name)
            && (name != "Variant" || self.structs.contains_key(name))
            && self.lookup(name).is_none()
        {
            return self.infer_variant_construction(span, param_args, args, kwargs);
        }
        let ty = match self.lookup(name) {
            Some(ty) => ty.clone(),
            // Built-ins and struct construction, resolved only when the name
            // isn't shadowed by a binding.
            None => match name {
                _ if self.structs.contains_key(name) => {
                    return self.infer_construction(span, name, param_args, args, kwargs);
                }
                _ if !kwargs.is_empty() => {
                    return Err(TypeError::BadCall {
                        func: name.to_string(),
                        reason: "keyword arguments are not supported here".to_string(),
                    });
                }
                "print" => return self.infer_print(args),
                "String" => return self.infer_stringify(args),
                "repr" => {
                    let tys = self.builtin_args("repr", 1, args)?;
                    if self.conforms_to(&tys[0], "Writable") {
                        return Ok(Ty::String);
                    }
                    return Err(TypeError::TypeMismatch {
                        expected: "Writable".to_string(),
                        found: tys[0].to_string(),
                        context: "argument to 'repr'".to_string(),
                    });
                }
                "hash" => {
                    let tys = self.builtin_args("hash", 1, args)?;
                    if self.conforms_to(&tys[0], "Hashable") {
                        return Ok(Ty::UInt);
                    }
                    return Err(TypeError::TraitNotSatisfied {
                        param: "T".to_string(),
                        ty: tys[0].to_string(),
                        trait_name: "Hashable".to_string(),
                        reason: self.trait_failure_reason(&tys[0], "Hashable"),
                    });
                }
                "abs" => return self.infer_abs(args),
                "min" | "max" => return self.infer_min_max(name, args),
                "round" => return self.infer_round(args),
                "input" => return self.infer_input(args),
                "len" => return self.infer_len(args),
                "range" => return self.infer_range(args),
                "Slice" | "slice" => return self.infer_slice_construction(name, args),
                "Int" => return self.infer_conversion(Ty::Int, args),
                "UInt" => return self.infer_conversion(Ty::UInt, args),
                "Float64" => return self.infer_conversion(Ty::Float64, args),
                "Bool" => return self.infer_conversion(Ty::Bool, args),
                "divmod" => return self.infer_divmod(args),
                "SIMD" => return self.infer_simd_construction(param_args, args),
                "Scalar" => {
                    if param_args.len() != 1 {
                        return Err(TypeError::WrongTypeArgCount {
                            name: "Scalar".to_string(),
                            expected: 1,
                            got: param_args.len(),
                        });
                    }
                    let dtype = dtype_from_arg(&param_args[0])?;
                    self.check_simd_args(dtype, 1, args)?;
                    return Ok(simd_ty(dtype, 1));
                }
                "List" => return self.infer_list_construction(param_args, args),
                "Set" => {
                    let Ty::Set(element) = self.set_type(param_args)? else {
                        unreachable!("Set type helper returns Set")
                    };
                    for argument in args {
                        let actual = self.infer(argument)?;
                        if !coerces(&actual, &element) {
                            return Err(TypeError::TypeMismatch {
                                expected: element.to_string(),
                                found: actual.to_string(),
                                context: "Set construction element".to_string(),
                            });
                        }
                        self.check_consuming(argument, &actual, "Set construction element")?;
                    }
                    return Ok(Ty::Set(element));
                }
                "Dict" => {
                    if !args.is_empty() {
                        return Err(TypeError::ArityMismatch {
                            name: "Dict".to_string(),
                            expected: 0,
                            got: args.len(),
                        });
                    }
                    return self.dict_type(param_args);
                }
                "Tuple" => return self.infer_tuple_construction(param_args, args),
                "Error" => return self.infer_error_construction(args),
                _ if Dtype::from_scalar_alias(name).is_some() => {
                    let dtype = Dtype::from_scalar_alias(name)
                        .expect("match guard established a scalar alias");
                    return self.infer_simd_alias_construction(dtype, param_args, args);
                }
                _ => return Err(TypeError::UndefinedVariable(name.to_string())),
            },
        };
        if let Ty::Overload(candidates) = ty {
            let mut matches = Vec::new();
            for candidate in &candidates {
                let saved_conversions = self.implicit_conversions.borrow().clone();
                if let Ok((ret, score, error)) =
                    self.infer_callable_ty(name, candidate.clone(), param_args, args, kwargs)
                    && let Some(target) = callable_lowered_name(name, candidate)
                {
                    matches.push((ret, score, target, error));
                }
                *self.implicit_conversions.borrow_mut() = saved_conversions;
            }
            return match select_callable_overload(matches) {
                Ok((ret, target, error)) => {
                    self.overload_targets
                        .borrow_mut()
                        .insert(span.clone(), target.clone());
                    if let Some(selected) = candidates.iter().find(|candidate| {
                        callable_lowered_name(name, candidate).as_deref() == Some(target.as_str())
                    }) {
                        self.infer_callable_ty(name, selected.clone(), param_args, args, kwargs)?;
                    }
                    if let Some(error) = error.filter(|ty| *ty != Ty::Never) {
                        self.record_call_effect(span.clone(), error.clone());
                        self.require_error(format!("call to raising function '{name}'"), error)?;
                    }
                    Ok(ret)
                }
                Err(OverloadSelect::NoMatch) => Err(TypeError::BadCall {
                    func: name.to_string(),
                    reason: "no overload matches the supplied arguments".to_string(),
                }),
                Err(OverloadSelect::Ambiguous) => Err(TypeError::BadCall {
                    func: name.to_string(),
                    reason: "ambiguous overloaded call".to_string(),
                }),
            };
        }
        let (ret, _, error) = self.infer_callable_ty(name, ty, param_args, args, kwargs)?;
        if let Some(error) = error.filter(|ty| *ty != Ty::Never) {
            self.record_call_effect(span, error.clone());
            self.require_error(format!("call to raising function '{name}'"), error)?;
        }
        Ok(ret)
    }

    fn infer_callable_ty(
        &self,
        name: &str,
        ty: Ty,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<(Ty, usize, Option<Ty>), TypeError> {
        let (
            params,
            names,
            ret,
            required,
            variadic,
            kw_variadic,
            positional_only,
            keyword_only,
            _raises,
            error,
            conventions,
            ref_params,
            ref_return,
        ) = match ty {
            Ty::Struct(struct_name, _) => {
                let callable = self
                    .structs
                    .get(&struct_name)
                    .and_then(|info| info.callable_conformance.clone())
                    .ok_or_else(|| TypeError::NotCallable {
                        name: name.to_string(),
                        ty: struct_name.clone(),
                    })?;
                return self.infer_callable_ty(name, callable, param_args, args, kwargs);
            }
            // A non-generic function takes no compile-time parameters.
            Ty::Func {
                params,
                names,
                ret,
                required,
                variadic,
                kw_variadic,
                positional_only,
                keyword_only,
                raises,
                error,
                conventions,
                ref_params,
                ref_return,
            } => {
                if !param_args.is_empty() {
                    return Err(TypeError::WrongTypeArgCount {
                        name: name.to_string(),
                        expected: 0,
                        got: param_args.len(),
                    });
                }
                (
                    params,
                    names,
                    ret,
                    required,
                    variadic,
                    kw_variadic,
                    positional_only,
                    keyword_only,
                    raises,
                    error,
                    conventions,
                    ref_params,
                    ref_return,
                )
            }
            // Bind ordinary arguments first, then infer or apply the generic
            // function's compile-time parameters from the occupied slots.
            generic @ Ty::GenericFunc { .. } => {
                return self.infer_generic_call(name, &generic, param_args, args, kwargs);
            }
            other => {
                return Err(TypeError::NotCallable {
                    name: name.to_string(),
                    ty: other.to_string(),
                });
            }
        };

        // Match positional then keyword arguments to the regular parameter slots
        // (extra positional args overflow into a `*args` parameter), then check
        // each supplied argument coerces to its parameter's type (an unfilled slot
        // uses the default, already type-checked at the definition site).
        let forwarded_element = self.forwarded_kwargs_element(name, kwargs)?;
        let kw_names: Vec<&str> = kwargs
            .iter()
            .filter(|argument| !argument.is_forwarded())
            .map(|argument| argument.name.as_str())
            .collect();
        let has_kw_collector = kw_variadic.is_some();
        let kw_collector = kw_variadic.map(|element| *element);
        if forwarded_element.is_some() && kw_collector.is_none() {
            return Err(TypeError::BadCall {
                func: name.to_string(),
                reason: "`**kwargs^` requires a callee with a `**kwargs` collector".to_string(),
            });
        }
        let matched = match_call_slots(
            &names,
            &required,
            positional_only,
            keyword_only,
            args.len(),
            &kw_names,
            CallVariadics {
                positional: variadic.is_some(),
                keyword: kw_collector.is_some(),
            },
        )
        .map_err(|e| e.into_type_error(name))?;
        let (slots, overflow, kw_overflow) = (
            matched.slots,
            matched.positional_overflow,
            matched.keyword_overflow,
        );
        let mut score = 0;
        for (i, slot) in slots.iter().enumerate() {
            let arg = match slot {
                ArgSlot::Positional(p) => &args[*p],
                ArgSlot::Keyword(k) => &kwargs[*k].value,
                ArgSlot::Default => continue,
            };
            let arg_ty = self.infer_with_expected(arg, &params[i], true)?;
            if !self.record_implicit_conversion(arg, &arg_ty, &params[i])? {
                return Err(TypeError::TypeMismatch {
                    expected: params[i].to_string(),
                    found: arg_ty.to_string(),
                    context: format!("argument '{}' to '{}'", names[i], name),
                });
            }
            score += conversion_count(&arg_ty, &params[i]);
            // Only a `var`/`deinit` parameter *consumes* its argument (moving the
            // value in). `read` (the default), `mut`, and `ref` all **borrow** — no
            // copy — so passing a non-Copyable value to them is fine.
            if matches!(
                conventions.get(i),
                Some(Some(ArgConvention::Var | ArgConvention::Deinit))
            ) {
                self.check_consuming(
                    arg,
                    &arg_ty,
                    &format!("argument '{}' to '{}'", names[i], name),
                )?;
            }
        }
        // Each overflow argument must coerce to the `*args` element type.
        if let Some(elem) = &variadic {
            for (pack_index, &p) in overflow.iter().enumerate() {
                let expected = match &**elem {
                    Ty::Tuple(elements) => {
                        elements
                            .get(pack_index)
                            .ok_or_else(|| TypeError::ArityMismatch {
                                name: name.to_string(),
                                expected: elements.len(),
                                got: overflow.len(),
                            })?
                    }
                    _ => elem,
                };
                let arg_ty = self.infer_with_expected(&args[p], expected, true)?;
                if !coerces(&arg_ty, expected) {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: arg_ty.to_string(),
                        context: format!("variadic argument to '{}'", name),
                    });
                }
                score += conversion_count(&arg_ty, expected);
            }
            if let Ty::Tuple(elements) = &**elem
                && elements.len() != overflow.len()
            {
                return Err(TypeError::ArityMismatch {
                    name: name.to_string(),
                    expected: elements.len(),
                    got: overflow.len(),
                });
            }
        }
        if let Some(elem) = kw_collector {
            for index in kw_overflow {
                let expression = &kwargs[index].value;
                let found = self.infer_with_expected(expression, &elem, true)?;
                if !self.record_implicit_conversion(expression, &found, &elem)? {
                    return Err(TypeError::TypeMismatch {
                        expected: elem.to_string(),
                        found: found.to_string(),
                        context: format!(
                            "keyword '{}' collected by '{}'",
                            kwargs[index].name, name
                        ),
                    });
                }
                self.check_consuming(
                    expression,
                    &found,
                    &format!("keyword '{}' collected by '{name}'", kwargs[index].name),
                )?;
                score += conversion_count(&found, &elem);
            }
            if let Some(found) = forwarded_element
                && found != elem
            {
                return Err(TypeError::TypeMismatch {
                    expected: format!("StringDict[{elem}]"),
                    found: format!("StringDict[{found}]"),
                    context: format!("forwarded keyword arguments to '{name}'"),
                });
            }
        }

        // Borrow check (mutable-XOR-shared), root-sensitive: within one call a
        // variable borrowed exclusively (`mut`/`ref`) or moved (`^`) may not be
        // borrowed again — mutably, shared, or moved.
        let (effective_conventions, return_ref) = self.solve_call_origins(
            &slots,
            &conventions,
            &ref_params,
            ref_return.as_deref(),
            args,
            kwargs,
        )?;
        let copied_reads = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let expression = match slot {
                    ArgSlot::Positional(position) => &args[*position],
                    ArgSlot::Keyword(position) => &kwargs[*position].value,
                    ArgSlot::Default => return Ok(false),
                };
                let convention = effective_conventions.get(index).copied().flatten();
                Ok(
                    !matches!(convention, Some(ArgConvention::Mut | ArgConvention::Ref))
                        && self.is_copyable(&self.infer_with_expected(
                            expression,
                            &params[index],
                            true,
                        )?),
                )
            })
            .collect::<Result<Vec<_>, TypeError>>()?;
        check_call_aliasing(&slots, &effective_conventions, &copied_reads, args, kwargs)?;

        let result = return_ref
            .map(|mut reference| {
                reference.referent = ret.clone();
                Ty::Ref(reference)
            })
            .unwrap_or(*ret);
        Ok((
            result,
            overload_rank(score, variadic.is_some() || has_kw_collector, 0, false),
            error.map(|error| *error),
        ))
    }

    /// Type a call to a generic function: solve its type parameters from the
    /// argument types, then check each argument coerces to the substituted
    /// parameter type and return the substituted result type.
    fn infer_generic_call(
        &self,
        name: &str,
        generic: &Ty,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<(Ty, usize, Option<Ty>), TypeError> {
        let Ty::GenericFunc {
            decls,
            params,
            names,
            ret,
            required,
            variadic,
            kw_variadic,
            positional_only,
            keyword_only,
            raises: _,
            error,
            conventions,
            ref_params,
            ref_return,
        } = generic
        else {
            return Err(TypeError::InvariantViolation(format!(
                "generic call inference received non-generic callee '{name}'"
            )));
        };
        let forwarded_element = self.forwarded_kwargs_element(name, kwargs)?;
        if forwarded_element.is_some() && kw_variadic.is_none() {
            return Err(TypeError::BadCall {
                func: name.to_string(),
                reason: "`**kwargs^` requires a callee with a `**kwargs` collector".to_string(),
            });
        }
        let kw_names: Vec<&str> = kwargs
            .iter()
            .filter(|argument| !argument.is_forwarded())
            .map(|argument| argument.name.as_str())
            .collect();
        let matched = match_call_slots(
            names,
            required,
            *positional_only,
            *keyword_only,
            args.len(),
            &kw_names,
            CallVariadics {
                positional: variadic.is_some(),
                keyword: kw_variadic.is_some(),
            },
        )
        .map_err(|e| e.into_type_error(name))?;
        let (slots, overflow, kw_overflow) = (
            matched.slots,
            matched.positional_overflow,
            matched.keyword_overflow,
        );
        let mut use_params = Vec::new();
        let mut arg_tys = Vec::new();
        let mut arg_exprs = Vec::new();
        for (i, slot) in slots.iter().enumerate() {
            let arg = match slot {
                ArgSlot::Positional(p) => &args[*p],
                ArgSlot::Keyword(k) => &kwargs[*k].value,
                ArgSlot::Default => continue,
            };
            use_params.push(params[i].clone());
            arg_tys.push(self.infer(arg)?);
            arg_exprs.push(arg);
        }
        if let Some(elem) = variadic.as_deref() {
            for &p in &overflow {
                use_params.push(elem.clone());
                arg_tys.push(self.infer(&args[p])?);
                arg_exprs.push(&args[p]);
            }
        }
        let mut keyword_actuals = Vec::new();
        if let Some(element) = kw_variadic.as_deref() {
            for &index in &kw_overflow {
                let actual = self.infer(&kwargs[index].value)?;
                use_params.push(element.clone());
                arg_tys.push(actual.clone());
                keyword_actuals.push((index, actual));
            }
            if let Some(actual) = &forwarded_element {
                use_params.push(element.clone());
                arg_tys.push(actual.clone());
            }
        }
        let (subst, _tyargs) =
            self.resolve_use_params(name, decls, param_args, &use_params, &arg_tys)?;
        let mut conversions = 0;
        for ((aty, pty), expression) in arg_tys.iter().zip(&use_params).zip(arg_exprs) {
            if matches!(pty, Ty::Param { name, .. } if name.starts_with('*')) {
                // Each pack element was checked independently against the pack's
                // bounds during inference; there is intentionally no single
                // substituted element type to coerce every argument into.
                continue;
            }
            let expected = self.resolve_assoc_ty(&substitute(pty, &subst));
            if !self.record_implicit_conversion(expression, aty, &expected)? {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: aty.to_string(),
                    context: format!("argument to '{}'", name),
                });
            }
            conversions += conversion_count(aty, &expected);
        }
        if let Some(element) = kw_variadic.as_deref() {
            let expected = self.resolve_assoc_ty(&substitute(element, &subst));
            for (index, actual) in keyword_actuals {
                let expression = &kwargs[index].value;
                if !self.record_implicit_conversion(expression, &actual, &expected)? {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.to_string(),
                        found: actual.to_string(),
                        context: format!(
                            "keyword '{}' collected by '{}'",
                            kwargs[index].name, name
                        ),
                    });
                }
                self.check_consuming(
                    expression,
                    &actual,
                    &format!("keyword '{}' collected by '{name}'", kwargs[index].name),
                )?;
                conversions += conversion_count(&actual, &expected);
            }
            if let Some(actual) = forwarded_element
                && actual != expected
            {
                return Err(TypeError::TypeMismatch {
                    expected: format!("StringDict[{expected}]"),
                    found: format!("StringDict[{actual}]"),
                    context: format!("forwarded keyword arguments to '{name}'"),
                });
            }
        }
        for (i, slot) in slots.iter().enumerate() {
            if matches!(
                conventions.get(i),
                Some(Some(ArgConvention::Var | ArgConvention::Deinit))
            ) {
                let arg = match slot {
                    ArgSlot::Positional(p) => &args[*p],
                    ArgSlot::Keyword(k) => &kwargs[*k].value,
                    ArgSlot::Default => continue,
                };
                let ty = self.infer(arg)?;
                self.check_consuming(arg, &ty, &format!("argument '{}' to '{}'", names[i], name))?;
            }
        }
        let (effective_conventions, return_ref) = self.solve_call_origins(
            &slots,
            conventions,
            ref_params,
            ref_return.as_deref(),
            args,
            kwargs,
        )?;
        let copied_reads = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let expression = match slot {
                    ArgSlot::Positional(position) => &args[*position],
                    ArgSlot::Keyword(position) => &kwargs[*position].value,
                    ArgSlot::Default => return Ok(false),
                };
                let convention = effective_conventions.get(index).copied().flatten();
                Ok(
                    !matches!(convention, Some(ArgConvention::Mut | ArgConvention::Ref))
                        && self.is_copyable(&self.infer(expression)?),
                )
            })
            .collect::<Result<Vec<_>, TypeError>>()?;
        check_call_aliasing(&slots, &effective_conventions, &copied_reads, args, kwargs)?;
        let referent = self.resolve_assoc_ty(&substitute(ret, &subst));
        let result = return_ref
            .map(|mut reference| {
                reference.referent = Box::new(referent.clone());
                Ty::Ref(reference)
            })
            .unwrap_or(referent);
        let error = error
            .as_ref()
            .map(|error| self.resolve_assoc_ty(&substitute(error, &subst)));
        Ok((
            result,
            overload_rank(
                conversions,
                variadic.is_some() || kw_variadic.is_some(),
                decls.len(),
                true,
            ),
            error,
        ))
    }

    fn forwarded_kwargs_element(
        &self,
        callee: &str,
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Option<Ty>, TypeError> {
        let mut forwarded = kwargs.iter().filter(|argument| argument.is_forwarded());
        let Some(argument) = forwarded.next() else {
            return Ok(None);
        };
        if forwarded.next().is_some() {
            return Err(TypeError::BadCall {
                func: callee.to_string(),
                reason: "only one keyword dictionary can be forwarded".to_string(),
            });
        }
        if !matches!(&argument.value.kind, ExprKind::Transfer(_)) {
            return Err(TypeError::BadCall {
                func: callee.to_string(),
                reason: "keyword forwarding requires ownership transfer (`**kwargs^`)".to_string(),
            });
        }
        let found = self.infer(&argument.value)?;
        match found {
            Ty::Struct(name, args) if name == "StringDict" => match args.as_slice() {
                [TyArg::Ty(element)] => Ok(Some(element.clone())),
                _ => Err(TypeError::InvariantViolation(
                    "StringDict must carry one value type".to_string(),
                )),
            },
            other => Err(TypeError::TypeMismatch {
                expected: "StringDict[T]".to_string(),
                found: other.to_string(),
                context: format!("forwarded keyword arguments to '{callee}'"),
            }),
        }
    }

    fn solve_call_origins(
        &self,
        slots: &[ArgSlot],
        conventions: &[Option<ArgConvention>],
        signatures: &[Option<crate::origin::RefSig>],
        return_signature: Option<&crate::origin::RefSig>,
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<(Vec<Option<ArgConvention>>, Option<crate::origin::RefTy>), TypeError> {
        use crate::origin::{Mutability, Origin, RefTy, SigMutability};
        let mut effective = conventions.to_vec();
        let mut origins = vec![None; slots.len()];
        let mut mutable = vec![false; slots.len()];
        for (index, signature) in signatures.iter().enumerate() {
            let Some(signature) = signature else { continue };
            let Some(slot) = slots.get(index) else {
                continue;
            };
            let expression = match slot {
                ArgSlot::Positional(position) => &args[*position],
                ArgSlot::Keyword(position) => &kwargs[*position].value,
                ArgSlot::Default => continue,
            };
            let place = self.origin_place(expression)?;
            let is_mutable = self.owner_is_mutable(place.root);
            let requires_mutable = matches!(signature.mutability, SigMutability::Mutable);
            if requires_mutable && !is_mutable {
                return Err(TypeError::ImmutableBinding(
                    "reference argument".to_string(),
                ));
            }
            origins[index] = Some(Origin::Place(place));
            mutable[index] = match signature.mutability {
                SigMutability::Immutable => false,
                SigMutability::Mutable => true,
                SigMutability::BoolParam(_) | SigMutability::Infer => is_mutable,
            };
            if !mutable[index] {
                effective[index] = Some(ArgConvention::Read);
            }
        }
        for (index, signature) in signatures.iter().enumerate() {
            if signature.as_ref().is_some_and(|signature| {
                matches!(signature.origin, crate::origin::SigOrigin::Static)
            }) && origins.get(index).is_some_and(Option::is_some)
            {
                return Err(TypeError::Unsupported(
                    "a local place cannot satisfy StaticOrigin".to_string(),
                ));
            }
        }
        let returned = return_signature.map(|signature| {
            let origin = substitute_sig_origin(&signature.origin, &origins);
            let is_mutable = match &signature.mutability {
                SigMutability::Immutable => false,
                SigMutability::Mutable => true,
                SigMutability::BoolParam(name) => signatures.iter().enumerate().any(|(i, sig)| {
                    sig.as_ref().is_some_and(|sig| {
                        matches!(&sig.mutability, SigMutability::BoolParam(other) if other == name)
                            && mutable[i]
                    })
                }),
                SigMutability::Infer => origins
                    .iter()
                    .enumerate()
                    .any(|(i, o)| o.is_some() && mutable[i]),
            };
            RefTy {
                referent: Box::new(Ty::None), // replaced by the caller's declared return type
                origin,
                mutability: if is_mutable {
                    Mutability::Mutable
                } else {
                    Mutability::Immutable
                },
            }
        });
        Ok((effective, returned))
    }

    /// Type `print(...)`. Intrinsic scalar/container values have builtin writing;
    /// user values must opt into current `Writable`. Concrete implementations
    /// may override `write_to`; otherwise the runtime uses field reflection.
    fn infer_print(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        for (i, arg) in args.iter().enumerate() {
            let ty = self.infer(arg)?;
            if matches!(ty, Ty::Struct(..) | Ty::Variant(_)) {
                if self.conforms_to(&ty, "Writable") {
                    continue;
                }
                return Err(TypeError::TypeMismatch {
                    expected: "Writable".to_string(),
                    found: ty.to_string(),
                    context: format!("argument {} to 'print'", i + 1),
                });
            }
            if matches!(&ty, Ty::Param { bounds, .. } if bounds.iter().any(|bound| bound == "Writable"))
            {
                continue;
            }
            if !is_printable(&ty) {
                return Err(TypeError::TypeMismatch {
                    expected: "a printable value".to_string(),
                    found: ty.to_string(),
                    context: format!("argument {} to 'print'", i + 1),
                });
            }
        }
        Ok(Ty::None)
    }

    /// Type the built-in `input(prompt)`: prompt must be a `String`, result is the
    /// line read from standard input as a `String`.
    fn infer_input(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("input", 1, args)?;
        if tys[0] == Ty::String {
            Ok(Ty::String)
        } else {
            Err(TypeError::TypeMismatch {
                expected: "String".to_string(),
                found: tys[0].to_string(),
                context: "argument to 'input'".to_string(),
            })
        }
    }

    /// Require a built-in call to have exactly `n` arguments, and return the
    /// inferred type of each.
    fn builtin_args(&self, name: &str, n: usize, args: &[Expr]) -> Result<Vec<Ty>, TypeError> {
        if args.len() != n {
            return Err(TypeError::ArityMismatch {
                name: name.to_string(),
                expected: n,
                got: args.len(),
            });
        }
        args.iter().map(|a| self.infer(a)).collect()
    }

    /// Type `String(x)`: stringify a numeric, `Bool`, or `String` value.
    fn infer_stringify(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("String", 1, args)?;
        if is_numeric(&tys[0]) || tys[0] == Ty::Bool || tys[0] == Ty::String {
            return Ok(Ty::String);
        }
        if self.conforms_to(&tys[0], "Writable") {
            return Ok(Ty::String);
        }
        Err(TypeError::TypeMismatch {
            expected: "Writable".to_string(),
            found: tys[0].to_string(),
            context: "argument to 'String'".to_string(),
        })
    }

    /// Type `abs(x)`: a numeric argument, returning the same numeric type.
    fn infer_abs(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("abs", 1, args)?;
        // A numeric value, or an opaque `T: Absable` — `abs` returns the same type
        // (`__abs__(self) -> Self`); the concrete impl runs after type erasure.
        if is_numeric(&tys[0]) || param_has_bound(&tys[0], "Absable") {
            Ok(tys[0].clone())
        } else {
            Err(TypeError::TypeMismatch {
                expected: "a numeric value".to_string(),
                found: tys[0].to_string(),
                context: "argument to 'abs'".to_string(),
            })
        }
    }

    /// Type `min(a, b)` / `max(a, b)`: two numeric arguments unified like an
    /// operator (no concrete-type mixing), returning their common type.
    fn infer_min_max(&self, name: &str, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args(name, 2, args)?;
        common_numeric(&tys[0], &tys[1]).ok_or_else(|| TypeError::BadOperator {
            op: name.to_string(),
            operands: format!("{} and {}", tys[0], tys[1]),
        })
    }

    /// Type `round(x)`: a `Float64` argument returning `Float64`, or an opaque
    /// `T: Roundable` returning the same type (`__round__(self) -> Self`; the
    /// concrete impl runs after type erasure).
    fn infer_round(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("round", 1, args)?;
        if matches!(tys[0], Ty::Float64 | Ty::FloatLiteral) {
            Ok(Ty::Float64)
        } else if param_has_bound(&tys[0], "Roundable") {
            Ok(tys[0].clone())
        } else {
            Err(TypeError::TypeMismatch {
                expected: "Float64".to_string(),
                found: tys[0].to_string(),
                context: "argument to 'round'".to_string(),
            })
        }
    }

    /// Type `len(x)`: a `String`, `List`, or `Tuple` argument, returning `Int`.
    fn infer_len(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("len", 1, args)?;
        if matches!(
            tys[0],
            Ty::String | Ty::List(_) | Ty::Set(_) | Ty::Dict(_, _) | Ty::Tuple(_)
        ) {
            return Ok(Ty::Int);
        }
        // `len(c)` on a user struct dispatches to `c.__len__()` (`Sized`), which
        // must return `Int`.
        if let Some(r) = self.struct_dunder(&tys[0], "__len__", &[]) {
            return r.and_then(|ret| require_dunder_ret(ret, &Ty::Int, "__len__"));
        }
        // `len(x)` on an opaque type parameter is permitted when its bound
        // promises a length (`T: Sized`) — the concrete type's `__len__` runs at
        // runtime after type erasure.
        if has_len_bound(&tys[0]) {
            return Ok(Ty::Int);
        }
        Err(TypeError::TypeMismatch {
            expected: "String, List, or Tuple".to_string(),
            found: tys[0].to_string(),
            context: "argument to 'len'".to_string(),
        })
    }

    /// Type the built-in `range(stop)` / `range(start, stop)` /
    /// `range(start, stop, step)`. All arguments must be `Int`; the result is a
    /// `range`. A zero `step` is a *runtime* value error, not a type error.
    fn infer_range(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        if args.is_empty() {
            return Err(TypeError::ArityMismatch {
                name: "range".to_string(),
                expected: 1,
                got: 0,
            });
        }
        if args.len() > 3 {
            return Err(TypeError::ArityMismatch {
                name: "range".to_string(),
                expected: 3,
                got: args.len(),
            });
        }
        for (i, arg) in args.iter().enumerate() {
            let arg_ty = self.infer(arg)?;
            if !coerces(&arg_ty, &Ty::Int) {
                return Err(TypeError::TypeMismatch {
                    expected: "Int".to_string(),
                    found: arg_ty.to_string(),
                    context: format!("argument {} to 'range'", i + 1),
                });
            }
        }
        Ok(Ty::Range)
    }

    /// Type a conversion built-in `Int(x)` / `UInt(x)` / `Float64(x)` / `Bool(x)`:
    /// exactly one argument of a numeric or `Bool` type, producing `target`. An
    /// opaque type parameter is also accepted when its bound promises the
    /// conversion — `Int(x)` on `T: Intable`, `Float64(x)` on `T: Floatable`,
    /// `Bool(x)` on `T: Boolable` (`__int__`/`__float__`/`__bool__` run after
    /// type erasure).
    fn infer_conversion(&self, target: Ty, args: &[Expr]) -> Result<Ty, TypeError> {
        if args.len() != 1 {
            return Err(TypeError::ArityMismatch {
                name: target.to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        let arg_ty = self.infer(&args[0])?;
        let bounded = match target {
            Ty::Int => param_has_bound(&arg_ty, "Intable"),
            Ty::Float64 => param_has_bound(&arg_ty, "Floatable"),
            Ty::Bool => param_has_bound(&arg_ty, "Boolable"),
            _ => false,
        };
        if !(is_numeric(&arg_ty) || arg_ty == Ty::Bool || bounded) {
            return Err(TypeError::TypeMismatch {
                expected: "a numeric or Bool value".to_string(),
                found: arg_ty.to_string(),
                context: format!("argument to '{}'", target),
            });
        }
        Ok(target)
    }

    /// Type the prelude built-in `divmod(a, b)` (`DivModable`) → `Tuple[T, T]`:
    /// two numeric arguments of a common type (like an operator), or two equal
    /// opaque type parameters bounded by `DivModable`.
    fn infer_divmod(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("divmod", 2, args)?;
        if let Some(common) = common_numeric(&tys[0], &tys[1]) {
            return Ok(Ty::Tuple(vec![common.clone(), common]));
        }
        if tys[0] == tys[1] && param_has_bound(&tys[0], "DivModable") {
            return Ok(Ty::Tuple(vec![tys[0].clone(), tys[0].clone()]));
        }
        Err(TypeError::BadOperator {
            op: "divmod".to_string(),
            operands: format!("{} and {}", tys[0], tys[1]),
        })
    }
}

/// Mojo's built-in traits that mojito recognizes in a type-parameter bound.
/// User-defined traits (and conformance checking) are a later phase, so a bound
/// must name one of these. `AnyType` is the least restrictive.
const BUILTIN_TRAITS: &[&str] = &[
    "AnyType",
    "ImplicitlyDeletable",
    "Movable",
    "Copyable",
    "ImplicitlyCopyable",
    "RegisterPassable",
    "TrivialRegisterPassable",
    "Defaultable",
    "Representable",
    "Writable",
    "Writer",
    "Boolable",
    "Intable",
    "Floatable",
    "Indexer",
    "Equatable",
    "Comparable",
    "Hashable",
    "Hasher",
    "Identifiable",
    "Sized",
    "SizedRaising",
    "Iterable",
    "IterableOwned",
    "Iterator",
    "Absable",
    "Powable",
    "Roundable",
    "Ceilable",
    "Floorable",
    "Truncable",
    "CeilDivable",
    "CeilDivableRaising",
    "DivModable",
];

mod places;
use places::*;

mod generics;
use generics::*;
mod declarations;
use declarations::*;

mod annotations;
use annotations::*;

mod calls;
use calls::*;
mod builtins;
use builtins::*;

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_origin_expr(
    expr: &Expr,
    origin_params: &HashSet<&str>,
    value_params: &HashSet<&str>,
) -> Result<(), TypeError> {
    match &expr.kind {
        ExprKind::Identifier(name)
            if name == "_"
                || name == "self"
                || name == "StaticOrigin"
                || name == "UntrackedOrigin"
                || name == "UnsafeAnyOrigin"
                || origin_params.contains(name.as_str())
                || value_params.contains(name.as_str()) =>
        {
            Ok(())
        }
        ExprKind::Call {
            name,
            args,
            kwargs,
            param_args,
        } if name == "origin_of" && kwargs.is_empty() && param_args.is_empty() => {
            if args.is_empty() {
                return Err(TypeError::Unsupported(
                    "origin_of requires at least one parameter place".to_string(),
                ));
            }
            for argument in args {
                let Some((root, _)) = place_path(argument) else {
                    return Err(TypeError::Unsupported(
                        "origin_of requires parameter places".to_string(),
                    ));
                };
                if root != "self" && !value_params.contains(root) {
                    return Err(TypeError::UndefinedVariable(root.to_string()));
                }
            }
            Ok(())
        }
        ExprKind::Member { .. } | ExprKind::Index { .. } => {
            let Some((root, _)) = place_path(expr) else {
                return Err(TypeError::Unsupported("invalid origin place".to_string()));
            };
            if root == "self" || value_params.contains(root) {
                Ok(())
            } else {
                Err(TypeError::UndefinedVariable(root.to_string()))
            }
        }
        ExprKind::Identifier(name) => Err(TypeError::UndefinedVariable(name.clone())),
        _ => Err(TypeError::Unsupported(
            "origin clauses must name origins or parameter places".to_string(),
        )),
    }
}

fn lower_ref_param_sigs(
    type_params: &[crate::ast::TypeParam],
    params: &[&FnParam],
) -> Result<Vec<Option<crate::origin::RefSig>>, TypeError> {
    params
        .iter()
        .map(|param| {
            if param.convention != Some(ArgConvention::Ref) {
                return Ok(None);
            }
            match &param.origin {
                Some(spec) => lower_ref_sig(spec, type_params, params).map(Some),
                None => Ok(Some(crate::origin::RefSig {
                    origin: crate::origin::SigOrigin::Infer,
                    mutability: crate::origin::SigMutability::Infer,
                })),
            }
        })
        .collect()
}

fn lower_ref_sig(
    spec: &crate::ast::OriginSpec,
    type_params: &[crate::ast::TypeParam],
    params: &[&FnParam],
) -> Result<crate::origin::RefSig, TypeError> {
    use crate::origin::{RefSig, SigMutability, SigOrigin};
    let mut members = Vec::new();
    let mut mutability = SigMutability::Infer;
    for expression in spec {
        match &expression.kind {
            ExprKind::Identifier(name) if name == "_" => members.push(SigOrigin::Infer),
            ExprKind::Identifier(name) if name == "self" => members.push(SigOrigin::Self_),
            ExprKind::Identifier(name) if name == "StaticOrigin" => {
                members.push(SigOrigin::Static);
                mutability = SigMutability::Immutable;
            }
            ExprKind::Identifier(name) if name == "UntrackedOrigin" => {
                members.push(SigOrigin::Untracked { mutable: false });
                mutability = SigMutability::Immutable;
            }
            ExprKind::Identifier(name) if name == "UnsafeAnyOrigin" => {
                members.push(SigOrigin::Untracked { mutable: true });
                mutability = SigMutability::Mutable;
            }
            ExprKind::Identifier(name) => {
                if let Some(index) = params.iter().position(|param| param.name == *name) {
                    members.push(SigOrigin::Param(index));
                    continue;
                }
                let origin_param = type_params
                    .iter()
                    .find(|param| param.name == *name && param.bounds.as_slice() == ["Origin"])
                    .ok_or_else(|| TypeError::UndefinedVariable(name.clone()))?;
                mutability = match origin_param.origin_mutability.as_ref().map(|e| &e.kind) {
                    Some(ExprKind::Bool(true)) => SigMutability::Mutable,
                    Some(ExprKind::Bool(false)) => SigMutability::Immutable,
                    Some(ExprKind::Identifier(value)) => SigMutability::BoolParam(value.clone()),
                    _ => SigMutability::Infer,
                };
                for (index, param) in params.iter().enumerate() {
                    if param.origin.as_ref().is_some_and(|origin| {
                        matches!(origin.as_slice(), [Expr { kind: ExprKind::Identifier(bound), .. }] if bound == name)
                    }) {
                        members.push(SigOrigin::Param(index));
                    }
                }
            }
            ExprKind::Call { name, args, .. } if name == "origin_of" => {
                for argument in args {
                    let (root, path) = place_path(argument).ok_or_else(|| {
                        TypeError::Unsupported("origin_of requires parameter places".to_string())
                    })?;
                    let base = if root == "self" {
                        SigOrigin::Self_
                    } else {
                        let index = params
                            .iter()
                            .position(|param| param.name == root)
                            .ok_or_else(|| TypeError::UndefinedVariable(root.to_string()))?;
                        SigOrigin::Param(index)
                    };
                    members.push(project_sig_origin(base, &path));
                }
            }
            ExprKind::Member { .. } | ExprKind::Index { .. } => {
                let (root, path) = place_path(expression)
                    .ok_or_else(|| TypeError::Unsupported("invalid origin place".to_string()))?;
                let base = if root == "self" {
                    SigOrigin::Self_
                } else {
                    let index = params
                        .iter()
                        .position(|param| param.name == root)
                        .ok_or_else(|| TypeError::UndefinedVariable(root.to_string()))?;
                    SigOrigin::Param(index)
                };
                members.push(project_sig_origin(base, &path));
            }
            _ => {
                return Err(TypeError::Unsupported(
                    "unsupported origin contract".to_string(),
                ));
            }
        }
    }
    members.sort_by_key(|member| match member {
        SigOrigin::Self_ => 0,
        SigOrigin::Param(i) => i + 1,
        _ => usize::MAX,
    });
    members.dedup();
    let origin = match members.as_slice() {
        [] => SigOrigin::Infer,
        [single] => single.clone(),
        _ => SigOrigin::Union(members),
    };
    Ok(RefSig { origin, mutability })
}

fn project_sig_origin(
    base: crate::origin::SigOrigin,
    path: &[PlaceSeg],
) -> crate::origin::SigOrigin {
    crate::origin::SigOrigin::Projected(
        Box::new(base),
        path.iter()
            .map(|segment| match segment {
                PlaceSeg::Field(name) => crate::origin::OriginSeg::Field(name.clone()),
                PlaceSeg::Index => crate::origin::OriginSeg::AnyIndex,
            })
            .collect(),
    )
}

fn project_origin(
    origin: crate::origin::Origin,
    path: &[crate::origin::OriginSeg],
) -> crate::origin::Origin {
    use crate::origin::Origin;
    match origin {
        Origin::Place(mut place) => {
            place.path.extend_from_slice(path);
            Origin::Place(place)
        }
        Origin::Union(members) => Origin::union(
            members
                .into_iter()
                .map(|member| project_origin(member, path)),
        ),
        other => other,
    }
}

fn substitute_sig_origin(
    signature: &crate::origin::SigOrigin,
    actual: &[Option<crate::origin::Origin>],
) -> crate::origin::Origin {
    use crate::origin::{Origin, SigOrigin};
    match signature {
        SigOrigin::Self_ => Origin::Union(vec![]),
        SigOrigin::Param(index) => actual
            .get(*index)
            .and_then(Clone::clone)
            .unwrap_or(Origin::Union(vec![])),
        SigOrigin::Static => Origin::Static,
        SigOrigin::Untracked { mutable } => Origin::Untracked { mutable: *mutable },
        SigOrigin::Projected(base, path) => {
            project_origin(substitute_sig_origin(base, actual), path)
        }
        SigOrigin::Union(members) => Origin::union(
            members
                .iter()
                .map(|member| substitute_sig_origin(member, actual)),
        ),
        SigOrigin::Infer => Origin::union(actual.iter().filter_map(Clone::clone)),
    }
}

fn substitute_sig_origin_with_self(
    signature: &crate::origin::SigOrigin,
    actual: &[Option<crate::origin::Origin>],
    self_owner: Option<crate::origin::OwnerId>,
) -> crate::origin::Origin {
    use crate::origin::{Origin, OriginPlace, SigOrigin};
    match signature {
        SigOrigin::Self_ => self_owner
            .map(|root| Origin::Place(OriginPlace { root, path: vec![] }))
            .unwrap_or_else(|| Origin::Union(vec![])),
        SigOrigin::Union(members) => Origin::union(
            members
                .iter()
                .map(|member| substitute_sig_origin_with_self(member, actual, self_owner)),
        ),
        SigOrigin::Projected(base, path) => project_origin(
            substitute_sig_origin_with_self(base, actual, self_owner),
            path,
        ),
        _ => substitute_sig_origin(signature, actual),
    }
}

fn origin_is_within(actual: &crate::origin::Origin, allowed: &crate::origin::Origin) -> bool {
    use crate::origin::Origin;
    match actual {
        Origin::Union(members) => members
            .iter()
            .all(|member| origin_is_within(member, allowed)),
        _ => match allowed {
            Origin::Union(members) => members
                .iter()
                .any(|member| origin_is_within(actual, member)),
            _ => actual.overlaps(allowed),
        },
    }
}

fn ref_parameter_is_writable(parameter: &FnParam, type_params: &[crate::ast::TypeParam]) -> bool {
    if parameter.convention != Some(ArgConvention::Ref) {
        return parameter_is_writable(parameter.convention);
    }
    let Some(
        [
            Expr {
                kind: ExprKind::Identifier(origin_name),
                ..
            },
        ],
    ) = parameter.origin.as_deref()
    else {
        return true;
    };
    let Some(origin) = type_params.iter().find(|candidate| {
        candidate.name == *origin_name && candidate.bounds.as_slice() == ["Origin"]
    }) else {
        return true;
    };
    matches!(
        origin.origin_mutability.as_ref().map(|expr| &expr.kind),
        Some(ExprKind::Bool(true))
    )
}

/// The linker qualifies `from std.utils import Variant` declarations.  Keep the
/// intrinsic recognition narrow so an unrelated user type ending in `Variant`
/// does not silently acquire built-in semantics.
fn is_variant_name(name: &str) -> bool {
    matches!(
        name,
        "Variant" | "__module$std$utilsVariant" | "__module$std$utils$Variant"
    )
}

/// Whether control can leave the owned iterator before exhaustion. A `break`
/// belongs to the nearest loop, while return/raise escape through every nested
/// loop. This is used only when residual elements cannot be deleted implicitly.
fn block_can_escape_owned_iteration(statements: &[Stmt], nested_loops: usize) -> bool {
    statements.iter().any(|statement| match &statement.kind {
        StmtKind::Break => nested_loops == 0,
        StmtKind::Return(_) | StmtKind::Raise(_) => true,
        StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
            branches
                .iter()
                .any(|(_, body)| block_can_escape_owned_iteration(body, nested_loops))
                || orelse
                    .as_ref()
                    .is_some_and(|body| block_can_escape_owned_iteration(body, nested_loops))
        }
        StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
            block_can_escape_owned_iteration(body, nested_loops + 1)
                || orelse
                    .as_ref()
                    .is_some_and(|body| block_can_escape_owned_iteration(body, nested_loops))
        }
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            block_can_escape_owned_iteration(body, nested_loops)
                || except
                    .as_ref()
                    .is_some_and(|(_, body)| block_can_escape_owned_iteration(body, nested_loops))
                || orelse
                    .as_ref()
                    .is_some_and(|body| block_can_escape_owned_iteration(body, nested_loops))
                || finalbody
                    .as_ref()
                    .is_some_and(|body| block_can_escape_owned_iteration(body, nested_loops))
        }
        StmtKind::With { body, .. } => block_can_escape_owned_iteration(body, nested_loops),
        // Nested declarations do not execute as part of the loop body.
        StmtKind::Def { .. } | StmtKind::Struct { .. } | StmtKind::Trait { .. } => false,
        _ => false,
    })
}

/// A readable symbol for an infix operator, for error messages.
fn infix_symbol(op: InfixOp) -> &'static str {
    match op {
        InfixOp::Add => "+",
        InfixOp::Sub => "-",
        InfixOp::Mul => "*",
        InfixOp::Div => "/",
        InfixOp::FloorDiv => "//",
        InfixOp::Mod => "%",
        InfixOp::MatMul => "@",
        InfixOp::Shl => "<<",
        InfixOp::Shr => ">>",
        InfixOp::BitAnd => "&",
        InfixOp::BitOr => "|",
        InfixOp::BitXor => "^",
        InfixOp::Pow => "**",
        InfixOp::Eq => "==",
        InfixOp::Ne => "!=",
        InfixOp::Lt => "<",
        InfixOp::Gt => ">",
        InfixOp::Le => "<=",
        InfixOp::Ge => ">=",
        InfixOp::And => "and",
        InfixOp::Or => "or",
        InfixOp::In => "in",
        InfixOp::NotIn => "not in",
    }
}

/// A readable symbol for a prefix operator, for error messages.
fn prefix_symbol(op: PrefixOp) -> &'static str {
    match op {
        PrefixOp::Neg => "-",
        PrefixOp::Not => "not",
    }
}
