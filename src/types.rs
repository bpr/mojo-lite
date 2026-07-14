//! Shared semantic type representation.
//!
//! This is the type lattice used by the checker, but it also needs to be visible
//! to compile-time values once comptime can carry type values. Keeping `Ty` out
//! of `checker.rs` lets [`CtValue`](crate::ct::CtValue) represent `Type(Box<Ty>)`
//! without making the checker the owner of all type-level facts.

use std::fmt;

use crate::ast::{ArgConvention, Dtype};
use crate::ct::CtValue;

/// A type in mojito's semantic lattice. Scalars mirror `ast::Type`; `Func` is
/// synthesized from a `def` signature. The annotation grammar has no function
/// types yet, so `Func` only ever arises from a `def`, never from an annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Int,
    UInt,
    Bool,
    String,
    Float64,
    None,
    /// The flexible type of an integer literal: coerces to `Int`, `UInt`, or
    /// `Float64` (materializing to `Int` if nothing forces a choice).
    IntLiteral,
    /// The flexible type of a float literal: coerces to `Float64`.
    FloatLiteral,
    /// A non-generic function. `params`/`names` describe the regular parameters;
    /// `required[i]` is true when regular parameter `i` has no default. The
    /// marker fields are indexes into this regular-parameter list.
    Func {
        params: Vec<Ty>,
        names: Vec<String>,
        ret: Box<Ty>,
        required: Vec<bool>,
        variadic: Option<Box<Ty>>,
        positional_only: Option<usize>,
        keyword_only: Option<usize>,
        /// The argument convention of each regular parameter.
        conventions: Vec<Option<ArgConvention>>,
        ref_params: Box<Vec<Option<crate::origin::RefSig>>>,
        ref_return: Option<Box<crate::origin::RefSig>>,
    },
    /// A generic function synthesized from a `def` with a `[params]` list.
    GenericFunc {
        decls: Vec<ParamDecl>,
        params: Vec<Ty>,
        names: Vec<String>,
        ret: Box<Ty>,
        required: Vec<bool>,
        variadic: Option<Box<Ty>>,
        positional_only: Option<usize>,
        keyword_only: Option<usize>,
        conventions: Vec<Option<ArgConvention>>,
        ref_params: Box<Vec<Option<crate::origin::RefSig>>>,
        ref_return: Option<Box<crate::origin::RefSig>>,
    },
    /// A source name that denotes multiple callable signatures. The checker
    /// resolves an overload set at each call site. The first implementation
    /// supports distinct call shapes/arity; keeping this as a first-class type
    /// leaves type-ranked overload resolution as a natural extension.
    Overload(Vec<Ty>),
    /// A type parameter (`T`) inside a generic body, carrying its trait bounds.
    Param {
        name: String,
        bounds: Vec<String>,
    },
    /// A symbolic associated type lookup such as `C.Element` where `C` is an
    /// opaque type parameter. It may resolve to a concrete type once `C` is
    /// substituted at a generic use site.
    Assoc {
        base: Box<Ty>,
        name: String,
    },
    /// `Self` inside a trait method requirement.
    SelfType,
    /// The iterable produced by the built-in `range(...)`.
    Range,
    /// A nominal struct type, named, with its parameter arguments.
    Struct(String, Vec<TyArg>),
    /// A SIMD vector type `SIMD[DType.<dtype>, width]`.
    Simd {
        dtype: Dtype,
        width: i64,
    },
    /// The built-in `Error` type.
    Error,
    /// The built-in `List[T]` collection type.
    List(Box<Ty>),
    /// The built-in `Tuple[T1, ..., Tn]`.
    Tuple(Vec<Ty>),
    /// The built-in `UnsafePointer[T]`.
    Pointer(Box<Ty>),
    /// A reference value. Origins and permissions are checked statically; its
    /// runtime representation is introduced only after loan checking exists.
    Ref(crate::origin::RefTy),
}

/// A declared compile-time parameter of a generic `struct`/`def`, classified
/// from `[name: X]` by whether `X` is a trait or a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamDecl {
    /// A type parameter `T: Trait & ...`.
    Type { name: String, bounds: Vec<String> },
    /// A value parameter `n: Int`.
    Value { name: String },
}

impl ParamDecl {
    pub fn name(&self) -> &str {
        match self {
            ParamDecl::Type { name, .. } | ParamDecl::Value { name } => name,
        }
    }
}

/// One argument in a struct type's parameter list: a type or a compile-time
/// value. Part of a struct type's identity, so `FixedBuffer[8] != FixedBuffer[9]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TyArg {
    Ty(Ty),
    Val(CtValue),
}

impl fmt::Display for TyArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TyArg::Ty(t) => write!(f, "{}", t),
            TyArg::Val(v) => write!(f, "{}", v),
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int | Ty::IntLiteral => write!(f, "Int"),
            Ty::UInt => write!(f, "UInt"),
            Ty::Bool => write!(f, "Bool"),
            Ty::String => write!(f, "String"),
            Ty::Float64 | Ty::FloatLiteral => write!(f, "Float64"),
            Ty::None => write!(f, "None"),
            Ty::Func { params, ret, .. } | Ty::GenericFunc { params, ret, .. } => {
                write!(f, "def(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p)?;
                }
                write!(f, ") -> {}", ret)
            }
            Ty::Overload(candidates) => {
                write!(f, "overload(")?;
                for (i, candidate) in candidates.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{}", candidate)?;
                }
                write!(f, ")")
            }
            Ty::Param { name, .. } => write!(f, "{}", name),
            Ty::Assoc { base, name } => write!(f, "{}.{}", base, name),
            Ty::SelfType => write!(f, "Self"),
            Ty::Simd { dtype, width: 1 } if dtype.scalar_alias().is_some() => {
                write!(f, "{}", dtype.scalar_alias().unwrap())
            }
            Ty::Simd { dtype, width } => write!(f, "SIMD[DType.{}, {}]", dtype.name(), width),
            Ty::Error => write!(f, "Error"),
            Ty::Pointer(elem) => write!(f, "UnsafePointer[{}]", elem),
            Ty::Ref(reference) => write!(f, "ref {}", reference.referent),
            Ty::List(elem) => write!(f, "List[{}]", elem),
            Ty::Tuple(elems) => {
                write!(f, "Tuple[")?;
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", t)?;
                }
                write!(f, "]")
            }
            Ty::Range => write!(f, "range"),
            Ty::Struct(name, args) => {
                write!(f, "{}", name)?;
                if !args.is_empty() {
                    write!(f, "[")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", a)?;
                    }
                    write!(f, "]")?;
                }
                Ok(())
            }
        }
    }
}
