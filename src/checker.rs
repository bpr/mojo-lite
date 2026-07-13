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
use std::collections::HashMap;

use crate::ast::{
    ArgConvention, Dtype, Expr, ExprKind, FnParam, InfixOp, Method, PrefixOp, Stmt, StmtKind,
    StructComptime, TraitComptime, Type as SourceType,
};
use crate::call::{
    ArgSlot, CallVariadics, MatchError, effective_keyword_only_index, match_call_slots,
    regular_marker_index,
};
use crate::ct::CtValue;
use crate::error::TypeError;
use crate::token::{SourceSpan, Span};
use crate::types::{ParamDecl, Ty, TyArg};

/// The checked signature of a struct, kept in the checker's registry.
struct StructInfo {
    /// Compile-time parameters (type and value); empty for a non-generic struct.
    decls: Vec<ParamDecl>,
    /// Traits this struct declares conformance to (verified at definition).
    conforms: Vec<String>,
    /// Declared fields, in order (drives the fieldwise constructor).
    fields: Vec<(String, Ty)>,
    /// Associated compile-time facts declared by `comptime NAME = ...` in the
    /// struct body. These live on the type, not on runtime instances.
    associated: HashMap<String, CtValue>,
    methods: HashMap<String, Vec<MethodSig>>,
    fieldwise_init: bool,
}

/// The source-level pieces of a struct declaration passed through checking.
struct StructDeclaration<'a> {
    module: &'a Option<String>,
    span: Span,
    name: &'a str,
    type_params: &'a [crate::ast::TypeParam],
    conforms: &'a [String],
    fields: &'a [crate::ast::Param],
    associated: &'a [StructComptime],
    methods: &'a [Method],
    fieldwise_init: bool,
}

/// The checked signature of a trait: required methods plus associated
/// compile-time facts. A method requirement's signature may mention
/// `Ty::SelfType` (the conforming type).
struct TraitInfo {
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

#[derive(Clone, PartialEq)]
struct MethodSig {
    /// Regular parameters only; variadic element type is stored separately.
    params: Vec<Ty>,
    names: Vec<String>,
    required: Vec<bool>,
    variadic: Option<Box<Ty>>,
    variadic_index: Option<usize>,
    positional_only: Option<usize>,
    keyword_only: Option<usize>,
    conventions: Vec<Option<ArgConvention>>,
    ret: Ty,
    /// Receiver convention. `None` means plain read-only `self`; explicit
    /// conventions (`mut`, `owned`, `ref`, ...) are preserved so trait
    /// requirements can compare them exactly. Today only `mut self` changes call
    /// checking behavior.
    self_convention: Option<crate::ast::ArgConvention>,
}

impl MethodSig {
    fn intrinsic(params: Vec<Ty>, ret: Ty) -> MethodSig {
        let len = params.len();
        MethodSig {
            params,
            names: (0..len).map(|i| format!("arg{i}")).collect(),
            required: vec![true; len],
            variadic: None,
            variadic_index: None,
            positional_only: None,
            keyword_only: None,
            conventions: vec![None; len],
            ret,
            self_convention: None,
        }
    }
}

fn fixed_callable_arity(ty: &Ty) -> Option<usize> {
    match ty {
        Ty::Func {
            params,
            required,
            variadic: None,
            ..
        } if required.iter().all(|r| *r) => Some(params.len()),
        Ty::GenericFunc { params, .. } => Some(params.len()),
        _ => None,
    }
}

fn method_arity_range(sig: &MethodSig) -> (usize, usize) {
    (sig.params.len(), sig.params.len())
}

fn same_method_shape(a: &MethodSig, b: &MethodSig) -> bool {
    method_arity_range(a) == method_arity_range(b) && a.params == b.params
}

fn same_callable_signature(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        (Ty::Func { params: ap, .. }, Ty::Func { params: bp, .. }) => ap == bp,
        (
            Ty::GenericFunc {
                decls: ad,
                params: ap,
                ..
            },
            Ty::GenericFunc {
                decls: bd,
                params: bp,
                ..
            },
        ) => canonical_generic_signature(ad, ap) == canonical_generic_signature(bd, bp),
        _ => false,
    }
}

fn canonical_generic_signature(decls: &[ParamDecl], params: &[Ty]) -> (Vec<ParamDecl>, Vec<Ty>) {
    let mut subst = HashMap::new();
    let canonical_decls = decls
        .iter()
        .enumerate()
        .map(|(index, decl)| match decl {
            ParamDecl::Type { name, bounds } => {
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
                }
            }
            ParamDecl::Value { .. } => ParamDecl::Value {
                name: format!("${index}"),
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
    let params = match ty {
        Ty::Func { params, .. } | Ty::GenericFunc { params, .. } => params,
        _ => return None,
    };
    Some(crate::symbol::function_symbol(
        name,
        &crate::symbol::SignatureKey::from_tys(params),
    ))
}

/// The lowered symbol of an overloaded method/constructor resolution, likewise
/// canonical (`sig.params` are the declared parameter types, unsubstituted —
/// matching the MIR definition side, which mangles the declared annotations).
fn method_lowered_name(type_name: &str, method: &str, sig: &MethodSig) -> String {
    crate::symbol::method_symbol(
        type_name,
        method,
        &crate::symbol::SignatureKey::from_tys(&sig.params),
    )
}

enum OverloadSelect {
    NoMatch,
    Ambiguous,
}

/// A concrete method candidate after receiver-type substitution and argument
/// scoring. Named fields keep overload resolution readable as it evolves.
struct MethodCallResolution {
    conversion_score: usize,
    slots: Vec<ArgSlot>,
    conventions: Vec<Option<ArgConvention>>,
    return_type: Ty,
    mutates_receiver: bool,
    lowered_name: Option<String>,
}

fn select_callable_overload(
    matches: Vec<(Ty, usize, String)>,
) -> Result<(Ty, String), OverloadSelect> {
    let best = matches
        .iter()
        .map(|(_, score, _)| *score)
        .min()
        .ok_or(OverloadSelect::NoMatch)?;
    let mut best_matches = matches
        .into_iter()
        .filter(|(_, score, _)| *score == best)
        .collect::<Vec<_>>();
    if best_matches.len() != 1 {
        return Err(OverloadSelect::Ambiguous);
    }
    let (ret, _, target) = best_matches.remove(0);
    Ok((ret, target))
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
    fixed_callable_arity(new_ty)?;
    match existing {
        Ty::Func { .. } | Ty::GenericFunc { .. } if fixed_callable_arity(existing).is_some() => {
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
    let mut checker = Checker::new();
    checker.check_program(stmts)?;
    Ok(crate::checked::CheckedProgram::new(
        stmts.to_vec(),
        checker.overload_targets.into_inner(),
        checker.declaration_types.into_inner(),
    ))
}

/// Type-check a program and return the concrete lowered callee chosen for every
/// overloaded call site. MIR lowering uses this side table so source calls like
/// `f(x)` can lower to a signature-specific function even when overloads share
/// the same arity.
pub fn resolve_overload_targets(stmts: &[Stmt]) -> Result<HashMap<SourceSpan, String>, TypeError> {
    Ok(check_program(stmts)?.overload_targets().clone())
}

/// A single-pass static type checker over the parsed AST.
pub struct Checker {
    /// Lexical scope chain, innermost last. Starts with the global scope.
    scopes: Vec<HashMap<String, Ty>>,
    /// Binding mutability, parallel to `scopes`. `var` locals are writable;
    /// ordinary function parameters are not.
    mutable_scopes: Vec<HashMap<String, bool>>,
    /// Index of the local scope for each function currently being checked.
    function_bases: Vec<usize>,
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
    /// Source-span to lowered callee for calls whose source name denotes an
    /// overload set. Interior mutability keeps expression inference usable from
    /// read-only helper methods while still recording resolution facts.
    overload_targets: RefCell<HashMap<SourceSpan, String>>,
    /// Free-function `**kwargs` collectors, keyed by source function name. The
    /// stored type is the homogeneous value element type.
    kw_collectors: RefCell<HashMap<String, Ty>>,
    declaration_types: RefCell<HashMap<crate::checked::AnnotationSite, Ty>>,
}

impl Checker {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
            mutable_scopes: vec![HashMap::new()],
            function_bases: Vec::new(),
            structs: HashMap::new(),
            traits: HashMap::new(),
            tparams: Vec::new(),
            self_decls: Vec::new(),
            self_ty: None,
            trait_self_comptime: Vec::new(),
            comptimes: HashMap::new(),
            self_mutable: false,
            overload_targets: RefCell::new(HashMap::new()),
            kw_collectors: RefCell::new(HashMap::new()),
            declaration_types: RefCell::new(HashMap::new()),
        }
    }

    /// The type denoted by a source annotation; resolves type parameters and
    /// validates struct names and type-argument counts.
    fn ty_from_anno(&self, ty: &SourceType) -> Result<Ty, TypeError> {
        self.resolve_ty_from_anno(ty)
    }

    fn resolve_ty_from_anno(&self, ty: &SourceType) -> Result<Ty, TypeError> {
        Ok(match ty {
            SourceType::Int => Ty::Int,
            SourceType::UInt => Ty::UInt,
            SourceType::Bool => Ty::Bool,
            SourceType::String => Ty::String,
            SourceType::Float64 => Ty::Float64,
            SourceType::None => Ty::None,
            // Function-type annotations parse but their semantics are deferred: the
            // checker's `Ty::Func` only ever arises from a `def` (with names/arity),
            // so a function-typed binding is not modeled yet.
            SourceType::Func { .. } => {
                return Err(TypeError::Unsupported(
                    "function type annotation".to_string(),
                ));
            }
            // A `ref [origin] T` reference type parses (its origin discarded) but
            // reference semantics / origins are not modeled.
            SourceType::Ref(_) => {
                return Err(TypeError::Unsupported("reference type".to_string()));
            }
            // A bare name may be an in-scope type parameter (a generic `def`'s
            // `T`) or a struct type, optionally applied to parameter arguments.
            SourceType::Named(name, args) => {
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
                if name == "Error" && args.is_empty() {
                    return Ok(Ty::Error);
                }
                if let Some(info) = self.structs.get(name) {
                    let decls = info.decls.clone();
                    if args.len() != decls.len() {
                        return Err(TypeError::WrongTypeArgCount {
                            name: name.clone(),
                            expected: decls.len(),
                            got: args.len(),
                        });
                    }
                    let tyargs = decls
                        .iter()
                        .zip(args)
                        .map(|(decl, arg)| self.resolve_param_arg(decl, arg))
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok(Ty::Struct(name.clone(), tyargs));
                }
                if name == "List" {
                    return self.list_type(args);
                }
                if name == "Tuple" {
                    return self.tuple_type(args);
                }
                if name == "UnsafePointer" {
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
            Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| self.resolve_assoc_ty(t)).collect()),
            Ty::Pointer(elem) => Ty::Pointer(Box::new(self.resolve_assoc_ty(elem))),
            Ty::Func {
                params,
                names,
                ret,
                required,
                variadic,
                positional_only,
                keyword_only,
                conventions,
            } => Ty::Func {
                params: params.iter().map(|p| self.resolve_assoc_ty(p)).collect(),
                names: names.clone(),
                ret: Box::new(self.resolve_assoc_ty(ret)),
                required: required.clone(),
                variadic: variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                positional_only: *positional_only,
                keyword_only: *keyword_only,
                conventions: conventions.clone(),
            },
            Ty::GenericFunc {
                decls,
                params,
                names,
                ret,
                required,
                variadic,
                positional_only,
                keyword_only,
                conventions,
            } => Ty::GenericFunc {
                decls: decls.clone(),
                params: params.iter().map(|p| self.resolve_assoc_ty(p)).collect(),
                names: names.clone(),
                ret: Box::new(self.resolve_assoc_ty(ret)),
                required: required.clone(),
                variadic: variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                positional_only: *positional_only,
                keyword_only: *keyword_only,
                conventions: conventions.clone(),
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
            ParamDecl::Type { name, bounds } => {
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
            ParamDecl::Value { name } => match arg {
                ParamArg::Value(expr) => Ok(TyArg::Val(CtValue::Int(self.eval_ct(expr)?))),
                ParamArg::Type(_) => Err(TypeError::TypeMismatch {
                    expected: "a value".to_string(),
                    found: "a type".to_string(),
                    context: format!("value parameter '{}'", name),
                }),
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
        }
    }

    /// Resolve `UnsafePointer[T]` from its single type argument.
    fn pointer_type(&self, args: &[crate::ast::ParamArg]) -> Result<Ty, TypeError> {
        if args.len() != 1 {
            return Err(TypeError::WrongTypeArgCount {
                name: "UnsafePointer".to_string(),
                expected: 1,
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
        };
        Ok(Ty::Pointer(Box::new(elem)))
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
            });
        }
        Ok(Ty::Tuple(elems))
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
        let width = self.simd_width(&args[1])?;
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
            ParamDecl::Type { name: n, bounds } if n == name => {
                Some(CtValue::Type(Box::new(Ty::Param {
                    name: n.clone(),
                    bounds: bounds.clone(),
                })))
            }
            ParamDecl::Value { name: n } if n == name => Some(CtValue::Param(n.clone())),
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
            StmtKind::RefDecl { .. } => Err(TypeError::Unsupported(
                "reference binding and origin semantics".to_string(),
            )),
            StmtKind::VarDecl { name, ty, value } => {
                let found = self.infer(value)?;
                self.check_consuming(value, &found, &format!("variable '{name}'"))?;
                let declared = match ty {
                    // Annotated: the value must coerce to the annotation.
                    Some(anno) => {
                        let expected = self.ty_from_anno(anno)?;
                        if !coerces(&found, &expected) {
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
                self.declare(name, declared)
            }

            StmtKind::Assign { name, value } => {
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
                        // Assigning a closure could move it to an outer binding.
                        if matches!(
                            found,
                            Ty::Func { .. } | Ty::GenericFunc { .. } | Ty::Overload(_)
                        ) {
                            return Err(TypeError::ClosureEscape);
                        }
                        if !coerces(&found, &target) {
                            return Err(TypeError::TypeMismatch {
                                expected: target.to_string(),
                                found: found.to_string(),
                                context: format!("assignment to '{}'", name),
                            });
                        }
                        Ok(())
                    }
                    // `x = e` on an undeclared name is a **var-less introduction**
                    // (implicit declaration). Mojo allows it; mojito parses and
                    // type-checks it by binding the materialized type. Later
                    // lowering retains the explicit unsupported boundary.
                    None => {
                        let declared = self.inferred_binding_ty(&found, name)?;
                        self.declare(name, declared)
                    }
                }
            }

            StmtKind::AugAssign { place, op, value } => {
                // `target OP= value` means `target = target OP value`: the place
                // must be writable, and the result of the operator must keep the
                // place's type. Typing `place OP value` reuses `infer_infix`.
                let target = self.check_place(place)?;
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

            // Tuple unpacking `a, b = t` is parsed and grammar-documented, but its
            // semantics (splitting a tuple across targets) are deferred — flagged
            // here, consistent with the other syntax-first parse-only constructs.
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
                // The place must be a writable location (a field/index chain
                // rooted at a mutable variable or `mut self`); the value must
                // keep the place's type. A width-1 SIMD target (a lane write, or
                // a scalar-alias field) additionally accepts a splatting literal.
                let target = self.check_place(place)?;
                let found = self.infer(value)?;
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
                ret: ret_anno,
                body,
                raises: _,
                decorators: _,
            } => {
                if self.structs.contains_key(name) {
                    return Err(TypeError::Redeclaration(name.clone()));
                }
                // `**kwargs` and out-parameter conventions remain outside the
                // supported subset. Other argument forms share one binder for
                // generic and non-generic functions.
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
                if kw_variadic_idx.is_some() && !type_params.is_empty() {
                    return Err(TypeError::Unsupported(
                        "generic functions with **kwargs".to_string(),
                    ));
                }
                // Regular (non-variadic) parameters, over which arity is computed.
                let regular: Vec<&crate::ast::FnParam> = params
                    .iter()
                    .filter(|p| p.kind == crate::ast::ParamKind::Regular)
                    .collect();
                let pos_only = regular_marker_index(params, *positional_only);
                let kw_only = effective_keyword_only_index(params, *keyword_only, variadic_idx);
                let required = required_mask(&regular, kw_only)?;
                let decls = self.classify_params(type_params)?;
                // Type parameters are in scope while resolving the signature and
                // checking the body (as bare `T`).
                self.tparams.push(type_scope(&decls));

                let signature = (|| {
                    let param_tys = self.param_tys(params)?;
                    let ret_ty = match ret_anno {
                        Some(t) => self.ty_from_anno(t)?,
                        None => Ty::None,
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
                if let Some(index) = kw_variadic_idx {
                    self.kw_collectors
                        .borrow_mut()
                        .insert(name.clone(), param_tys[index].clone());
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
                let fn_ty = if decls.is_empty() {
                    let regular_tys: Vec<Ty> = params
                        .iter()
                        .zip(&param_tys)
                        .filter(|(p, _)| p.kind == crate::ast::ParamKind::Regular)
                        .map(|(_, ty)| ty.clone())
                        .collect();
                    Ty::Func {
                        params: regular_tys,
                        names: regular.iter().map(|p| p.name.clone()).collect(),
                        ret: Box::new(ret_ty.clone()),
                        required,
                        variadic: variadic_idx.map(|vi| Box::new(param_tys[vi].clone())),
                        positional_only: pos_only,
                        keyword_only: kw_only,
                        conventions: regular.iter().map(|p| p.convention).collect(),
                    }
                } else {
                    let regular_tys: Vec<Ty> = params
                        .iter()
                        .zip(&param_tys)
                        .filter(|(p, _)| p.kind == crate::ast::ParamKind::Regular)
                        .map(|(_, ty)| ty.clone())
                        .collect();
                    Ty::GenericFunc {
                        decls: decls.clone(),
                        params: regular_tys,
                        names: regular.iter().map(|p| p.name.clone()).collect(),
                        ret: Box::new(ret_ty.clone()),
                        required,
                        variadic: variadic_idx.map(|vi| Box::new(param_tys[vi].clone())),
                        positional_only: pos_only,
                        keyword_only: kw_only,
                        conventions: regular.iter().map(|p| p.convention).collect(),
                    }
                };
                if let Err(e) = self.declare(name, fn_ty) {
                    self.tparams.pop();
                    return Err(e);
                }

                self.push_scope();
                self.function_bases.push(self.scopes.len() - 1);
                let mut result = Ok(());
                // Value parameters are ordinary `Int` locals in the body.
                for d in &decls {
                    if let ParamDecl::Value { name } = d {
                        result = self.declare_immutable(name, Ty::Int);
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
                            crate::ast::ParamKind::Variadic => Ty::List(Box::new(ty.clone())),
                            crate::ast::ParamKind::KwVariadic => Ty::Struct(
                                "HashDict".to_string(),
                                vec![TyArg::Ty(Ty::String), TyArg::Ty(ty.clone())],
                            ),
                            crate::ast::ParamKind::Regular => ty.clone(),
                        };
                        // Duplicate parameter names are a redeclaration.
                        result = self.declare_with_mutability(
                            &param.name,
                            bind_ty,
                            param.kind == crate::ast::ParamKind::KwVariadic
                                || parameter_is_writable(param.convention),
                        );
                        if result.is_err() {
                            break;
                        }
                    }
                }
                // A function body is a fresh loop context: `break`/`continue`
                // do not cross into a nested `def`.
                if result.is_ok() {
                    result = self.check_block(body, Some(&ret_ty), false);
                }
                // A function with a non-`None` return type must return on every
                // path (falling off the end would yield `None`).
                if result.is_ok() && ret_ty != Ty::None && !definitely_returns(body) {
                    result = Err(TypeError::MissingReturn(name.clone()));
                }
                self.pop_scope();
                self.function_bases.pop();
                self.tparams.pop();
                result
            }

            StmtKind::Struct {
                name,
                type_params,
                conforms,
                fields,
                associated,
                methods,
                fieldwise_init,
                decorators: _,
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
                    fields,
                    associated,
                    methods,
                    fieldwise_init: *fieldwise_init,
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
                for (cond, body) in branches {
                    self.expect_bool(cond, "if condition")?;
                    self.check_scoped_block(body, ret, in_loop)?;
                }
                if let Some(body) = orelse {
                    self.check_scoped_block(body, ret, in_loop)?;
                }
                Ok(())
            }

            StmtKind::While { cond, body } => {
                self.expect_bool(cond, "while condition")?;
                self.check_scoped_block(body, ret, true)
            }

            // `raise` — its operand must be an `Error` (or a `String`, the
            // shorthand). The raises *effect* (that this must be in a `raises`
            // function or a `try`) is deliberately not analyzed.
            StmtKind::Raise(expr) => {
                let ty = self.infer(expr)?;
                if ty != Ty::Error && ty != Ty::String {
                    return Err(TypeError::TypeMismatch {
                        expected: "Error".to_string(),
                        found: ty.to_string(),
                        context: "raise".to_string(),
                    });
                }
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
                self.check_scoped_block(body, ret, in_loop)?;
                if let Some((name, ex_body)) = except {
                    self.push_scope();
                    // `except e:` binds the caught error as an `Error`.
                    let result = match name {
                        Some(n) => self
                            .declare(n, Ty::Error)
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

            StmtKind::For { var, iter, body } => {
                // The loop variable's type comes from the iterable: `Int` for a
                // `range`, the element type for a `List`, or — for a user struct —
                // the element type of its `__iter__()` iterator (`__next__`'s return).
                let iter_ty = self.infer(iter)?;
                let elem_ty = self.iterable_element_ty(&iter_ty)?;
                self.push_scope();
                let result = match self.declare(var, elem_ty) {
                    Ok(()) => self.check_block(body, ret, true),
                    Err(e) => Err(e),
                };
                self.pop_scope();
                result
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
                let expected = match ret {
                    Some(ty) => ty,
                    None => return Err(TypeError::ReturnOutsideFunction),
                };
                let found = match expr {
                    Some(e) => self.infer(e)?,
                    None => Ty::None,
                };
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
                if !coerces(&found, expected) {
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
                self.infer(expr)?;
                Ok(())
            }
        }
    }

    /// Resolve a parameter/field list to its types.
    fn param_tys(&self, params: &[crate::ast::FnParam]) -> Result<Vec<Ty>, TypeError> {
        params.iter().map(|p| self.ty_from_anno(&p.ty)).collect()
    }

    fn method_sig(&self, method: &Method, all_types: &[Ty]) -> Result<MethodSig, TypeError> {
        let variadic_idx = method
            .params
            .iter()
            .position(|p| p.kind == crate::ast::ParamKind::Variadic);
        let regular: Vec<_> = method
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == crate::ast::ParamKind::Regular)
            .collect();
        let keyword_only =
            effective_keyword_only_index(&method.params, method.keyword_only, variadic_idx);
        Ok(MethodSig {
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
            positional_only: regular_marker_index(&method.params, method.positional_only),
            keyword_only,
            conventions: regular.iter().map(|(_, p)| p.convention).collect(),
            ret: match &method.ret {
                Some(ret) => self.ty_from_anno(ret)?,
                None => Ty::None,
            },
            self_convention: method.self_convention,
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
        use crate::ast::{ArgConvention, ParamKind};
        if flag_defaults && params.iter().any(|p| p.default.is_some()) {
            return Some("default argument values");
        }
        if flag_variadic && params.iter().any(|p| p.kind == ParamKind::Variadic) {
            return Some("variadic '*args' parameters");
        }
        if flag_kw_variadic && params.iter().any(|p| p.kind == ParamKind::KwVariadic) {
            return Some("variadic '**kwargs' parameters");
        }
        // `owned`/`read` bind by value; `mut`/`ref` are references whose mutations
        // are written back to the caller (modeled — see `eval_call`). Only `out`
        // (an uninitialized out-parameter, i.e. a hand-written `__init__`) still
        // has semantics we don't model.
        if params
            .iter()
            .any(|p| matches!(p.convention, Some(ArgConvention::Out)))
        {
            return Some("the 'out' argument convention");
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
            // A lone bound that names a scalar type marks a value parameter.
            if let [only] = tp.bounds.as_slice()
                && let Some(vty) = scalar_type_name(only)
            {
                if vty != Ty::Int {
                    return Err(TypeError::BadValueParamType {
                        name: tp.name.clone(),
                        ty: only.clone(),
                    });
                }
                decls.push(ParamDecl::Value {
                    name: tp.name.clone(),
                });
                continue;
            }
            for bound in &tp.bounds {
                self.check_trait_name(bound)?;
            }
            decls.push(ParamDecl::Type {
                name: tp.name.clone(),
                bounds: tp.bounds.clone(),
            });
        }
        Ok(decls)
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
        // The remaining parse-only trait extensions are flagged here; method
        // requirements and comptime associated-member requirements are modeled.
        if !refines.is_empty() {
            return Err(TypeError::Unsupported("trait inheritance".to_string()));
        }
        if methods.iter().any(|m| m.default_body.is_some()) {
            return Err(TypeError::Unsupported("trait default method".to_string()));
        }
        let mut ct_members = HashMap::new();
        for member in comptime_members {
            if ct_members.contains_key(&member.name) {
                return Err(TypeError::Redeclaration(member.name.clone()));
            }
            ct_members.insert(
                member.name.clone(),
                self.ct_member_req_from_anno(&member.ty)?,
            );
        }
        // Requirement signatures resolve `Self` to the abstract `Ty::SelfType`.
        let saved_self_ty = self.self_ty.replace(Ty::SelfType);
        let saved_self_decls = std::mem::take(&mut self.self_decls);
        self.trait_self_comptime.push(ct_members.clone());
        let result = (|| {
            let mut sigs: HashMap<String, Vec<MethodSig>> = HashMap::new();
            for m in methods {
                if ct_members.contains_key(&m.name) {
                    return Err(TypeError::Redeclaration(m.name.clone()));
                }
                if let Some(feature) = Self::advanced_param_feature(
                    &m.params,
                    m.positional_only,
                    m.keyword_only,
                    true,
                    true,
                    true,
                ) {
                    return Err(TypeError::Unsupported(feature.to_string()));
                }
                if m.positional_only.is_some() || m.keyword_only.is_some() {
                    return Err(TypeError::Unsupported(
                        "positional-only/keyword-only markers on trait methods".to_string(),
                    ));
                }
                let sig = MethodSig {
                    params: self.param_tys(&m.params)?,
                    names: m.params.iter().map(|p| p.name.clone()).collect(),
                    required: vec![true; m.params.len()],
                    variadic: None,
                    variadic_index: None,
                    positional_only: m.positional_only,
                    keyword_only: m.keyword_only,
                    conventions: m.params.iter().map(|p| p.convention).collect(),
                    ret: match &m.ret {
                        Some(t) => self.ty_from_anno(t)?,
                        None => Ty::None,
                    },
                    self_convention: m.self_convention,
                };
                let overloads = sigs.entry(m.name.clone()).or_default();
                if overloads
                    .iter()
                    .any(|existing| same_method_shape(existing, &sig))
                {
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
        let saved_self_ty = self.self_ty.replace(self_ty.clone());
        let result = self.check_struct_members(declaration, decls, &self_ty);
        self.self_decls = saved_self_decls;
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
        // Field types are resolved against structs defined *so far* (so a struct
        // can't contain itself); duplicate field names are a redeclaration.
        let mut field_tys: Vec<(String, Ty)> = Vec::new();
        for (field_index, f) in fields.iter().enumerate() {
            if field_tys.iter().any(|(n, _)| n == &f.name) {
                return Err(TypeError::Redeclaration(f.name.clone()));
            }
            let ty = self.ty_from_anno(&f.ty)?;
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
        // Register the (method-less) struct first, so methods may reference the
        // struct's own type (even parameterized, `Pair[Self.T]`) in signatures.
        self.structs.insert(
            name.to_string(),
            StructInfo {
                decls,
                conforms: conforms.to_vec(),
                fields: field_tys,
                associated: associated_values,
                methods: HashMap::new(),
                fieldwise_init,
            },
        );
        // Method signatures.
        for (method_index, m) in methods.iter().enumerate() {
            let method_name = lifecycle_method_name(m);
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
            let sig = self.method_sig(m, &all_types)?;
            let info = self.structs.get_mut(name).ok_or_else(|| {
                TypeError::InvariantViolation(format!("struct '{name}' was not registered"))
            })?;
            let overloads = info.methods.entry(method_name.to_string()).or_default();
            if overloads
                .iter()
                .any(|existing| same_method_shape(existing, &sig))
            {
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
                let want = MethodSig {
                    params: req_sig
                        .params
                        .iter()
                        .map(|t| self.resolve_assoc_ty(&substitute_self(t, self_ty)))
                        .collect(),
                    names: req_sig.names.clone(),
                    required: req_sig.required.clone(),
                    variadic: req_sig
                        .variadic
                        .as_ref()
                        .map(|ty| Box::new(self.resolve_assoc_ty(&substitute_self(ty, self_ty)))),
                    variadic_index: req_sig.variadic_index,
                    positional_only: req_sig.positional_only,
                    keyword_only: req_sig.keyword_only,
                    conventions: req_sig.conventions.clone(),
                    ret: self.resolve_assoc_ty(&substitute_self(&req_sig.ret, self_ty)),
                    self_convention: req_sig.self_convention,
                };
                if !got_sigs.contains(&want) {
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
            "ImplicitlyDeletable" => self.struct_implicitly_deletable_conformance_ok(name),
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
            CtValue::Type(_) => {
                let _ = self_ty;
                None
            }
        }
    }

    fn check_method(&mut self, self_ty: &Ty, m: &Method) -> Result<(), TypeError> {
        if !is_mojo_copy_constructor(m)
            && let Some(feature) = Self::advanced_param_feature(
                &m.params,
                m.positional_only,
                m.keyword_only,
                false,
                false,
                true,
            )
        {
            return Err(TypeError::Unsupported(feature.to_string()));
        }
        // A `@staticmethod` (no `self`) is parsed but its semantics are deferred.
        if !m.has_self {
            return Err(TypeError::Unsupported(
                "a method without a 'self' receiver (@staticmethod)".to_string(),
            ));
        }
        // `out self` initializes the receiver: it is allowed on the **`__init__`**
        // lifecycle method (a hand-written constructor), where `self`'s fields are
        // assigned in the body. `ref self` (parametric-mutability references), and
        // `out self` on any other method, still need semantics we don't model, so
        // they stay flagged. A plain `self`, `read self`, `mut self`, or **`owned
        // self`** (a consuming method — notably `__del__(owned self)`) is fine.
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
        if matches!(m.self_convention, Some(crate::ast::ArgConvention::Ref))
            || (matches!(m.self_convention, Some(crate::ast::ArgConvention::Out)) && !out_init)
        {
            return Err(TypeError::Unsupported(
                "'out self' / 'ref self' receiver".to_string(),
            ));
        }
        let ret_ty = match &m.ret {
            Some(t) => self.ty_from_anno(t)?,
            None => Ty::None,
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
        let mut result = self.bind_and_check_method(self_ty, m, &ret_ty);
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
        let mut assigned = std::collections::HashSet::new();
        collect_self_assigned_fields(body, &mut assigned);
        let info = self.structs.get(sname).ok_or_else(|| {
            TypeError::InvariantViolation(format!("struct '{sname}' was not registered"))
        })?;
        for (field, _) in &info.fields {
            if !assigned.contains(field.as_str()) {
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
    ) -> Result<(), TypeError> {
        let self_writable = matches!(
            m.self_convention,
            Some(crate::ast::ArgConvention::Mut | crate::ast::ArgConvention::Out)
        );
        self.declare_with_mutability("self", self_ty.clone(), self_writable)?;
        for p in &m.params {
            let mut pty = self.ty_from_anno(&p.ty)?;
            if p.kind == crate::ast::ParamKind::Variadic {
                pty = Ty::List(Box::new(pty));
            }
            self.declare_with_mutability(&p.name, pty, parameter_is_writable(p.convention))?;
        }
        // `self` is writable in a `mut self` method, or an `out self` `__init__`
        // (which assigns its fields). Restored after the body.
        let saved = std::mem::replace(&mut self.self_mutable, self_writable);
        let result = self.check_block(&m.body, Some(ret_ty), false);
        self.self_mutable = saved;
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
        if !kwargs.is_empty() {
            if args.is_empty() && kwargs.len() == 1 && kwargs[0].name == "copy" {
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
            return Err(TypeError::BadCall {
                func: name.to_string(),
                reason: "keyword arguments are not supported here".to_string(),
            });
        }
        // A hand-written `def __init__(out self, …)` is the constructor: check the
        // call arguments against its parameters (the `self` receiver is implicit).
        // Takes precedence over `@fieldwise_init`. On a **generic** struct, the type
        // parameters are solved by unifying `__init__`'s parameter types against the
        // argument types — exactly as the fieldwise path unifies field types.
        if let Some(sigs) = info.methods.get("__init__") {
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
            .map(|a| self.infer(a))
            .collect::<Result<Vec<_>, _>>()?;
        let (subst, tyargs) =
            self.resolve_use_params(name, &decls, param_args, &field_tys, &arg_tys)?;
        for (i, (aty, fty)) in arg_tys.iter().zip(&field_tys).enumerate() {
            let expected = substitute(fty, &subst);
            if !coerces(aty, &expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: aty.to_string(),
                    context: format!("field {} of '{}'", i + 1, name),
                });
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
            return Ok((subst, Vec::new()));
        }
        if !param_args.is_empty() {
            if param_args.len() != decls.len() {
                return Err(TypeError::WrongTypeArgCount {
                    name: name.to_string(),
                    expected: decls.len(),
                    got: param_args.len(),
                });
            }
            let mut tyargs = Vec::with_capacity(decls.len());
            for (decl, arg) in decls.iter().zip(param_args) {
                let tyarg = self.resolve_param_arg(decl, arg)?;
                if let (ParamDecl::Type { name, .. }, TyArg::Ty(t)) = (decl, &tyarg) {
                    subst.insert(name.clone(), t.clone());
                }
                tyargs.push(tyarg);
            }
            return Ok((subst, tyargs));
        }
        // Inference: only type parameters, solved from the argument types.
        for (pat, act) in patterns.iter().zip(actuals) {
            unify(pat, act, &mut subst)?;
        }
        let mut tyargs = Vec::with_capacity(decls.len());
        for decl in decls {
            match decl {
                ParamDecl::Value { name: pname } => {
                    return Err(TypeError::CannotInferTypeParam {
                        name: name.to_string(),
                        param: pname.clone(),
                    });
                }
                ParamDecl::Type {
                    name: pname,
                    bounds,
                } => {
                    let solved =
                        subst
                            .get(pname)
                            .ok_or_else(|| TypeError::CannotInferTypeParam {
                                name: name.to_string(),
                                param: pname.clone(),
                            })?;
                    for bound in bounds {
                        if !self.conforms_to(solved, bound) {
                            return Err(TypeError::TraitNotSatisfied {
                                param: pname.clone(),
                                ty: solved.to_string(),
                                trait_name: bound.clone(),
                                reason: self.trait_failure_reason(solved, bound),
                            });
                        }
                    }
                    tyargs.push(TyArg::Ty(solved.clone()));
                }
            }
        }
        Ok((subst, tyargs))
    }

    /// Whether `ty` conforms to trait `tr`. Lifecycle marker built-ins are tied
    /// to observable ownership behavior; other built-ins remain recognized but
    /// shallow unless their feature has a dedicated checker path. A user trait is
    /// satisfied nominally: a struct must *declare* conformance, and a type
    /// parameter must carry `tr` among its bounds (so a bounded `T` can be
    /// forwarded to another `[U: tr]` parameter).
    fn conforms_to(&self, ty: &Ty, tr: &str) -> bool {
        if BUILTIN_TRAITS.contains(&tr) {
            return match tr {
                "AnyType" => true,
                "Copyable" => self.is_copyable(ty),
                "ImplicitlyCopyable" => self.is_implicitly_copyable(ty),
                "Movable" => self.is_movable(ty),
                "ImplicitlyDeletable" => self.is_implicitly_deletable(ty),
                "Hashable" => self.is_hashable(ty),
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
            Ty::Struct(name, _) => self
                .structs
                .get(name)
                .is_some_and(|info| info.conforms.iter().any(|t| t == tr)),
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == tr),
            _ => false,
        }
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

    /// `ImplicitlyCopyable` is stronger than `Copyable`: it means the type can be
    /// copied by the ordinary implicit copy path, not only by an explicit custom
    /// copy constructor. Structs opt in by declaring the marker, and fieldwise
    /// conformance requires all fields to be implicitly copyable.
    fn is_implicitly_copyable(&self, ty: &Ty) -> bool {
        match ty {
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
            Ty::Struct(name, _) => self.struct_implicitly_deletable_conformance_ok(name),
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == "ImplicitlyDeletable"),
            _ => true,
        }
    }

    fn is_hashable(&self, ty: &Ty) -> bool {
        match ty {
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

    fn struct_implicitly_deletable_conformance_ok(&self, name: &str) -> bool {
        let Some(info) = self.structs.get(name) else {
            return false;
        };
        info.fields
            .iter()
            .all(|(_, ty)| self.is_implicitly_deletable(ty))
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
        Ok(())
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.mutable_scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.mutable_scopes.pop();
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

    fn infer(&self, expr: &Expr) -> Result<Ty, TypeError> {
        match &expr.kind {
            ExprKind::Int(_) => Ok(Ty::IntLiteral),
            ExprKind::Float(_) => Ok(Ty::FloatLiteral),
            ExprKind::Bool(_) => Ok(Ty::Bool),
            ExprKind::Str(_) => Ok(Ty::String),
            ExprKind::None => Ok(Ty::None),
            ExprKind::TypeValue(_) => Err(TypeError::Unsupported(
                "function types as compile-time values".to_string(),
            )),
            ExprKind::Invoke { .. } => Err(TypeError::Unsupported(
                "calls through callable expressions".to_string(),
            )),
            ExprKind::BraceLit(_) => Err(TypeError::Unsupported(
                "brace-delimited collection literals".to_string(),
            )),
            ExprKind::Identifier(name) => self
                .lookup(name)
                .cloned()
                .ok_or_else(|| TypeError::UndefinedVariable(name.clone())),
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
            } => {
                let obj_ty = self.infer(object)?;
                for bound in [lower.as_deref(), upper.as_deref(), step.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    let bt = self.infer(bound)?;
                    if !coerces(&bt, &Ty::Int) {
                        return Err(TypeError::TypeMismatch {
                            expected: "Int".to_string(),
                            found: bt.to_string(),
                            context: "slice bound".to_string(),
                        });
                    }
                }
                match &obj_ty {
                    Ty::List(_) => Ok(obj_ty.clone()),
                    Ty::String => Ok(Ty::String),
                    _ => Err(TypeError::NotIndexable(obj_ty.to_string())),
                }
            }
            ExprKind::TString { .. } => Err(TypeError::Unsupported("t-string".to_string())),
            // A parameterized type is not a runtime value; it is only valid as a
            // static-method receiver (`UnsafePointer[T].alloc(…)`), typed in
            // `infer_method_call`.
            ExprKind::TypeApply { name, .. } => Err(TypeError::TypeMismatch {
                expected: "a value".to_string(),
                found: format!("the type '{name}[…]'"),
                context: "a parameterized type is not a value".to_string(),
            }),
        }
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
                let aty = self.infer(arg)?;
                if !coerces(&aty, &elem) {
                    return Err(TypeError::TypeMismatch {
                        expected: elem.to_string(),
                        found: aty.to_string(),
                        context: format!("element {} of List", i + 1),
                    });
                }
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

    /// Validate an assignment **place** and return the type stored there. A place
    /// is a chain of field (`.x`) and index (`[i]`) accesses over a root that
    /// must be a mutable location: any variable, or `self` in a `mut self`
    /// method. Recursing on the object of each step verifies the whole chain is
    /// rooted at a mutable place (so `foo().x = e` or `self.x` in a read-only
    /// method are rejected). SIMD lane writes are not supported yet.
    fn check_place(&self, place: &Expr) -> Result<Ty, TypeError> {
        match &place.kind {
            ExprKind::Identifier(name) => {
                if name == "self" && !self.self_mutable {
                    return Err(TypeError::ImmutableSelf);
                }
                if !self.is_binding_mutable(name) {
                    return Err(TypeError::ImmutableBinding(name.clone()));
                }
                self.lookup(name)
                    .cloned()
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
                    // A pointer store `ptr[i] = e`: the target is the pointee type.
                    Ty::Pointer(elem) => (**elem).clone(),
                    // A SIMD lane write `v[i] = e`: the target is the width-1 scalar.
                    Ty::Simd { dtype, .. } => simd_ty(*dtype, 1),
                    _ => return Err(TypeError::NotIndexable(obj_ty.to_string())),
                };
                let idx_ty = self.infer(index)?;
                if !coerces(&idx_ty, &Ty::Int) {
                    return Err(TypeError::TypeMismatch {
                        expected: "Int".to_string(),
                        found: idx_ty.to_string(),
                        context: "index".to_string(),
                    });
                }
                Ok(elem)
            }
            other => Err(TypeError::InvalidAssignTarget(format!("{:?}", other))),
        }
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
            return Ok(elems[i as usize].clone());
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
        // The result of indexing: a SIMD lane, a List element, or a pointer pointee.
        let result = match &obj_ty {
            Ty::Simd { dtype, .. } => simd_ty(*dtype, 1),
            Ty::List(elem) => (**elem).clone(),
            Ty::Pointer(elem) => (**elem).clone(),
            _ => return Err(TypeError::NotIndexable(obj_ty.to_string())),
        };
        let idx_ty = self.infer(index)?;
        if !coerces(&idx_ty, &Ty::Int) {
            return Err(TypeError::TypeMismatch {
                expected: "Int".to_string(),
                found: idx_ty.to_string(),
                context: "index".to_string(),
            });
        }
        Ok(result)
    }

    /// Type a field access `object.field`. On a generic struct value the field
    /// type has the struct's type arguments substituted in (`Pair[Int].left :
    /// Int`).
    fn infer_member(&self, object: &Expr, field: &str) -> Result<Ty, TypeError> {
        // `Self.n` reads the enclosing struct's value parameter (an `Int`).
        if let ExprKind::Identifier(s) = &object.kind
            && s == "Self"
        {
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
            return Ok(ty);
        }
        let obj_ty = self.infer(object)?;
        if let Ty::Struct(sname, targs) = &obj_ty {
            let info = self.structs.get(sname).ok_or_else(|| {
                TypeError::InvariantViolation(format!("struct '{sname}' was not registered"))
            })?;
            if let Some((_, fty)) = info.fields.iter().find(|(n, _)| n == field) {
                let subst = struct_subst(&info.decls, targs);
                return Ok(substitute(fty, &subst));
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
        let obj_ty = self.infer(object)?;
        // Built-in `List` methods (mutating; require a plain variable receiver).
        if let Ty::List(elem) = &obj_ty {
            reject_kwargs(kwargs)?;
            return self.infer_list_method(object, method, elem, args);
        }
        // Built-in `UnsafePointer` methods (`free`).
        if let Ty::Pointer(elem) = &obj_ty {
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
                            let params: Vec<Ty> =
                                sig.params.iter().map(|t| substitute(t, &subst)).collect();
                            let variadic = sig.variadic.as_ref().map(|ty| substitute(ty, &subst));
                            if let Ok((score, slots)) = self.score_method_call(
                                sig,
                                &params,
                                variadic.as_ref(),
                                args,
                                kwargs,
                            ) {
                                matches.push(MethodCallResolution {
                                    conversion_score: score,
                                    slots,
                                    conventions: sig.conventions.clone(),
                                    return_type: substitute(&sig.ret, &subst),
                                    mutates_receiver: matches!(
                                        sig.self_convention,
                                        Some(crate::ast::ArgConvention::Mut)
                                    ),
                                    lowered_name: overloaded
                                        .then(|| method_lowered_name(sname, method, sig)),
                                });
                            }
                        }
                        select_method_overload(method, matches).map(Some)
                    }
                    None => Ok(None),
                }
            }
            Ty::Param { bounds, .. } => Ok(self
                .lookup_trait_method(bounds, method, args.len())
                .map(|sig| MethodCallResolution {
                    conversion_score: 0,
                    slots: (0..args.len()).map(ArgSlot::Positional).collect(),
                    conventions: sig.conventions.clone(),
                    return_type: substitute_self(&sig.ret, &obj_ty),
                    mutates_receiver: matches!(
                        sig.self_convention,
                        Some(crate::ast::ArgConvention::Mut)
                    ),
                    lowered_name: None,
                })),
            // `x.__hash__()` on a concrete built-in hashable type (`Int`, `String`,
            // …) is an intrinsic returning `UInt` — lets a key struct combine
            // `self.field.__hash__()` values (Phase 6).
            _ if method == "__hash__" && args.is_empty() && builtin_hashable_ty(&obj_ty) => {
                Ok(Some(MethodCallResolution {
                    conversion_score: 0,
                    slots: vec![],
                    conventions: vec![],
                    return_type: Ty::UInt,
                    mutates_receiver: false,
                    lowered_name: None,
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
        if let Some(target) = resolved.lowered_name {
            self.overload_targets.borrow_mut().insert(span, target);
        }
        // A `mut self` method mutates its receiver, so the receiver must be a
        // writable place (the mutation is written back to it): a variable, a
        // field/index chain, or `self` in a `mut self` method.
        if resolved.mutates_receiver {
            self.check_place(object)?;
        }
        for (index, slot) in resolved.slots.iter().enumerate() {
            let expression = match slot {
                ArgSlot::Positional(position) => &args[*position],
                ArgSlot::Keyword(position) => &kwargs[*position].value,
                ArgSlot::Default => continue,
            };
            let ty = self.infer(expression)?;
            if matches!(
                resolved.conventions.get(index),
                Some(Some(ArgConvention::Owned | ArgConvention::Deinit))
            ) {
                self.check_consuming(
                    expression,
                    &ty,
                    &format!("argument {} to method '{}'", index + 1, method),
                )?;
            }
        }
        check_call_aliasing(&resolved.slots, &resolved.conventions, args, kwargs)?;
        Ok(resolved.return_type)
    }

    fn score_method_call(
        &self,
        signature: &MethodSig,
        params: &[Ty],
        variadic: Option<&Ty>,
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<(usize, Vec<ArgSlot>), TypeError> {
        let keyword_names: Vec<_> = kwargs.iter().map(|arg| arg.name.as_str()).collect();
        let matched = match_call_slots(
            &signature.names,
            &signature.required,
            signature.positional_only,
            signature.keyword_only,
            args.len(),
            &keyword_names,
            CallVariadics {
                positional: variadic.is_some(),
                keyword: false,
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
            let actual = self.infer(expression)?;
            if !coerces(&actual, &params[index]) {
                return Err(TypeError::TypeMismatch {
                    expected: params[index].to_string(),
                    found: actual.to_string(),
                    context: "method overload candidate".to_string(),
                });
            }
            if actual != params[index] {
                score += 1;
            }
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
                if actual != *element {
                    score += 1;
                }
            }
        }
        Ok((score, slots))
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
            "alloc" => {
                if args.len() != 1 {
                    return Err(TypeError::ArityMismatch {
                        name: "alloc".to_string(),
                        expected: 1,
                        got: args.len(),
                    });
                }
                let aty = self.infer(&args[0])?;
                if !coerces(&aty, &Ty::Int) {
                    return Err(TypeError::TypeMismatch {
                        expected: "Int".to_string(),
                        found: aty.to_string(),
                        context: "argument to 'UnsafePointer.alloc'".to_string(),
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
                object_type: Ty::Pointer(Box::new(elem.clone())).to_string(),
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

    fn iterable_element_ty(&self, ty: &Ty) -> Result<Ty, TypeError> {
        match ty {
            Ty::Range => Ok(Ty::Int),
            Ty::List(elem) => Ok((**elem).clone()),
            Ty::Struct(..) => self.iter_element_ty(ty),
            Ty::Param { bounds, .. } => {
                if self.lookup_trait_assoc_type(bounds, "Element").is_some() {
                    Ok(Ty::Assoc {
                        base: Box::new(ty.clone()),
                        name: "Element".to_string(),
                    })
                } else {
                    Err(TypeError::TypeMismatch {
                        expected: "range, List, a type with __iter__, or an Iterable-style bound"
                            .to_string(),
                        found: ty.to_string(),
                        context: "for-loop iterable".to_string(),
                    })
                }
            }
            other => Err(TypeError::TypeMismatch {
                expected: "range, List, a type with __iter__, or an Iterable-style bound"
                    .to_string(),
                found: other.to_string(),
                context: "for-loop iterable".to_string(),
            }),
        }
    }

    /// The `for`-loop element type of a user struct iterable, validating the
    /// stable Mojo iterator protocol. A struct defines `__iter__(self) -> Iter`,
    /// where `Iter` is either a built-in `List[Element]` or a struct with
    /// `__next__(mut self) -> Element` (advances) and `__len__(self) -> Int`
    /// (remaining count — bounded iteration).
    fn iter_element_ty(&self, c_ty: &Ty) -> Result<Ty, TypeError> {
        let no_method = |ty: &Ty, m: &str| TypeError::NoSuchMethod {
            object_type: ty.to_string(),
            method: m.to_string(),
        };
        let Ty::Struct(cname, ctargs) = c_ty else {
            return Err(no_method(c_ty, "__iter__"));
        };
        let cinfo = self.structs.get(cname).ok_or_else(|| {
            TypeError::InvariantViolation(format!("struct '{cname}' was not registered"))
        })?;
        let iter_sig = cinfo
            .methods
            .get("__iter__")
            .and_then(|sigs| sigs.iter().find(|sig| sig.params.is_empty()))
            .ok_or_else(|| no_method(c_ty, "__iter__"))?;
        if !iter_sig.params.is_empty() {
            return Err(TypeError::ArityMismatch {
                name: "__iter__".to_string(),
                expected: 0,
                got: iter_sig.params.len(),
            });
        }
        let it_ty = substitute(&iter_sig.ret, &struct_subst(&cinfo.decls, ctargs));
        if let Ty::List(elem) = &it_ty {
            return Ok((**elem).clone());
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
            return self.iter_element_ty(&it_ty);
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
        Ok(substitute(&next_sig.ret, &isubst))
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

    /// Find a `method` required by any of the given trait `bounds` (a bounded
    /// type parameter's usable methods). Built-in bounds contribute none.
    fn lookup_trait_method(
        &self,
        bounds: &[String],
        method: &str,
        argc: usize,
    ) -> Option<MethodSig> {
        // The built-in `Hashable` trait contributes `__hash__(self) -> UInt`
        // (Phase 6). A user trait cannot shadow a built-in name, so this is
        // unambiguous.
        if method == "__hash__" && argc == 0 && bounds.iter().any(|b| b == "Hashable") {
            return Some(MethodSig::intrinsic(vec![], Ty::UInt));
        }
        // The built-in numeric-rounding traits contribute a `-> Self` dunder
        // (Phase 7), used by the self-hosted `math` module: `Floorable`/
        // `Ceilable`/`Truncable` a nullary `__floor__`/`__ceil__`/`__trunc__`,
        // and `CeilDivable`/`CeilDivableRaising` a unary `__ceildiv__(Self)`.
        let accepts = math_dunder_bound(method, argc);
        if !accepts.is_empty() && bounds.iter().any(|b| accepts.contains(&b.as_str())) {
            let params = if argc == 1 {
                vec![Ty::SelfType]
            } else {
                vec![]
            };
            return Some(MethodSig::intrinsic(params, Ty::SelfType));
        }
        bounds
            .iter()
            .filter_map(|b| self.traits.get(b))
            .find_map(|info| {
                info.methods
                    .get(method)
                    .and_then(|sigs| sigs.iter().find(|sig| sig.params.len() == argc))
                    .cloned()
            })
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
                    || (lt == rt && (is_scalar(&lt) || has_equality_bound(&lt))) =>
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
    /// a `List[T]` (with `x` coercing to an equatable `T`) or a `String` (with
    /// `x` a `String` — substring test).
    fn infer_membership(&self, op: InfixOp, lt: &Ty, rt: &Ty) -> Result<Ty, TypeError> {
        let ok = match rt {
            Ty::List(elem) => coerces(lt, elem) && is_list_equatable(elem),
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
        let (dtype, width) = self.simd_dims(param_args)?;
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

    fn infer_call(
        &self,
        span: SourceSpan,
        name: &str,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Ty, TypeError> {
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
                "abs" => return self.infer_abs(args),
                "min" | "max" => return self.infer_min_max(name, args),
                "round" => return self.infer_round(args),
                "input" => return self.infer_input(args),
                "len" => return self.infer_len(args),
                "range" => return self.infer_range(args),
                "Int" => return self.infer_conversion(Ty::Int, args),
                "UInt" => return self.infer_conversion(Ty::UInt, args),
                "Float64" => return self.infer_conversion(Ty::Float64, args),
                "Bool" => return self.infer_conversion(Ty::Bool, args),
                "divmod" => return self.infer_divmod(args),
                "SIMD" => return self.infer_simd_construction(param_args, args),
                "List" => return self.infer_list_construction(param_args, args),
                "Error" => return self.infer_error_construction(args),
                _ if Dtype::from_scalar_alias(name).is_some() => {
                    let dtype = Dtype::from_scalar_alias(name).unwrap();
                    return self.infer_simd_alias_construction(dtype, param_args, args);
                }
                _ => return Err(TypeError::UndefinedVariable(name.to_string())),
            },
        };
        if let Ty::Overload(candidates) = ty {
            let mut matches = Vec::new();
            for candidate in candidates {
                if let Ok((ret, score)) =
                    self.infer_callable_ty(name, candidate.clone(), param_args, args, kwargs)
                    && let Some(target) = callable_lowered_name(name, &candidate)
                {
                    matches.push((ret, score, target));
                }
            }
            return match select_callable_overload(matches) {
                Ok((ret, target)) => {
                    self.overload_targets.borrow_mut().insert(span, target);
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
        self.infer_callable_ty(name, ty, param_args, args, kwargs)
            .map(|(ret, _)| ret)
    }

    fn infer_callable_ty(
        &self,
        name: &str,
        ty: Ty,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<(Ty, usize), TypeError> {
        let (params, names, ret, required, variadic, positional_only, keyword_only, conventions) =
            match ty {
                // A non-generic function takes no compile-time parameters.
                Ty::Func {
                    params,
                    names,
                    ret,
                    required,
                    variadic,
                    positional_only,
                    keyword_only,
                    conventions,
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
                        positional_only,
                        keyword_only,
                        conventions,
                    )
                }
                // Bind ordinary arguments first, then infer or apply the generic
                // function's compile-time parameters from the occupied slots.
                generic @ Ty::GenericFunc { .. } => {
                    return self
                        .infer_generic_call(name, &generic, param_args, args, kwargs)
                        // Concrete overloads are preferred to generic candidates.
                        // Keep the genericity penalty above any realistic argument
                        // coercion count while preserving ties between generics.
                        .map(|ret| (ret, 1 << 20));
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
        let kw_names: Vec<&str> = kwargs.iter().map(|k| k.name.as_str()).collect();
        let kw_collector = self.kw_collectors.borrow().get(name).cloned();
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
            let arg_ty = self.infer(arg)?;
            if !coerces(&arg_ty, &params[i]) {
                return Err(TypeError::TypeMismatch {
                    expected: params[i].to_string(),
                    found: arg_ty.to_string(),
                    context: format!("argument '{}' to '{}'", names[i], name),
                });
            }
            if arg_ty != params[i] {
                score += 1;
            }
            // Only an `owned`/`deinit` parameter *consumes* its argument (moving the
            // value in). `read` (the default), `mut`, and `ref` all **borrow** — no
            // copy — so passing a non-Copyable value to them is fine.
            if matches!(
                conventions.get(i),
                Some(Some(ArgConvention::Owned | ArgConvention::Deinit))
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
            for &p in &overflow {
                let arg_ty = self.infer(&args[p])?;
                if !coerces(&arg_ty, elem) {
                    return Err(TypeError::TypeMismatch {
                        expected: elem.to_string(),
                        found: arg_ty.to_string(),
                        context: format!("variadic argument to '{}'", name),
                    });
                }
                if arg_ty != **elem {
                    score += 1;
                }
            }
        }
        if let Some(elem) = kw_collector {
            for index in kw_overflow {
                let found = self.infer(&kwargs[index].value)?;
                if !coerces(&found, &elem) {
                    return Err(TypeError::TypeMismatch {
                        expected: elem.to_string(),
                        found: found.to_string(),
                        context: format!(
                            "keyword '{}' collected by '{}'",
                            kwargs[index].name, name
                        ),
                    });
                }
            }
        }

        // Borrow check (mutable-XOR-shared), root-sensitive: within one call a
        // variable borrowed exclusively (`mut`/`ref`) or moved (`^`) may not be
        // borrowed again — mutably, shared, or moved.
        check_call_aliasing(&slots, &conventions, args, kwargs)?;

        Ok((*ret, score))
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
    ) -> Result<Ty, TypeError> {
        let Ty::GenericFunc {
            decls,
            params,
            names,
            ret,
            required,
            variadic,
            positional_only,
            keyword_only,
            conventions,
        } = generic
        else {
            return Err(TypeError::InvariantViolation(format!(
                "generic call inference received non-generic callee '{name}'"
            )));
        };
        let kw_names: Vec<&str> = kwargs.iter().map(|k| k.name.as_str()).collect();
        let matched = match_call_slots(
            names,
            required,
            *positional_only,
            *keyword_only,
            args.len(),
            &kw_names,
            CallVariadics {
                positional: variadic.is_some(),
                keyword: false,
            },
        )
        .map_err(|e| e.into_type_error(name))?;
        let (slots, overflow) = (matched.slots, matched.positional_overflow);
        let mut use_params = Vec::new();
        let mut arg_tys = Vec::new();
        for (i, slot) in slots.iter().enumerate() {
            let arg = match slot {
                ArgSlot::Positional(p) => &args[*p],
                ArgSlot::Keyword(k) => &kwargs[*k].value,
                ArgSlot::Default => continue,
            };
            use_params.push(params[i].clone());
            arg_tys.push(self.infer(arg)?);
        }
        if let Some(elem) = variadic.as_deref() {
            for &p in &overflow {
                use_params.push(elem.clone());
                arg_tys.push(self.infer(&args[p])?);
            }
        }
        let (subst, _tyargs) =
            self.resolve_use_params(name, decls, param_args, &use_params, &arg_tys)?;
        for (aty, pty) in arg_tys.iter().zip(&use_params) {
            let expected = self.resolve_assoc_ty(&substitute(pty, &subst));
            if !coerces(aty, &expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: aty.to_string(),
                    context: format!("argument to '{}'", name),
                });
            }
        }
        for (i, slot) in slots.iter().enumerate() {
            if matches!(
                conventions.get(i),
                Some(Some(ArgConvention::Owned | ArgConvention::Deinit))
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
        check_call_aliasing(&slots, conventions, args, kwargs)?;
        Ok(self.resolve_assoc_ty(&substitute(ret, &subst)))
    }

    /// Type the built-in `print(...)`: any number of *printable* arguments,
    /// returning `None`. (Unlike Mojo, an argument need not conform to `Writable`
    /// — any displayable value prints; only functions/ranges/opaque parameters
    /// are rejected.)
    fn infer_print(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        for (i, arg) in args.iter().enumerate() {
            let ty = self.infer(arg)?;
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
        // `String(c)` on a user struct dispatches to `c.__str__()` (`Stringable`),
        // which must return `String`.
        if let Some(r) = self.struct_dunder(&tys[0], "__str__", &[]) {
            return r.and_then(|ret| require_dunder_ret(ret, &Ty::String, "__str__"));
        }
        Err(TypeError::TypeMismatch {
            expected: "a numeric, Bool, or String value".to_string(),
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

    /// Type `len(x)`: a `String` or `List` argument, returning `Int`.
    fn infer_len(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("len", 1, args)?;
        if matches!(tys[0], Ty::String | Ty::List(_)) {
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
            expected: "String or List".to_string(),
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

/// A readable symbol for an infix operator, for error messages.
fn infix_symbol(op: InfixOp) -> &'static str {
    match op {
        InfixOp::Add => "+",
        InfixOp::Sub => "-",
        InfixOp::Mul => "*",
        InfixOp::Div => "/",
        InfixOp::FloorDiv => "//",
        InfixOp::Mod => "%",
        InfixOp::Shl => "<<",
        InfixOp::Shr => ">>",
        InfixOp::BitAnd => "&",
        InfixOp::BitOr => "|",
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
