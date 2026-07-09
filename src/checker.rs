//! Static type checker: the pass between parsing and evaluation.
//!
//! mojo-lite's evaluator is dynamically typed — annotations are parsed but
//! ignored at runtime. This pass enforces them so that the errors real Mojo
//! reports at compile time are reported here, before evaluation. It is a *sound*
//! approximation: if [`check`] succeeds, evaluating the program will not raise
//! `UndefinedVariable`, `TypeError`, `NotCallable`, `ArityMismatch`, or
//! `ClosureEscape`. It is deliberately not *complete* — see the forward-reference
//! note below — so a few valid Mojo programs are rejected.
//!
//! ## Scoping
//! A stack of scopes (`Vec<HashMap<String, Ty>>`) mirrors the evaluator's lexical
//! scope chain. Names are bound *sequentially* in source order, exactly as the
//! evaluator binds them, and a nested `def` body is checked at its definition
//! site with the enclosing scopes still on the stack (so capture is lexical).
//! One consequence: a function body may not forward-reference a sibling `def`
//! declared later in the same block (mutual recursion), even though the evaluator
//! would resolve it at call time. Choosing soundness over completeness here keeps
//! the checker simple; hoisting `def` signatures per block is future work.

use std::collections::HashMap;

use crate::ast::{
    ArgConvention, Dtype, Expr, ExprKind, FnParam, InfixOp, Method, PrefixOp, Stmt, StmtKind,
    StructComptime, TraitComptime, Type,
};
use crate::ct::CtValue;
use crate::error::TypeError;
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
    methods: HashMap<String, MethodSig>,
    fieldwise_init: bool,
}

/// The checked signature of a trait: required methods plus associated
/// compile-time facts. A method requirement's signature may mention
/// `Ty::SelfType` (the conforming type).
struct TraitInfo {
    methods: HashMap<String, MethodSig>,
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
    params: Vec<Ty>,
    ret: Ty,
    /// `mut self` — the method may mutate `self`, so a call site must have a
    /// mutable variable receiver (the mutation is written back to it).
    mut_self: bool,
}

/// Type-check a whole program. Convenience wrapper over [`Checker`].
pub fn check(stmts: &[Stmt]) -> Result<(), TypeError> {
    Checker::new().check_program(stmts)
}

/// A single-pass static type checker over the parsed AST.
pub struct Checker {
    /// Lexical scope chain, innermost last. Starts with the global scope.
    scopes: Vec<HashMap<String, Ty>>,
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
}

impl Checker {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
            structs: HashMap::new(),
            traits: HashMap::new(),
            tparams: Vec::new(),
            self_decls: Vec::new(),
            self_ty: None,
            trait_self_comptime: Vec::new(),
            comptimes: HashMap::new(),
            self_mutable: false,
        }
    }

    /// The type denoted by a source annotation; resolves type parameters and
    /// validates struct names and type-argument counts.
    fn ty_from_anno(&self, ty: &Type) -> Result<Ty, TypeError> {
        Ok(match ty {
            Type::Int => Ty::Int,
            Type::UInt => Ty::UInt,
            Type::Bool => Ty::Bool,
            Type::String => Ty::String,
            Type::Float64 => Ty::Float64,
            Type::None => Ty::None,
            // Function-type annotations parse but their semantics are deferred: the
            // checker's `Ty::Func` only ever arises from a `def` (with names/arity),
            // so a function-typed binding is not modeled yet.
            Type::Func { .. } => {
                return Err(TypeError::Unsupported(
                    "function type annotation".to_string(),
                ));
            }
            // A `ref [origin] T` reference type parses (its origin discarded) but
            // reference semantics / origins are not modeled.
            Type::Ref(_) => {
                return Err(TypeError::Unsupported("reference type".to_string()));
            }
            // A bare name may be an in-scope type parameter (a generic `def`'s
            // `T`) or a struct type, optionally applied to parameter arguments.
            Type::Named(name, args) => {
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
            Type::SelfParam(name) => match self.self_decls.iter().find(|d| d.name() == name) {
                Some(ParamDecl::Type { bounds, .. }) => Ty::Param {
                    name: name.clone(),
                    bounds: bounds.clone(),
                },
                _ => return self.associated_type_for_self(name),
            },
            // Bare `Self` — the enclosing struct type or a trait's abstract Self.
            // Not usable as a type in a value-parameterized struct (a value
            // parameter can't appear in a type).
            Type::SelfType => match &self.self_ty {
                Some(Ty::Struct(_, args)) if args.iter().any(|a| matches!(a, TyArg::Val(_))) => {
                    return Err(TypeError::UnknownSelfParam("Self".to_string()));
                }
                Some(ty) => ty.clone(),
                None => return Err(TypeError::UnknownSelfParam("Self".to_string())),
            },
            Type::Assoc { base, name } => {
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
                conventions,
            } => Ty::Func {
                params: params.iter().map(|p| self.resolve_assoc_ty(p)).collect(),
                names: names.clone(),
                ret: Box::new(self.resolve_assoc_ty(ret)),
                required: *required,
                variadic: variadic
                    .as_ref()
                    .map(|v| Box::new(self.resolve_assoc_ty(v))),
                conventions: conventions.clone(),
            },
            Ty::GenericFunc { decls, params, ret } => Ty::GenericFunc {
                decls: decls.clone(),
                params: params.iter().map(|p| self.resolve_assoc_ty(p)).collect(),
                ret: Box::new(self.resolve_assoc_ty(ret)),
            },
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
                    }) => self.ty_from_anno(&Type::Named(id.clone(), vec![]))?,
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
                self.ty_from_anno(&Type::Named(id.clone(), vec![]))?,
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
            }) => self.ty_from_anno(&Type::Named(id.clone(), vec![]))?,
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
                }) => self.ty_from_anno(&Type::Named(id.clone(), vec![]))?,
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
    fn ct_member_req_from_anno(&self, ty: &Type) -> Result<CtMemberReq, TypeError> {
        if let Type::Named(name, args) = ty
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
        self.ty_from_anno(&Type::Named(name.to_string(), args.to_vec()))
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
        self.scopes.push(HashMap::new());
        let result = self.check_block(stmts, ret, in_loop);
        self.scopes.pop();
        result
    }

    fn check_stmt(
        &mut self,
        stmt: &Stmt,
        ret: Option<&Ty>,
        in_loop: bool,
    ) -> Result<(), TypeError> {
        match &stmt.kind {
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
                match self.lookup(name).cloned() {
                    // Re-assignment: the value must keep the variable's type.
                    Some(target) => {
                        // Assigning a closure could move it to an outer binding.
                        if matches!(found, Ty::Func { .. } | Ty::GenericFunc { .. }) {
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
                    // (implicit declaration). Mojo allows it; mojo-lite parses and
                    // type-checks it (binding the materialized type) but the
                    // evaluator flags it as unsupported. Binding here keeps the rest
                    // of the program type-checking.
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
                // Conventions / markers / `**kwargs` are parsed but not implemented;
                // **default values** and a trailing **`*args`** are supported on
                // non-generic functions.
                if let Some(feature) = Self::advanced_param_feature(
                    params,
                    *positional_only,
                    *keyword_only,
                    false,
                    false,
                ) {
                    return Err(TypeError::Unsupported(feature.to_string()));
                }
                let has_default = params.iter().any(|p| p.default.is_some());
                if has_default && !type_params.is_empty() {
                    return Err(TypeError::Unsupported(
                        "default values on generic functions".to_string(),
                    ));
                }
                // A `*args` variadic must be a single, trailing parameter, on a
                // non-generic function.
                let variadic_idx = params
                    .iter()
                    .position(|p| p.kind == crate::ast::ParamKind::Variadic);
                if let Some(vi) = variadic_idx {
                    if !type_params.is_empty() {
                        return Err(TypeError::Unsupported(
                            "variadic parameters on generic functions".to_string(),
                        ));
                    }
                    if vi != params.len() - 1 {
                        return Err(TypeError::Unsupported(
                            "a parameter after '*args' (keyword-only) is not supported".to_string(),
                        ));
                    }
                }
                // Regular (non-variadic) parameters, over which arity is computed.
                let regular = &params[..variadic_idx.unwrap_or(params.len())];
                let required = required_count(regular)?;
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
                    let nreg = variadic_idx.unwrap_or(params.len());
                    Ty::Func {
                        params: param_tys[..nreg].to_vec(),
                        names: regular.iter().map(|p| p.name.clone()).collect(),
                        ret: Box::new(ret_ty.clone()),
                        required,
                        variadic: variadic_idx.map(|vi| Box::new(param_tys[vi].clone())),
                        conventions: regular.iter().map(|p| p.convention).collect(),
                    }
                } else {
                    Ty::GenericFunc {
                        decls: decls.clone(),
                        params: param_tys.clone(),
                        ret: Box::new(ret_ty.clone()),
                    }
                };
                if let Err(e) = self.declare(name, fn_ty) {
                    self.tparams.pop();
                    return Err(e);
                }

                self.scopes.push(HashMap::new());
                let mut result = Ok(());
                // Value parameters are ordinary `Int` locals in the body.
                for d in &decls {
                    if let ParamDecl::Value { name } = d {
                        result = self.declare(name, Ty::Int);
                        if result.is_err() {
                            break;
                        }
                    }
                }
                if result.is_ok() {
                    for (param, ty) in params.iter().zip(&param_tys) {
                        // A `*args` parameter is a `List[element]` inside the body;
                        // a regular parameter keeps its declared type.
                        let bind_ty = if param.kind == crate::ast::ParamKind::Variadic {
                            Ty::List(Box::new(ty.clone()))
                        } else {
                            ty.clone()
                        };
                        // Duplicate parameter names are a redeclaration.
                        result = self.declare(&param.name, bind_ty);
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
                self.scopes.pop();
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
            } => self.check_struct(
                name,
                type_params,
                conforms,
                fields,
                associated,
                methods,
                *fieldwise_init,
            ),

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
                        self.declare(name, Ty::Int)
                    }
                    Err(_) => {
                        let ty = self.infer(value)?;
                        let declared = self.inferred_binding_ty(&ty, name)?;
                        self.declare(name, declared)
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
                    self.scopes.push(HashMap::new());
                    // `except e:` binds the caught error as an `Error`.
                    let result = match name {
                        Some(n) => self
                            .declare(n, Ty::Error)
                            .and_then(|()| self.check_block(ex_body, ret, in_loop)),
                        None => self.check_block(ex_body, ret, in_loop),
                    };
                    self.scopes.pop();
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
                self.scopes.push(HashMap::new());
                let result = match self.declare(var, elem_ty) {
                    Ok(()) => self.check_block(body, ret, true),
                    Err(e) => Err(e),
                };
                self.scopes.pop();
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
                // Returning a function value is an escape, regardless of the
                // declared return type — match the evaluator's ClosureEscape.
                if matches!(found, Ty::Func { .. } | Ty::GenericFunc { .. }) {
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

    /// The name of the first advanced parameter feature used by a signature (a
    /// default value, a `*args`/`**kwargs` variadic, an argument convention, or a
    /// `/`/`*` marker), or `None` if the signature is a plain list of typed
    /// parameters. These are **parsed** but not implemented, so a signature using
    /// any of them is flagged unsupported.
    fn advanced_param_feature(
        params: &[crate::ast::FnParam],
        positional_only: Option<usize>,
        keyword_only: Option<usize>,
        flag_defaults: bool,
        flag_variadic: bool,
    ) -> Option<&'static str> {
        use crate::ast::{ArgConvention, ParamKind};
        if flag_defaults && params.iter().any(|p| p.default.is_some()) {
            return Some("default argument values");
        }
        if flag_variadic && params.iter().any(|p| p.kind == ParamKind::Variadic) {
            return Some("variadic '*args' parameters");
        }
        if params.iter().any(|p| p.kind == ParamKind::KwVariadic) {
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
        if positional_only.is_some() {
            return Some("positional-only '/' parameters");
        }
        if keyword_only.is_some() {
            return Some("keyword-only '*' parameters");
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
            let mut sigs: HashMap<String, MethodSig> = HashMap::new();
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
                ) {
                    return Err(TypeError::Unsupported(feature.to_string()));
                }
                let sig = MethodSig {
                    params: self.param_tys(&m.params)?,
                    ret: match &m.ret {
                        Some(t) => self.ty_from_anno(t)?,
                        None => Ty::None,
                    },
                    mut_self: false,
                };
                if sigs.insert(m.name.clone(), sig).is_some() {
                    return Err(TypeError::Redeclaration(m.name.clone()));
                }
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
    #[allow(clippy::too_many_arguments)]
    fn check_struct(
        &mut self,
        name: &str,
        type_params: &[crate::ast::TypeParam],
        conforms: &[String],
        fields: &[crate::ast::Param],
        associated: &[StructComptime],
        methods: &[Method],
        fieldwise_init: bool,
    ) -> Result<(), TypeError> {
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
        let result = self.check_struct_members(
            name,
            decls,
            conforms,
            fields,
            associated,
            methods,
            fieldwise_init,
            &self_ty,
        );
        self.self_decls = saved_self_decls;
        self.self_ty = saved_self_ty;
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn check_struct_members(
        &mut self,
        name: &str,
        decls: Vec<ParamDecl>,
        conforms: &[String],
        fields: &[crate::ast::Param],
        associated: &[StructComptime],
        methods: &[Method],
        fieldwise_init: bool,
        self_ty: &Ty,
    ) -> Result<(), TypeError> {
        // Field types are resolved against structs defined *so far* (so a struct
        // can't contain itself); duplicate field names are a redeclaration.
        let mut field_tys: Vec<(String, Ty)> = Vec::new();
        for f in fields {
            if field_tys.iter().any(|(n, _)| n == &f.name) {
                return Err(TypeError::Redeclaration(f.name.clone()));
            }
            field_tys.push((f.name.clone(), self.ty_from_anno(&f.ty)?));
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
        for m in methods {
            let method_name = lifecycle_method_name(m);
            let sig = MethodSig {
                params: self.param_tys(&m.params)?,
                ret: match &m.ret {
                    Some(t) => self.ty_from_anno(t)?,
                    None => Ty::None,
                },
                mut_self: matches!(m.self_convention, Some(crate::ast::ArgConvention::Mut)),
            };
            let info = self.structs.get_mut(name).expect("just registered");
            if info.methods.insert(method_name.to_string(), sig).is_some() {
                return Err(TypeError::Redeclaration(method_name.to_string()));
            }
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
    /// every method required by trait `tr`, with a matching signature. Built-in
    /// traits impose no checked requirements.
    fn verify_conformance(&self, name: &str, tr: &str, self_ty: &Ty) -> Result<(), TypeError> {
        let trait_info = match self.traits.get(tr) {
            Some(info) => info,
            None => return Ok(()), // a built-in trait — nothing to check
        };
        let struct_info = self
            .structs
            .get(name)
            .expect("registered before conformance");
        for (mname, req_sig) in &trait_info.methods {
            let got =
                struct_info
                    .methods
                    .get(mname)
                    .ok_or_else(|| TypeError::MissingTraitMethod {
                        struct_name: name.to_string(),
                        trait_name: tr.to_string(),
                        method: mname.clone(),
                    })?;
            // The requirement's `Self` becomes this struct's type. `mut_self` is
            // not part of conformance checking yet, so mirror the implementation.
            let want = MethodSig {
                params: req_sig
                    .params
                    .iter()
                    .map(|t| self.resolve_assoc_ty(&substitute_self(t, self_ty)))
                    .collect(),
                ret: self.resolve_assoc_ty(&substitute_self(&req_sig.ret, self_ty)),
                mut_self: got.mut_self,
            };
            if *got != want {
                return Err(TypeError::TraitMethodMismatch {
                    struct_name: name.to_string(),
                    trait_name: tr.to_string(),
                    method: mname.clone(),
                });
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
                true,
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
        self.scopes.push(HashMap::new());
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
        self.scopes.pop();
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
        let info = self
            .structs
            .get(sname)
            .expect("struct types are registered");
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
        self.declare("self", self_ty.clone())?;
        for p in &m.params {
            let pty = self.ty_from_anno(&p.ty)?;
            self.declare(&p.name, pty)?;
        }
        // `self` is writable in a `mut self` method, or an `out self` `__init__`
        // (which assigns its fields). Restored after the body.
        let saved = std::mem::replace(
            &mut self.self_mutable,
            matches!(
                m.self_convention,
                Some(crate::ast::ArgConvention::Mut | crate::ast::ArgConvention::Out)
            ),
        );
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
        name: &str,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
        kwargs: &[crate::ast::KwArg],
    ) -> Result<Ty, TypeError> {
        let info = self
            .structs
            .get(name)
            .expect("caller checked it is a struct");
        if !kwargs.is_empty() {
            if args.is_empty()
                && kwargs.len() == 1
                && kwargs[0].name == "copy"
                && let Some(sig) = info.methods.get("__copyinit__")
            {
                let params = sig.params.clone();
                let decls = info.decls.clone();
                if params.len() != 1 {
                    return Err(TypeError::BadCall {
                        func: name.to_string(),
                        reason: "copy constructor must take exactly one 'copy' argument"
                            .to_string(),
                    });
                }
                let arg_ty = self.infer(&kwargs[0].value)?;
                let (subst, tyargs) =
                    self.resolve_use_params(name, &decls, param_args, &params, std::slice::from_ref(&arg_ty))?;
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
        if let Some(sig) = info.methods.get("__init__") {
            let params = sig.params.clone();
            let decls = info.decls.clone();
            if params.len() != args.len() {
                return Err(TypeError::ArityMismatch {
                    name: name.to_string(),
                    expected: params.len(),
                    got: args.len(),
                });
            }
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
                self.check_consuming(&args[i], aty, &format!("argument {} to '{}'", i + 1, name))?;
            }
            return Ok(Ty::Struct(name.to_string(), tyargs));
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
                            });
                        }
                    }
                    tyargs.push(TyArg::Ty(solved.clone()));
                }
            }
        }
        Ok((subst, tyargs))
    }

    /// Whether `ty` conforms to trait `tr`. A built-in trait imposes no checked
    /// requirement (any type is accepted — we don't model, e.g., `Copyable`).
    /// A user trait is satisfied nominally: a struct must *declare* conformance,
    /// and a type parameter must carry `tr` among its bounds (so a bounded `T`
    /// can be forwarded to another `[U: tr]` parameter).
    fn conforms_to(&self, ty: &Ty, tr: &str) -> bool {
        if BUILTIN_TRAITS.contains(&tr) {
            return true;
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

    /// Whether a value of this type may be **copied** (implicitly duplicated). Mojo
    /// is move-only by default: scalars and the built-in value types are Copyable,
    /// (see the free [`is_place_expr`] used by [`Self::check_consuming`]).
    /// but a `struct` is Copyable only if it declares `Copyable` conformance **or
    /// defines `__copyinit__`** (which is what makes a type copyable), and a type
    /// parameter only if bounded by `Copyable`.
    fn is_copyable(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Struct(name, _) => self
                .structs
                .get(name)
                .map(|s| {
                    s.conforms.iter().any(|c| c == "Copyable")
                        || s.methods.contains_key("__copyinit__")
                })
                .unwrap_or(true),
            Ty::Param { bounds, .. } => bounds.iter().any(|b| b == "Copyable"),
            // Scalars, `String`, `List`/`Tuple`/`Simd`/`Range`, `Error`, closures,
            // and `Self` are treated as copyable (element-wise copyability of
            // aggregates is not modeled).
            _ => true,
        }
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

    /// Bind `name` in the innermost scope, rejecting a same-scope redeclaration.
    fn declare(&mut self, name: &str, ty: Ty) -> Result<(), TypeError> {
        let scope = self.scopes.last_mut().expect("scope stack is never empty");
        if scope.contains_key(name) {
            return Err(TypeError::Redeclaration(name.to_string()));
        }
        scope.insert(name.to_string(), ty);
        Ok(())
    }

    /// The type to declare for an **inferred** binding — `var x = e` (no
    /// annotation) or a var-less `x = e`. A numeric literal materializes to its
    /// default kind (`default_literal`); a value that cannot live in a named
    /// binding is rejected: a closure (`ClosureEscape`, matching `return`/reassign)
    /// or the non-first-class `range` (which has no annotation and only belongs in
    /// a `for` header).
    fn inferred_binding_ty(&self, value_ty: &Ty, name: &str) -> Result<Ty, TypeError> {
        match value_ty {
            Ty::Func { .. } | Ty::GenericFunc { .. } => Err(TypeError::ClosureEscape),
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
            } => self.infer_call(name, param_args, args, kwargs),
            ExprKind::Member { object, field } => self.infer_member(object, field),
            ExprKind::MethodCall {
                object,
                method,
                args,
                kwargs,
            } => {
                reject_kwargs(kwargs)?;
                self.infer_method_call(object, method, args)
            }
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
            // Walrus `name := value` types as `value` (the evaluator flags it as
            // unsupported). The name is not bound here — `infer` is read-only — so
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
        Ok(default_literal(&acc.expect("non-empty element list")))
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
                unreachable!("list_type returns a List");
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
                    let info = self
                        .structs
                        .get(sname)
                        .expect("struct types are registered");
                    let sig = info
                        .methods
                        .get("__setitem__")
                        .ok_or_else(|| TypeError::NotIndexable(obj_ty.to_string()))?;
                    if !sig.mut_self {
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

    /// Type a subscript `object[index]`: a SIMD lane read. The object must be a
    /// SIMD value and the index an `Int`; the result is the width-1 scalar.
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
            let info = self
                .structs
                .get(sname)
                .expect("struct types are registered");
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
        object: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        // A **static** method on a parameterized built-in type — the receiver is a
        // type, not a value (`UnsafePointer[T].alloc(n)`). Handled before inferring
        // the object (which would reject a bare `TypeApply`).
        if let ExprKind::TypeApply { name, args: targs } = &object.kind {
            return self.infer_static_method(name, targs, method, args);
        }
        let obj_ty = self.infer(object)?;
        // Built-in `List` methods (mutating; require a plain variable receiver).
        if let Ty::List(elem) = &obj_ty {
            return self.infer_list_method(object, method, elem, args);
        }
        // Built-in `UnsafePointer` methods (`free`).
        if let Ty::Pointer(elem) = &obj_ty {
            return self.infer_pointer_method(method, elem, args);
        }
        // Resolve the method to a concrete signature (params + return + whether
        // it mutates `self`) for this receiver, substituting the receiver's type
        // arguments (struct) or `Self` (a bounded type parameter's trait method).
        let resolved: Option<(Vec<Ty>, Ty, bool)> = match &obj_ty {
            Ty::Struct(sname, targs) => {
                let info = self
                    .structs
                    .get(sname)
                    .expect("struct types are registered");
                info.methods.get(method).map(|sig| {
                    let subst = struct_subst(&info.decls, targs);
                    (
                        sig.params.iter().map(|t| substitute(t, &subst)).collect(),
                        substitute(&sig.ret, &subst),
                        sig.mut_self,
                    )
                })
            }
            Ty::Param { bounds, .. } => self.lookup_trait_method(bounds, method).map(|sig| {
                (
                    sig.params
                        .iter()
                        .map(|t| substitute_self(t, &obj_ty))
                        .collect(),
                    substitute_self(&sig.ret, &obj_ty),
                    sig.mut_self,
                )
            }),
            _ => None,
        };
        let (params, ret, mut_self) = resolved.ok_or_else(|| TypeError::NoSuchMethod {
            object_type: obj_ty.to_string(),
            method: method.to_string(),
        })?;
        // A `mut self` method mutates its receiver, so the receiver must be a
        // writable place (the mutation is written back to it): a variable, a
        // field/index chain, or `self` in a `mut self` method.
        if mut_self {
            self.check_place(object)?;
        }
        if params.len() != args.len() {
            return Err(TypeError::ArityMismatch {
                name: method.to_string(),
                expected: params.len(),
                got: args.len(),
            });
        }
        for (i, (arg, expected)) in args.iter().zip(&params).enumerate() {
            let aty = self.infer(arg)?;
            if !coerces(&aty, expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: aty.to_string(),
                    context: format!("argument {} to method '{}'", i + 1, method),
                });
            }
        }
        Ok(ret)
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
        let sig = info.methods.get(name)?;
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
        let cinfo = self
            .structs
            .get(cname)
            .expect("struct types are registered");
        let iter_sig = cinfo
            .methods
            .get("__iter__")
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
            .ok_or_else(|| no_method(&it_ty, "__next__"))?;
        if !next_sig.params.is_empty() {
            return Err(TypeError::ArityMismatch {
                name: "__next__".to_string(),
                expected: 0,
                got: next_sig.params.len(),
            });
        }
        if !next_sig.mut_self {
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
    fn lookup_trait_method(&self, bounds: &[String], method: &str) -> Option<MethodSig> {
        bounds
            .iter()
            .filter_map(|b| self.traits.get(b))
            .find_map(|info| info.methods.get(method).cloned())
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
            // Short-circuiting boolean logic (the evaluator requires Bool here).
            And | Or if lt == Ty::Bool && rt == Ty::Bool => Some(Ty::Bool),
            // `+` concatenates String, or adds numbers (result = common type).
            Add if lt == Ty::String && rt == Ty::String => Some(Ty::String),
            // Arithmetic that preserves the operand type.
            Add | Sub | Mul | FloorDiv | Mod | Pow => common,
            // True division always yields Float64 (for any numeric operands).
            Div if common.is_some() => Some(Ty::Float64),
            // Ordering between numbers.
            Lt | Gt | Le | Ge if common.is_some() => Some(Ty::Bool),
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
            unreachable!()
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
                    return self.infer_construction(name, param_args, args, kwargs);
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
                "len" => return self.infer_len(args),
                "range" => return self.infer_range(args),
                "Int" => return self.infer_conversion(Ty::Int, args),
                "UInt" => return self.infer_conversion(Ty::UInt, args),
                "Float64" => return self.infer_conversion(Ty::Float64, args),
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
        let (params, names, ret, required, variadic, conventions) = match ty {
            // A non-generic function takes no compile-time parameters.
            Ty::Func {
                params,
                names,
                ret,
                required,
                variadic,
                conventions,
            } => {
                if !param_args.is_empty() {
                    return Err(TypeError::WrongTypeArgCount {
                        name: name.to_string(),
                        expected: 0,
                        got: param_args.len(),
                    });
                }
                (params, names, ret, required, variadic, conventions)
            }
            // A generic function infers or is supplied its parameters. Keyword
            // arguments to a generic function are deferred.
            Ty::GenericFunc { decls, params, ret } => {
                if !kwargs.is_empty() {
                    return Err(TypeError::Unsupported(
                        "keyword arguments to a generic function".to_string(),
                    ));
                }
                return self.infer_generic_call(name, &decls, &params, &ret, param_args, args);
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
        let (slots, overflow) = match_call_slots(
            &names,
            params.len(),
            required,
            args.len(),
            &kw_names,
            variadic.is_some(),
        )
        .map_err(|e| e.into_type_error(name))?;
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
            }
        }

        // Borrow check (mutable-XOR-shared), root-sensitive: within one call a
        // variable borrowed exclusively (`mut`/`ref`) or moved (`^`) may not be
        // borrowed again — mutably, shared, or moved.
        check_call_aliasing(&slots, &conventions, args, kwargs)?;

        Ok(*ret)
    }

    /// Type a call to a generic function: solve its type parameters from the
    /// argument types, then check each argument coerces to the substituted
    /// parameter type and return the substituted result type.
    fn infer_generic_call(
        &self,
        name: &str,
        decls: &[ParamDecl],
        params: &[Ty],
        ret: &Ty,
        param_args: &[crate::ast::ParamArg],
        args: &[Expr],
    ) -> Result<Ty, TypeError> {
        if params.len() != args.len() {
            return Err(TypeError::ArityMismatch {
                name: name.to_string(),
                expected: params.len(),
                got: args.len(),
            });
        }
        let arg_tys = args
            .iter()
            .map(|a| self.infer(a))
            .collect::<Result<Vec<_>, _>>()?;
        let (subst, _tyargs) =
            self.resolve_use_params(name, decls, param_args, params, &arg_tys)?;
        for (i, (aty, pty)) in arg_tys.iter().zip(params).enumerate() {
            let expected = self.resolve_assoc_ty(&substitute(pty, &subst));
            if !coerces(aty, &expected) {
                return Err(TypeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: aty.to_string(),
                    context: format!("argument {} to '{}'", i + 1, name),
                });
            }
        }
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
        if is_numeric(&tys[0]) {
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

    /// Type `round(x)`: a `Float64` argument, returning `Float64`.
    fn infer_round(&self, args: &[Expr]) -> Result<Ty, TypeError> {
        let tys = self.builtin_args("round", 1, args)?;
        if matches!(tys[0], Ty::Float64 | Ty::FloatLiteral) {
            Ok(Ty::Float64)
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

    /// Type a numeric conversion built-in `Int(x)` / `UInt(x)` / `Float64(x)`:
    /// exactly one argument of a numeric or `Bool` type, producing `target`.
    fn infer_conversion(&self, target: Ty, args: &[Expr]) -> Result<Ty, TypeError> {
        if args.len() != 1 {
            return Err(TypeError::ArityMismatch {
                name: target.to_string(),
                expected: 1,
                got: args.len(),
            });
        }
        let arg_ty = self.infer(&args[0])?;
        if !(is_numeric(&arg_ty) || arg_ty == Ty::Bool) {
            return Err(TypeError::TypeMismatch {
                expected: "a numeric or Bool value".to_string(),
                found: arg_ty.to_string(),
                context: format!("argument to '{}'", target),
            });
        }
        Ok(target)
    }
}

/// Mojo's built-in traits that mojo-lite recognizes in a type-parameter bound.
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
    "Divmodable",
];

/// Solve a type parameter against an actual type, recording it in `subst`. A
/// numeric literal is defaulted to its concrete type first (`IntLiteral → Int`,
/// `FloatLiteral → Float64`) so the solution matches the value the type-erased
/// evaluator produces — this deliberately forbids widening one literal to match
/// another across arguments (e.g. `Pair(1.0, 2)` is a conflict, not `Pair[Float64]`).
/// Whether an expression is a **place** — it names an existing binding (a variable
/// or a field/index chain rooted at one) rather than producing a fresh value. A
/// `^` transfer, a call result, a literal, or an operator is *not* a place.
fn is_place_expr(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Identifier(_) | ExprKind::Member { .. } | ExprKind::Index { .. }
    )
}

/// The root variable of a place expression (`p` for `p`, `p.a.b`, `p.items[i]`),
/// or `None` if the expression isn't rooted at a variable. A `mut`/shared borrow of
/// a place borrows its root, so the borrow checker keys on this.
/// Mojo's borrow rule (mutable-XOR-shared), checked per call and **place-sensitive**
/// (field-aware). An argument accesses its place either **exclusively** (a
/// `mut`/`ref` borrow, or a `^` move) or **shared** (a plain `read`/default borrow).
/// Any number of shared accesses to overlapping places is fine, but an exclusive
/// access requires no *overlapping* place elsewhere in the call — so `f(mut a, a)`,
/// `f(mut a, mut a)`, `f(a, a^)`, and `f(mut p, p.a)` are rejected, while
/// `f(mut p.a, mut p.b)` (disjoint fields) is allowed. mojo-lite's borrows are
/// call-scoped (no references persist in variables), so this per-call check is
/// complete — no cross-block loan dataflow is needed.
fn check_call_aliasing(
    slots: &[ArgSlot],
    conventions: &[Option<ArgConvention>],
    args: &[Expr],
    kwargs: &[crate::ast::KwArg],
) -> Result<(), TypeError> {
    // Each place argument's access: its full place (root + projection path) and
    // whether it is *exclusive* (a `mut`/`ref` borrow, or a `^` move).
    let mut accesses: Vec<(&str, Vec<PlaceSeg>, bool)> = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        let arg = match slot {
            ArgSlot::Positional(p) => &args[*p],
            ArgSlot::Keyword(k) => &kwargs[*k].value,
            ArgSlot::Default => continue,
        };
        let (place, exclusive) = match &arg.kind {
            ExprKind::Transfer(inner) => (place_path(inner), true),
            _ => (
                place_path(arg),
                matches!(
                    conventions.get(i),
                    Some(Some(ArgConvention::Mut | ArgConvention::Ref))
                ),
            ),
        };
        if let Some((root, path)) = place {
            accesses.push((root, path, exclusive));
        }
    }
    // Mutable-XOR-shared, **place-sensitive**: two accesses to the *same variable*
    // conflict only if their places overlap (a prefix relationship) and at least one
    // is exclusive. So `f(mut p.a, mut p.b)` is fine (disjoint fields), while
    // `f(mut p.a, p.a)` and `f(mut p, p.a)` are rejected.
    for i in 0..accesses.len() {
        for j in (i + 1)..accesses.len() {
            let (ra, pa, ea) = &accesses[i];
            let (rb, pb, eb) = &accesses[j];
            if ra == rb && (*ea || *eb) && places_overlap(pa, pb) {
                return Err(TypeError::AliasingViolation {
                    var: ra.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// One step of a place's projection path (used by the place-sensitive borrow
/// check). A dynamic `Index` is treated conservatively — it may alias any index.
enum PlaceSeg {
    Field(String),
    Index,
}

/// A place expression's root variable and projection path (root → leaf), or `None`
/// if it isn't rooted at a variable.
fn place_path(e: &Expr) -> Option<(&str, Vec<PlaceSeg>)> {
    fn go<'a>(e: &'a Expr, path: &mut Vec<PlaceSeg>) -> Option<&'a str> {
        match &e.kind {
            ExprKind::Identifier(n) => Some(n),
            ExprKind::Member { object, field } => {
                let r = go(object, path)?;
                path.push(PlaceSeg::Field(field.clone()));
                Some(r)
            }
            ExprKind::Index { object, .. } => {
                let r = go(object, path)?;
                path.push(PlaceSeg::Index);
                Some(r)
            }
            _ => None,
        }
    }
    let mut path = Vec::new();
    let root = go(e, &mut path)?;
    Some((root, path))
}

/// Whether two projection paths (of the same root) may refer to overlapping
/// memory: they overlap unless a `Field` step names distinct fields. A dynamic
/// `Index` conservatively may alias, so it never proves disjointness.
fn places_overlap(a: &[PlaceSeg], b: &[PlaceSeg]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if let (PlaceSeg::Field(fa), PlaceSeg::Field(fb)) = (x, y)
            && fa != fb
        {
            return false; // distinct fields are disjoint
        }
    }
    true // one path is a prefix of the other (or they diverge only at an index)
}

fn unify(pattern: &Ty, actual: &Ty, subst: &mut HashMap<String, Ty>) -> Result<(), TypeError> {
    match pattern {
        Ty::Param { name, .. } => {
            let solved = default_literal(actual);
            match subst.get(name) {
                None => {
                    subst.insert(name.clone(), solved);
                }
                Some(existing) if *existing == solved => {}
                Some(existing) => {
                    return Err(TypeError::TypeMismatch {
                        expected: existing.to_string(),
                        found: solved.to_string(),
                        context: format!("type parameter '{}'", name),
                    });
                }
            }
            Ok(())
        }
        // Recurse into a parameterized struct pattern to solve nested type
        // parameters (`Pair[T]` against `Pair[Int]` solves `T = Int`). Value
        // arguments contribute no type solution. A structural mismatch is left
        // for the caller's coercion check to report.
        Ty::Struct(pn, pargs) => {
            if let Ty::Struct(an, aargs) = actual
                && pn == an
                && pargs.len() == aargs.len()
            {
                for (p, a) in pargs.iter().zip(aargs) {
                    if let (TyArg::Ty(p), TyArg::Ty(a)) = (p, a) {
                        unify(p, a, subst)?;
                    }
                }
            }
            Ok(())
        }
        // A non-parameter pattern contributes no solution; coercion is checked
        // separately by the caller.
        _ => Ok(()),
    }
}

/// Replace every `Ty::Param` in `ty` with its solution from `subst` (leaving an
/// unsolved parameter untouched). Recurses into struct type arguments.
fn substitute(ty: &Ty, subst: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param { name, .. } => subst.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Struct(name, args) => {
            Ty::Struct(name.clone(), map_tyargs(args, |t| substitute(t, subst)))
        }
        Ty::List(elem) => Ty::List(Box::new(substitute(elem, subst))),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| substitute(t, subst)).collect()),
        Ty::Pointer(elem) => Ty::Pointer(Box::new(substitute(elem, subst))),
        Ty::Assoc { base, name } => Ty::Assoc {
            base: Box::new(substitute(base, subst)),
            name: name.clone(),
        },
        Ty::Func {
            params,
            names,
            ret,
            required,
            variadic,
            conventions,
        } => Ty::Func {
            params: params.iter().map(|p| substitute(p, subst)).collect(),
            names: names.clone(),
            ret: Box::new(substitute(ret, subst)),
            required: *required,
            variadic: variadic.as_ref().map(|v| Box::new(substitute(v, subst))),
            conventions: conventions.clone(),
        },
        _ => ty.clone(),
    }
}

/// Replace every `Ty::SelfType` in `ty` with `replacement` (the conforming
/// struct type, or a bounded `T`). Recurses into struct/function types.
fn substitute_self(ty: &Ty, replacement: &Ty) -> Ty {
    match ty {
        Ty::SelfType => replacement.clone(),
        Ty::Struct(name, args) => Ty::Struct(
            name.clone(),
            map_tyargs(args, |t| substitute_self(t, replacement)),
        ),
        Ty::List(elem) => Ty::List(Box::new(substitute_self(elem, replacement))),
        Ty::Tuple(elems) => Ty::Tuple(
            elems
                .iter()
                .map(|t| substitute_self(t, replacement))
                .collect(),
        ),
        Ty::Pointer(elem) => Ty::Pointer(Box::new(substitute_self(elem, replacement))),
        Ty::Assoc { base, name } => Ty::Assoc {
            base: Box::new(substitute_self(base, replacement)),
            name: name.clone(),
        },
        Ty::Func {
            params,
            names,
            ret,
            required,
            variadic,
            conventions,
        } => Ty::Func {
            params: params
                .iter()
                .map(|p| substitute_self(p, replacement))
                .collect(),
            names: names.clone(),
            ret: Box::new(substitute_self(ret, replacement)),
            required: *required,
            variadic: variadic
                .as_ref()
                .map(|v| Box::new(substitute_self(v, replacement))),
            conventions: conventions.clone(),
        },
        _ => ty.clone(),
    }
}

/// Apply `f` to each type argument of a struct's parameter list, passing value
/// arguments through unchanged.
fn map_tyargs(args: &[TyArg], mut f: impl FnMut(&Ty) -> Ty) -> Vec<TyArg> {
    args.iter()
        .map(|a| match a {
            TyArg::Ty(t) => TyArg::Ty(f(t)),
            TyArg::Val(v) => TyArg::Val(v.clone()),
        })
        .collect()
}

/// Whether a block **definitely returns** on every path — used to require that a
/// non-`None` function returns rather than falling off the end. Conservative: a
/// `return` returns; an `if` returns only when it has an `else` and *every* arm
/// returns; loops never count (they may run zero times, and `while True` is not
/// special-cased). A statement that returns makes the rest of its block dead, so
/// the block returns if any statement does.
fn definitely_returns(body: &[Stmt]) -> bool {
    body.iter().any(stmt_returns)
}

fn stmt_returns(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) => true,
        // A `raise` diverges (it never falls through to the end), so for
        // reachability it behaves like a `return`.
        StmtKind::Raise(_) => true,
        StmtKind::If { branches, orelse } => {
            orelse.as_ref().is_some_and(|e| definitely_returns(e))
                && branches.iter().all(|(_, b)| definitely_returns(b))
        }
        // A `try` definitely diverges when: a `finally` does (it overrides every
        // path); or the **normal-completion** path diverges (the body — or, if the
        // body may complete, the `else`) *and* the **exceptional** path does (every
        // `except` handler diverges; with no handler, an uncaught raise itself
        // exits, so only the normal path can fall through).
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            if finalbody.as_ref().is_some_and(|fb| definitely_returns(fb)) {
                return true;
            }
            let normal = match orelse {
                Some(else_) => definitely_returns(body) || definitely_returns(else_),
                None => definitely_returns(body),
            };
            let exceptional = match except {
                Some((_, handler)) => definitely_returns(handler),
                None => true,
            };
            normal && exceptional
        }
        _ => false,
    }
}

/// Extract a dtype from a `SIMD` first argument, which must be `DType.<name>`.
fn dtype_from_arg(arg: &crate::ast::ParamArg) -> Result<Dtype, TypeError> {
    if let crate::ast::ParamArg::Value(Expr {
        kind: ExprKind::Member { object, field },
        ..
    }) = arg
        && let ExprKind::Identifier(ns) = &object.kind
        && ns == "DType"
        && let Some(dtype) = Dtype::from_name(field)
    {
        return Ok(dtype);
    }
    Err(TypeError::BadDtype(match arg {
        crate::ast::ParamArg::Value(Expr {
            kind: ExprKind::Member { field, .. },
            ..
        }) => {
            format!("DType.{}", field)
        }
        _ => "a non-DType argument".to_string(),
    }))
}

/// Whether a value of type `ty` can be a `dtype` SIMD element (a construction
/// argument, or the non-SIMD operand of an elementwise operator that splats). A
/// numeric literal fits any matching-kind lane; a same-dtype width-1 SIMD fits.
fn splats_to(ty: &Ty, dtype: Dtype) -> bool {
    match ty {
        Ty::IntLiteral => dtype != Dtype::Bool,
        Ty::FloatLiteral => dtype.is_float(),
        Ty::Bool => dtype == Dtype::Bool,
        // `Float64` is `SIMD[DType.float64, 1]`, so it splats into a float64 vector.
        Ty::Float64 => dtype == Dtype::Float64,
        Ty::Simd { dtype: d, width: 1 } => *d == dtype,
        _ => false,
    }
}

/// The (canonicalized) `Ty` for a SIMD of `dtype`/`width`: a **width-1 `float64`**
/// is the native `Ty::Float64` (Mojo unifies `Float64` with `SIMD[DType.float64,
/// 1]`); everything else is a `Ty::Simd`.
fn simd_ty(dtype: Dtype, width: i64) -> Ty {
    if dtype == Dtype::Float64 && width == 1 {
        Ty::Float64
    } else {
        Ty::Simd { dtype, width }
    }
}

/// The scalar `Ty` a value-parameter type name denotes, or `None` if the name is
/// not a scalar type (so it is a trait, i.e. a type parameter). Used to classify
/// `[name: X]` as a value vs. type parameter.
fn scalar_type_name(name: &str) -> Option<Ty> {
    match name {
        "Int" => Some(Ty::Int),
        "UInt" => Some(Ty::UInt),
        "Bool" => Some(Ty::Bool),
        "String" => Some(Ty::String),
        "Float64" => Some(Ty::Float64),
        _ => None,
    }
}

/// The type-parameter scope (`name → bounds`) of a parameter list, for resolving
/// a bare `T` annotation. Value parameters are excluded (they are `Int` locals).
fn type_scope(decls: &[ParamDecl]) -> HashMap<String, Vec<String>> {
    decls
        .iter()
        .filter_map(|d| match d {
            ParamDecl::Type { name, bounds } => Some((name.clone(), bounds.clone())),
            ParamDecl::Value { .. } => None,
        })
        .collect()
}

/// A struct's own parameter, as the `TyArg` it contributes to the struct's `Self`
/// type while its body is checked: a type parameter as `Ty::Param`, a value
/// parameter as a symbolic `CtValue::Param`.
fn param_as_arg(decl: &ParamDecl) -> TyArg {
    match decl {
        ParamDecl::Type { name, bounds } => TyArg::Ty(Ty::Param {
            name: name.clone(),
            bounds: bounds.clone(),
        }),
        ParamDecl::Value { name } => TyArg::Val(CtValue::Param(name.clone())),
    }
}

/// The substitution mapping a struct's type-parameter names to a value's type
/// arguments (`[T] @ [Int]` ⟹ `{T: Int}`). Value parameters/arguments are
/// skipped (they never appear in a type). Empty for a non-generic struct.
fn struct_subst(decls: &[ParamDecl], targs: &[TyArg]) -> HashMap<String, Ty> {
    decls
        .iter()
        .zip(targs)
        .filter_map(|(d, a)| match (d, a) {
            (ParamDecl::Type { name, .. }, TyArg::Ty(t)) => Some((name.clone(), t.clone())),
            _ => None,
        })
        .collect()
}

/// Materialize a numeric literal type to its concrete default; other types pass
/// through unchanged. Used when solving a type parameter to a literal argument.
/// Where a parameter slot's value comes from after matching a call's arguments.
#[derive(Clone, Copy)]
pub(crate) enum ArgSlot {
    /// The positional argument at this index.
    Positional(usize),
    /// The keyword argument at this index (into the call's `kwargs`).
    Keyword(usize),
    /// No argument supplied — use the parameter's default value.
    Default,
}

/// An argument/parameter mismatch from `match_call_slots`, mapped by each stage
/// into its own error type (`TypeError` / `RuntimeError`).
pub(crate) enum MatchError {
    TooManyPositional { expected: usize, got: usize },
    UnknownKeyword(String),
    Duplicate(String),
    Missing(String),
}

impl MatchError {
    pub(crate) fn into_type_error(self, func: &str) -> TypeError {
        match self {
            MatchError::TooManyPositional { expected, got } => TypeError::ArityMismatch {
                name: func.to_string(),
                expected,
                got,
            },
            MatchError::UnknownKeyword(k) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("unexpected keyword argument '{}'", k),
            },
            MatchError::Duplicate(k) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("argument '{}' supplied more than once", k),
            },
            MatchError::Missing(m) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("missing required argument '{}'", m),
            },
        }
    }

    /// The runtime analogue, for the evaluator (the checker normally catches these
    /// first, so this is a safety net rather than the usual path).
    pub(crate) fn into_runtime_error(self, func: &str) -> crate::error::RuntimeError {
        use crate::error::RuntimeError;
        match self {
            MatchError::TooManyPositional { expected, got } => RuntimeError::ArityMismatch {
                name: func.to_string(),
                expected,
                got,
            },
            MatchError::UnknownKeyword(k) => RuntimeError::TypeError(format!(
                "'{}' got an unexpected keyword argument '{}'",
                func, k
            )),
            MatchError::Duplicate(k) => {
                RuntimeError::TypeError(format!("'{}' got argument '{}' more than once", func, k))
            }
            MatchError::Missing(m) => {
                RuntimeError::TypeError(format!("'{}' missing required argument '{}'", func, m))
            }
        }
    }
}

/// Match a call's positional + keyword arguments to a callee's **regular**
/// parameter slots, returning where each parameter's value comes from plus the
/// indices of any extra positional arguments (which a `*args` parameter collects;
/// only possible when `has_variadic`). Shared by the checker (over argument
/// *types*) and the evaluator (over argument *values*) so the two agree.
/// `required` is the number of leading regular parameters that must be bound.
pub(crate) fn match_call_slots(
    param_names: &[String],
    nparams: usize,
    required: usize,
    npos: usize,
    kw_names: &[&str],
    has_variadic: bool,
) -> Result<(Vec<ArgSlot>, Vec<usize>), MatchError> {
    if npos > nparams && !has_variadic {
        return Err(MatchError::TooManyPositional {
            expected: nparams,
            got: npos + kw_names.len(),
        });
    }
    // Positional arguments beyond the regular parameters overflow into `*args`.
    let n_regular_pos = npos.min(nparams);
    let overflow: Vec<usize> = (nparams..npos).collect();
    let mut slots: Vec<Option<ArgSlot>> = vec![None; nparams];
    for (i, s) in slots.iter_mut().enumerate().take(n_regular_pos) {
        *s = Some(ArgSlot::Positional(i));
    }
    for (j, kwname) in kw_names.iter().enumerate() {
        let idx = param_names
            .iter()
            .position(|n| n == kwname)
            .ok_or_else(|| MatchError::UnknownKeyword((*kwname).to_string()))?;
        if slots[idx].is_some() {
            return Err(MatchError::Duplicate((*kwname).to_string()));
        }
        slots[idx] = Some(ArgSlot::Keyword(j));
    }
    let mut out = Vec::with_capacity(nparams);
    for (i, s) in slots.into_iter().enumerate() {
        match s {
            Some(slot) => out.push(slot),
            None if i < required => return Err(MatchError::Missing(param_names[i].clone())),
            None => out.push(ArgSlot::Default),
        }
    }
    Ok((out, overflow))
}

/// The number of **required** parameters (those with no default). Defaults must
/// be trailing, so a required parameter after a defaulted one is an error.
fn lifecycle_method_name(m: &Method) -> &str {
    if is_mojo_copy_constructor(m) {
        "__copyinit__"
    } else {
        &m.name
    }
}

fn is_mojo_copy_constructor(m: &Method) -> bool {
    m.name == "__init__"
        && m.has_self
        && matches!(m.self_convention, Some(ArgConvention::Out))
        && m.positional_only.is_none()
        && m.keyword_only == Some(0)
        && m.params.len() == 1
        && is_copy_param(&m.params[0])
        && m.ret.is_none()
}

fn is_copy_param(p: &FnParam) -> bool {
    p.name == "copy"
        && p.default.is_none()
        && p.kind == crate::ast::ParamKind::Regular
        && p.convention.is_none()
        && matches!(p.ty, Type::SelfType)
}

fn required_count(params: &[crate::ast::FnParam]) -> Result<usize, TypeError> {
    let mut seen_default = false;
    let mut required = 0;
    for p in params {
        if p.default.is_some() {
            seen_default = true;
        } else if seen_default {
            return Err(TypeError::Unsupported(format!(
                "a required parameter ('{}') cannot follow one with a default value",
                p.name
            )));
        } else {
            required += 1;
        }
    }
    Ok(required)
}

/// A call using keyword arguments (`name=value`) is parsed but not implemented.
fn reject_kwargs(kwargs: &[crate::ast::KwArg]) -> Result<(), TypeError> {
    if kwargs.is_empty() {
        Ok(())
    } else {
        Err(TypeError::Unsupported("keyword arguments".to_string()))
    }
}

fn default_literal(ty: &Ty) -> Ty {
    match ty {
        Ty::IntLiteral => Ty::Int,
        Ty::FloatLiteral => Ty::Float64,
        // Materialize each element of a tuple literal (`(1, 2)` → `Tuple[Int, Int]`).
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(default_literal).collect()),
        other => other.clone(),
    }
}

/// Whether `ty` is a non-numeric scalar value type — what `==`/`!=` compare once
/// the numeric cases (handled by `common_numeric`) are out of the way.
fn is_scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Bool | Ty::String | Ty::None)
}

/// Whether an opaque type parameter carries a bound that promises equality.
/// Built-in bounds are intentionally shallow today, but `T: Equatable` should at
/// least let generic library code type-check `T == T`.
fn has_equality_bound(ty: &Ty) -> bool {
    match ty {
        Ty::Param { bounds, .. } => bounds.iter().any(|b| {
            matches!(
                b.as_str(),
                "Equatable" | "Comparable" | "Hashable" | "EqualityComparable"
            )
        }),
        _ => false,
    }
}

/// Collect the field names assigned via `self.FIELD = …` anywhere in a body
/// (recursing into nested `if`/`while`/`for`/`try` blocks) — the flow-insensitive
/// basis of the `__init__` definite-initialization check. A nested write like
/// `self.a.b = e` does *not* count as initializing `a` (its object isn't `self`).
fn collect_self_assigned_fields(body: &[Stmt], out: &mut std::collections::HashSet<String>) {
    for stmt in body {
        match &stmt.kind {
            StmtKind::SetPlace { place, .. } => {
                if let ExprKind::Member { object, field } = &place.kind
                    && matches!(&object.kind, ExprKind::Identifier(n) if n == "self")
                {
                    out.insert(field.clone());
                }
            }
            StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    collect_self_assigned_fields(b, out);
                }
                if let Some(b) = orelse {
                    collect_self_assigned_fields(b, out);
                }
            }
            StmtKind::While { body, .. } | StmtKind::For { body, .. } => {
                collect_self_assigned_fields(body, out);
            }
            StmtKind::Try {
                body,
                except,
                orelse,
                finalbody,
            } => {
                collect_self_assigned_fields(body, out);
                if let Some((_, b)) = except {
                    collect_self_assigned_fields(b, out);
                }
                if let Some(b) = orelse {
                    collect_self_assigned_fields(b, out);
                }
                if let Some(b) = finalbody {
                    collect_self_assigned_fields(b, out);
                }
            }
            _ => {}
        }
    }
}

/// Enforce that a builtin-driven dunder (`__len__`/`__str__`/`__contains__`)
/// returns its Mojo-mandated type, so `len`/`String`/`in` on a user struct stay
/// well-typed.
fn require_dunder_ret(ret: Ty, expected: &Ty, name: &str) -> Result<Ty, TypeError> {
    if ret == *expected {
        Ok(ret)
    } else {
        Err(TypeError::TypeMismatch {
            expected: expected.to_string(),
            found: ret.to_string(),
            context: format!("return type of '{name}'"),
        })
    }
}

/// Whether list elements of type `ty` can be compared for equality (needed by
/// `List.remove`/`count`/`index`) — the same scalar set `==`/`!=` accept.
fn is_list_equatable(ty: &Ty) -> bool {
    is_numeric(ty) || matches!(ty, Ty::Bool | Ty::String | Ty::None) || has_equality_bound(ty)
}

/// Whether a value of type `ty` can be `print`ed (has a user-facing display).
/// Functions, ranges, and opaque type parameters are not printable.
fn is_printable(ty: &Ty) -> bool {
    match ty {
        Ty::Int
        | Ty::UInt
        | Ty::Bool
        | Ty::String
        | Ty::Float64
        | Ty::None
        | Ty::IntLiteral
        | Ty::FloatLiteral
        | Ty::Struct(_, _)
        | Ty::Simd { .. }
        | Ty::Error
        | Ty::List(_) => true,
        // A tuple prints if every element prints.
        Ty::Tuple(elems) => elems.iter().all(is_printable),
        _ => false,
    }
}

/// Whether `ty` is a numeric type (concrete or literal).
fn is_numeric(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Int | Ty::UInt | Ty::Float64 | Ty::IntLiteral | Ty::FloatLiteral
    )
}

/// Whether a value of type `from` can be used where `to` is required. Only the
/// literal types coerce (to the concrete numeric types, or `IntLiteral` up to
/// `FloatLiteral`); everything else must match exactly.
fn coerces(from: &Ty, to: &Ty) -> bool {
    if from == to {
        return true;
    }
    match (from, to) {
        (Ty::Param { name: a, .. }, Ty::Param { name: b, .. }) => a == b,
        (Ty::Struct(an, aargs), Ty::Struct(bn, bargs)) => {
            an == bn
                && aargs.len() == bargs.len()
                && aargs.iter().zip(bargs).all(|(a, b)| match (a, b) {
                    (TyArg::Ty(a), TyArg::Ty(b)) => coerces(a, b),
                    (TyArg::Val(a), TyArg::Val(b)) => a == b,
                    _ => false,
                })
        }
        (Ty::List(a), Ty::List(b)) => coerces(a, b),
        (Ty::Pointer(a), Ty::Pointer(b)) => coerces(a, b),
        (Ty::IntLiteral, Ty::Int | Ty::UInt | Ty::Float64 | Ty::FloatLiteral) => true,
        (Ty::FloatLiteral, Ty::Float64) => true,
        // A tuple coerces element-wise (same arity) — so a literal element
        // materializes: `(1, 2.0)` fits `Tuple[Float64, Float64]`.
        (Ty::Tuple(a), Ty::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| coerces(x, y))
        }
        _ => false,
    }
}

/// The common type of two list elements: numeric elements unify like operands
/// (widening literals); otherwise the two must be equal.
fn common_elem(a: &Ty, b: &Ty) -> Option<Ty> {
    if is_numeric(a) && is_numeric(b) {
        common_numeric(a, b)
    } else if a == b {
        Some(a.clone())
    } else {
        None
    }
}

/// The common type of two numeric operands, coercing literals as needed, or
/// `None` if they can't be unified (e.g. two different concrete types).
/// The common type of a ternary's two branches: unify numerics (widening
/// literals), else an exact match or a one-way literal coercion. `None` if the
/// branches are incompatible.
fn common_branch_ty(a: &Ty, b: &Ty) -> Option<Ty> {
    if let Some(c) = common_numeric(a, b) {
        return Some(c);
    }
    if a == b {
        Some(a.clone())
    } else if coerces(a, b) {
        Some(b.clone())
    } else if coerces(b, a) {
        Some(a.clone())
    } else {
        None
    }
}

fn common_numeric(a: &Ty, b: &Ty) -> Option<Ty> {
    if !is_numeric(a) || !is_numeric(b) {
        return None;
    }
    if a == b {
        Some(a.clone())
    } else if coerces(a, b) {
        Some(b.clone())
    } else if coerces(b, a) {
        Some(a.clone())
    } else {
        None
    }
}

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
