//! Canonical overload identity and lowered-symbol formatting.
//!
//! This module is the **single owner** of the `$ov$` signature-qualified name
//! scheme that overload resolution lowers to. The checker records a resolved
//! callee per call span, the MIR names each overloaded `def`/method, and the VM
//! looks both up — all three must agree on the exact spelling, so none of them
//! may assemble or inspect these strings directly. A new hand-built overload
//! symbol elsewhere in `src/` is a bug (`tests/symbol_test.rs` scans for it).
//!
//! An overload signature is represented as typed data (`SignatureKey`, a list of
//! `TypeKey`s) before it is ever formatted. A `TypeKey` can be built from two
//! worlds and **must produce the same spelling for the same source annotation**:
//!
//! - [`TypeKey::from_ast`] — the declared `ast::Type` (MIR/VM lowering names
//!   each overloaded definition from its parameter annotations).
//! - [`TypeKey::from_ty`] — the checker's resolved `types::Ty` (the checker
//!   records the selected callee from the winning signature's parameter types).
//!
//! Definition-side value arguments are folded with the same integer operations
//! as the checker before formatting, so `FixedBuffer[N]` and `FixedBuffer[2+6]`
//! name the same `FixedBuffer[8]` specialization selected at a call site.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArgConvention, Expr, ExprKind, FnParam, Method, ParamArg, ParamKind, Stmt, StmtKind, Type,
    TypeParam,
};
use crate::types::{Ty, TyArg};

/// The separator that marks a signature-qualified overload symbol:
/// `pick$ov$Int`, `Box.__init__$ov$String`. Never referenced outside this
/// module.
const OV_SEP: &str = "$ov$";

/// The canonical mangled spelling of one parameter type. Only this module can
/// construct one, so every signature part obeys the same sanitization rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeKey(String);

impl TypeKey {
    /// Mangle a declared parameter annotation (the MIR/VM definition side).
    pub fn from_ast(ty: &Type) -> TypeKey {
        TypeKey(sanitize(&ast_raw(ty, &HashMap::new(), &HashMap::new())))
    }

    /// Mangle a checker-resolved type (the call-resolution side). Aligned with
    /// [`TypeKey::from_ast`]: a struct/parameter/`Self.T` type spells exactly as
    /// its annotation does, so checker-recorded callees name real MIR functions.
    pub fn from_ty(ty: &Ty) -> TypeKey {
        TypeKey(sanitize(&ty_raw(ty)))
    }
}

fn ast_raw(
    ty: &Type,
    comptimes: &HashMap<String, i64>,
    type_bounds: &HashMap<String, Vec<String>>,
) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::UInt => "UInt".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::String => "String".to_string(),
        Type::Float64 => "Float64".to_string(),
        Type::None => "None".to_string(),
        Type::Named(name, args) => {
            let mut s = parameter_raw(name, type_bounds);
            for arg in args {
                s.push('$');
                match arg {
                    ParamArg::Type(t) => s.push_str(&ast_raw(t, comptimes, type_bounds)),
                    ParamArg::Value(v) => {
                        s.push('V');
                        s.push_str(&value_expr_raw(v, comptimes));
                    }
                    ParamArg::Named { name, value } => {
                        s.push_str(name);
                        s.push('=');
                        match &**value {
                            ParamArg::Type(t) => s.push_str(&ast_raw(t, comptimes, type_bounds)),
                            ParamArg::Value(v) => s.push_str(&value_expr_raw(v, comptimes)),
                            ParamArg::Named { .. } => unreachable!(),
                        }
                    }
                }
            }
            s
        }
        // `Self.T` names the same parameter a bare `T` does inside the struct,
        // and the checker resolves both to the same `Ty::Param` — spell them
        // identically so the two sides agree.
        Type::SelfParam(name) => parameter_raw(name, type_bounds),
        Type::Assoc { base, name } => format!(
            "Assoc${}${}",
            ast_raw(base, comptimes, type_bounds),
            encode_identifier(name)
        ),
        Type::SelfType => "Self".to_string(),
        other => format!("{other:?}"),
    }
}

fn ty_raw(ty: &Ty) -> String {
    match ty {
        Ty::Int | Ty::IntLiteral => "Int".to_string(),
        Ty::UInt => "UInt".to_string(),
        Ty::Float64 | Ty::FloatLiteral => "Float64".to_string(),
        Ty::Bool => "Bool".to_string(),
        Ty::String => "String".to_string(),
        Ty::None => "None".to_string(),
        Ty::List(elem) => format!("List${}", ty_raw(elem)),
        Ty::Tuple(elems) => format!(
            "Tuple${}",
            elems.iter().map(ty_raw).collect::<Vec<_>>().join("$")
        ),
        Ty::Variant(alternatives) => format!(
            "Variant${}",
            alternatives
                .iter()
                .map(ty_raw)
                .collect::<Vec<_>>()
                .join("$")
        ),
        // A struct type spells as its annotation does (`Point`, `Pair$Int`) —
        // no `Struct$` marker, so the MIR definition name matches.
        Ty::Struct(name, args) => {
            let mut s = encode_identifier(name);
            for arg in args {
                s.push('$');
                match arg {
                    TyArg::Ty(t) => s.push_str(&ty_raw(t)),
                    TyArg::Val(v) => s.push_str(&format!("V{v}")),
                }
            }
            s
        }
        // A type parameter spells as the bare annotation `T` does.
        Ty::Param { name, bounds } => {
            let mut result = encode_identifier(name);
            for bound in bounds {
                result.push('$');
                result.push_str(&encode_identifier(bound));
            }
            result
        }
        // Pointer origins affect checking/lifetimes but erase from the runtime
        // callable ABI, just like origin parameters on `ref` arguments.
        Ty::Pointer { element, .. } => format!("UnsafePointer${}", ty_raw(element)),
        Ty::Assoc { base, name } => {
            format!("Assoc${}${}", ty_raw(base), encode_identifier(name))
        }
        Ty::SelfType => "Self".to_string(),
        other => other.to_string(),
    }
}

fn parameter_raw(name: &str, type_bounds: &HashMap<String, Vec<String>>) -> String {
    let mut result = encode_identifier(name);
    if let Some(bounds) = type_bounds.get(name) {
        for bound in bounds {
            result.push('$');
            result.push_str(&encode_identifier(bound));
        }
    }
    result
}

/// The mangled spelling of a compile-time value argument in an annotation
/// (`FixedBuffer[8]` → `8`). A non-literal expression degrades to a stable
/// placeholder — good enough because the name only needs to be deterministic.
fn value_expr_raw(expr: &Expr, comptimes: &HashMap<String, i64>) -> String {
    if let Some(value) = eval_comptime_int(expr, comptimes) {
        return value.to_string();
    }
    match &expr.kind {
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::Str(s) => encode_identifier(s),
        ExprKind::Identifier(name) => encode_identifier(name),
        _ => "expr".to_string(),
    }
}

fn eval_comptime_int(expr: &Expr, comptimes: &HashMap<String, i64>) -> Option<i64> {
    use crate::ast::{InfixOp, PrefixOp};
    match &expr.kind {
        ExprKind::Int(value) => Some(*value),
        ExprKind::Identifier(name) => comptimes.get(name).copied(),
        ExprKind::Prefix(PrefixOp::Neg, value) => {
            eval_comptime_int(value, comptimes)?.checked_neg()
        }
        ExprKind::Infix(op, left, right) => {
            let (left, right) = (
                eval_comptime_int(left, comptimes)?,
                eval_comptime_int(right, comptimes)?,
            );
            match op {
                InfixOp::Add => left.checked_add(right),
                InfixOp::Sub => left.checked_sub(right),
                InfixOp::Mul => left.checked_mul(right),
                InfixOp::FloorDiv if right != 0 => Some(left.div_euclid(right)),
                InfixOp::Mod if right != 0 => Some(left.rem_euclid(right)),
                InfixOp::Pow if right >= 0 => left.checked_pow(right as u32),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Encode source-controlled identifier text injectively while leaving ordinary
/// ASCII identifiers unchanged. Structural `$` separators are added only after
/// this encoding, so stropped names such as `A-B` and `A_B` cannot collide.
fn encode_identifier(name: &str) -> String {
    let mut encoded = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            encoded.push(ch);
        } else {
            encoded.push_str(&format!("$u{:X}$", ch as u32));
        }
    }
    encoded
}

fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '$' })
        .collect()
}

/// An overload signature as typed data: the ordered parameter type keys.
/// Format it only through [`function_symbol`]/[`method_symbol`] (or the
/// `lowered_*` helpers below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureKey(Vec<TypeKey>);

impl SignatureKey {
    /// The signature of a declared `def`/method parameter list.
    pub fn from_ast_params(params: &[FnParam]) -> SignatureKey {
        SignatureKey(params.iter().map(|p| TypeKey::from_ast(&p.ty)).collect())
    }

    /// The signature of a checker-resolved parameter-type list.
    pub fn from_tys<'a>(tys: impl IntoIterator<Item = &'a Ty>) -> SignatureKey {
        SignatureKey(tys.into_iter().map(TypeKey::from_ty).collect())
    }

    fn suffix(&self) -> String {
        let parts = self
            .0
            .iter()
            .map(|k| k.0.as_str())
            .collect::<Vec<_>>()
            .join("$");
        format!("{OV_SEP}{parts}")
    }

    fn with_receiver(&self, convention: Option<ArgConvention>) -> SignatureKey {
        let receiver = match convention {
            None | Some(ArgConvention::Read) => "SelfRead",
            Some(ArgConvention::Mut) => "SelfMut",
            Some(ArgConvention::Var) => "SelfVar",
            Some(ArgConvention::Out) => "SelfOut",
            Some(ArgConvention::Ref) => "SelfRef",
            Some(ArgConvention::Deinit) => "SelfDeinit",
        };
        let mut parts = vec![TypeKey(receiver.to_string())];
        parts.extend(self.0.iter().cloned());
        SignatureKey(parts)
    }
}

/// The lowered symbol of an overloaded free function: `pick$ov$Int`.
pub fn function_symbol(base: &str, sig: &SignatureKey) -> String {
    format!("{base}{}", sig.suffix())
}

/// The lowered symbol of an overloaded struct method (including `__init__` and
/// the other lifecycle methods): `Box.value$ov$Int`.
pub fn method_symbol(type_name: &str, method: &str, sig: &SignatureKey) -> String {
    format!("{type_name}.{method}{}", sig.suffix())
}

/// Convention-qualified symbol for `__iter__` overloads. Current Mojo permits
/// borrowed and owned `__iter__` methods with identical explicit parameters;
/// receiver convention is therefore part of this method's callable identity.
pub fn iterator_method_symbol(
    type_name: &str,
    convention: Option<ArgConvention>,
    sig: &SignatureKey,
) -> String {
    method_symbol(type_name, "__iter__", &sig.with_receiver(convention))
}

/// Retarget a checker-selected method symbol from an abstract receiver (for
/// example `__trait_dispatch.pick$ov$Int`) to the concrete runtime type while
/// preserving the exact selected method/signature suffix. Keeping this parsing
/// here preserves this module's ownership of the overload encoding.
pub fn retarget_method_symbol(symbol: &str, type_name: &str) -> Option<String> {
    let (_, method_and_signature) = symbol.rsplit_once('.')?;
    Some(format!("{type_name}.{method_and_signature}"))
}

/// The overloaded declarations of a program, scanned from its top level: which
/// free-function names and `Type.method` names have more than one definition
/// (and at which arities). Definitions of non-overloaded names keep their plain
/// source name, so lowering consults this before qualifying anything.
#[derive(Debug, Default, Clone)]
pub struct OverloadSets {
    functions: HashMap<String, HashSet<usize>>,
    all_functions: HashSet<String>,
    methods: HashMap<String, HashSet<usize>>,
    comptimes: HashMap<String, i64>,
}

impl OverloadSets {
    pub fn scan(program: &[Stmt]) -> OverloadSets {
        let mut functions: HashMap<String, Vec<usize>> = HashMap::new();
        let mut methods: HashMap<String, Vec<usize>> = HashMap::new();
        let mut comptimes = HashMap::new();
        for stmt in program {
            match &stmt.kind {
                StmtKind::Comptime { name, value, .. } => {
                    if let Some(value) = eval_comptime_int(value, &comptimes) {
                        comptimes.insert(name.clone(), value);
                    }
                }
                StmtKind::Def { name, params, .. } => {
                    functions
                        .entry(name.clone())
                        .or_default()
                        .push(params.len());
                }
                StmtKind::Struct {
                    name, methods: ms, ..
                } => {
                    for method in ms {
                        let method_name = lifecycle_method_name(method);
                        methods
                            .entry(format!("{name}.{method_name}"))
                            .or_default()
                            .push(method.params.len());
                    }
                }
                _ => {}
            }
        }
        OverloadSets {
            all_functions: functions.keys().cloned().collect(),
            functions: keep_overloaded(functions),
            methods: keep_overloaded(methods),
            comptimes,
        }
    }

    /// Whether free function `name` is overloaded and defines arity `arity`.
    pub fn function_is_overloaded(&self, name: &str, arity: usize) -> bool {
        self.functions
            .get(name)
            .is_some_and(|arities| arities.contains(&arity))
    }

    /// Whether `name` denotes any linked free-function declaration. MIR uses
    /// this to distinguish a function value from a non-local runtime name.
    pub fn is_function(&self, name: &str) -> bool {
        self.all_functions.contains(name)
    }

    /// Whether method `source_name` (`Type.method`) is overloaded and defines
    /// arity `arity` (`self` excluded, matching the declared parameter list).
    pub fn method_is_overloaded(&self, source_name: &str, arity: usize) -> bool {
        self.methods
            .get(source_name)
            .is_some_and(|arities| arities.contains(&arity))
    }
}

fn keep_overloaded(counts: HashMap<String, Vec<usize>>) -> HashMap<String, HashSet<usize>> {
    counts
        .into_iter()
        .filter_map(|(name, arities)| {
            if arities.len() > 1 {
                Some((name, arities.into_iter().collect()))
            } else {
                None
            }
        })
        .collect()
}

/// The name a top-level `def` lowers to: signature-qualified when the name is
/// overloaded, the plain source name otherwise.
pub fn lowered_def_name(
    name: &str,
    type_params: &[TypeParam],
    params: &[FnParam],
    sets: &OverloadSets,
) -> String {
    if sets.function_is_overloaded(name, params.len()) {
        function_symbol(
            name,
            &signature_from_ast(params, type_params, &sets.comptimes),
        )
    } else {
        name.to_string()
    }
}

/// The name a struct method lowers to, from its already-joined source name
/// (`Type.method`): signature-qualified when overloaded, unchanged otherwise.
pub fn lowered_method_name(
    source_name: &str,
    type_params: &[TypeParam],
    params: &[FnParam],
    self_convention: Option<ArgConvention>,
    sets: &OverloadSets,
) -> String {
    if sets.method_is_overloaded(source_name, params.len()) {
        let signature = signature_from_ast(params, type_params, &sets.comptimes);
        if let Some(type_name) = source_name.strip_suffix(".__iter__") {
            iterator_method_symbol(type_name, self_convention, &signature)
        } else {
            format!("{source_name}{}", signature.suffix())
        }
    } else {
        source_name.to_string()
    }
}

fn signature_from_ast(
    params: &[FnParam],
    type_params: &[TypeParam],
    comptimes: &HashMap<String, i64>,
) -> SignatureKey {
    let type_bounds = type_params
        .iter()
        .map(|param| (param.name.clone(), param.bounds.clone()))
        .collect();
    SignatureKey(
        params
            .iter()
            .map(|param| TypeKey(sanitize(&ast_raw(&param.ty, comptimes, &type_bounds))))
            .collect(),
    )
}

/// The name a method is *registered and counted* under: current Mojo spells the
/// copy constructor as an `__init__` overload with an `out self, copy: Self`
/// shape, which the whole pipeline models as `__copyinit__`.
pub fn lifecycle_method_name(m: &Method) -> &str {
    if is_mojo_copy_constructor(m) {
        "__copyinit__"
    } else if is_mojo_move_constructor(m) {
        "__moveinit__"
    } else {
        &m.name
    }
}

fn is_mojo_move_constructor(m: &Method) -> bool {
    m.name == "__init__"
        && m.has_self
        && matches!(m.self_convention, Some(ArgConvention::Out))
        && m.positional_only.is_none()
        && m.keyword_only == Some(0)
        && m.params.len() == 1
        && m.params[0].name == "move"
        && m.params[0].default.is_none()
        && m.params[0].kind == ParamKind::Regular
        && m.params[0].convention.is_none()
        && matches!(m.params[0].ty, Type::SelfType)
        && m.ret.is_none()
}

fn is_mojo_copy_constructor(m: &Method) -> bool {
    m.name == "__init__"
        && m.has_self
        && matches!(m.self_convention, Some(ArgConvention::Out))
        && m.positional_only.is_none()
        && m.keyword_only == Some(0)
        && m.params.len() == 1
        && m.params[0].name == "copy"
        && m.params[0].default.is_none()
        && m.params[0].kind == ParamKind::Regular
        && m.params[0].convention.is_none()
        && matches!(m.params[0].ty, Type::SelfType)
        && m.ret.is_none()
}

/// The lifted name of a nested `def` (`inner` declared inside `outer`).
pub fn nested_lifted_name(outer: &str, inner: &str) -> String {
    format!("{outer}${inner}")
}

/// A deliberate **poison name** for an overloaded call the checker recorded no
/// target for (only reachable off the checked path): it can never name a real
/// function, so the VM reports it instead of guessing among overloads.
pub fn unresolved_overload_marker(name: &str, argc: usize) -> String {
    format!("{name}#{argc}")
}

/// Whether `symbol` is a signature-qualified overload of source name `base`
/// (used by the VM's arity fallback to enumerate an overload set).
pub fn is_overload_of(symbol: &str, base: &str) -> bool {
    symbol
        .strip_prefix(base)
        .is_some_and(|rest| rest.starts_with(OV_SEP))
}

/// If `symbol` is a signature-qualified `__init__` overload (`Type.__init__$ov$…`),
/// the struct it constructs.
pub fn init_overload_struct(symbol: &str) -> Option<&str> {
    let (struct_name, rest) = symbol.rsplit_once(".__init__")?;
    rest.starts_with(OV_SEP).then_some(struct_name)
}
