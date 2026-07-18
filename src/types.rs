//! Shared semantic type representation.
//!
//! This is the type lattice used by the checker, but it also needs to be visible
//! to compile-time values once comptime can carry type values. Keeping `Ty` out
//! of `checker.rs` lets [`CtValue`](crate::ct::CtValue) represent `Type(Box<Ty>)`
//! without making the checker the owner of all type-level facts.

use std::fmt;

use crate::ast::{ArgConvention, Dtype};
use crate::ct::{CtExpr, CtValue};

/// Descriptor type selected for a slice literal at the checked boundary.
/// Two-component literals can use the view-oriented contiguous descriptor;
/// literals with a second colon use the owning strided descriptor. `Slice` is
/// the general protocol fallback accepted by user-defined collections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceKind {
    Slice,
    ContiguousSlice,
    StridedSlice,
}

impl SliceKind {
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Slice => "Slice",
            Self::ContiguousSlice => "ContiguousSlice",
            Self::StridedSlice => "StridedSlice",
        }
    }
}

/// A type in mojito's semantic lattice. Scalars mirror `ast::Type`; `Func` is
/// synthesized from a `def` signature or lowered from a function-type annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Int,
    UInt,
    Bool,
    String,
    Float64,
    None,
    /// Bottom type: no runtime value can inhabit it.
    Never,
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
        /// Homogeneous element type collected by `**kwargs`, when present.
        kw_variadic: Option<Box<Ty>>,
        positional_only: Option<usize>,
        keyword_only: Option<usize>,
        raises: bool,
        error: Option<Box<Ty>>,
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
        /// Homogeneous element type collected by `**kwargs`, when present.
        kw_variadic: Option<Box<Ty>>,
        positional_only: Option<usize>,
        keyword_only: Option<usize>,
        raises: bool,
        error: Option<Box<Ty>>,
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
    /// The default type of a set display/comprehension.
    Set(Box<Ty>),
    /// The default type of a dictionary display/comprehension.
    Dict(Box<Ty>, Box<Ty>),
    /// The built-in `Tuple[T1, ..., Tn]`.
    Tuple(Vec<Ty>),
    /// The built-in tagged union `Variant[T1, ..., Tn]`.  The ordering is part
    /// of the type: it determines the runtime tag used by typed projection.
    Variant(Vec<Ty>),
    /// The built-in `UnsafePointer[T, origin]`.  The VM erases the origin, but
    /// the checked and MIR types retain it for lifetime/aggregate validation.
    Pointer {
        element: Box<Ty>,
        origin: crate::origin::PointerOrigin,
    },
    /// A reference value. Origins and permissions are checked statically; its
    /// runtime representation is introduced only after loan checking exists.
    Ref(crate::origin::RefTy),
}

/// A declared compile-time parameter of a generic `struct`/`def`, classified
/// from `[name: X]` by whether `X` is a trait or a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamDecl {
    /// A type parameter `T: Trait & ...`.
    Type {
        name: String,
        bounds: Vec<String>,
        default: Option<Box<Ty>>,
        infer_only: bool,
        variadic: bool,
        constraints: Vec<GenericConstraint>,
    },
    /// A value parameter such as `n: Int` or `label: String`.  Retaining the
    /// declared type is essential: compile-time values participate in generic
    /// identity, but only values representable by this type may bind here.
    Value {
        name: String,
        ty: Box<Ty>,
        default: Option<CtExpr>,
        infer_only: bool,
        variadic: bool,
        constraints: Vec<GenericConstraint>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintOperand {
    Param(String),
    Value(CtValue),
    Type(Ty),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericConstraint {
    Conforms { param: String, trait_name: String },
    ConformsPack { param: String, trait_name: String },
    Eq(ConstraintOperand, ConstraintOperand),
    Ne(ConstraintOperand, ConstraintOperand),
    Lt(ConstraintOperand, ConstraintOperand),
    Le(ConstraintOperand, ConstraintOperand),
    Gt(ConstraintOperand, ConstraintOperand),
    Ge(ConstraintOperand, ConstraintOperand),
    And(Box<GenericConstraint>, Box<GenericConstraint>),
    Or(Box<GenericConstraint>, Box<GenericConstraint>),
    Not(Box<GenericConstraint>),
    Bool(bool),
}

impl ParamDecl {
    pub fn name(&self) -> &str {
        match self {
            ParamDecl::Type { name, .. } | ParamDecl::Value { name, .. } => name,
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
            Ty::Never => write!(f, "Never"),
            Ty::Func {
                params,
                ret,
                raises,
                ..
            }
            | Ty::GenericFunc {
                params,
                ret,
                raises,
                ..
            } => {
                write!(f, "def(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p)?;
                }
                write!(f, ")")?;
                if *raises {
                    write!(f, " raises")?;
                }
                write!(f, " -> {}", ret)
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
            Ty::Simd { dtype, width: 1 } => match dtype.scalar_alias() {
                Some(alias) => write!(f, "{}", alias),
                None => write!(f, "SIMD[DType.{}, 1]", dtype.name()),
            },
            Ty::Simd { dtype, width } => write!(f, "SIMD[DType.{}, {}]", dtype.name(), width),
            Ty::Error => write!(f, "Error"),
            Ty::Pointer { element, origin } => {
                write!(f, "UnsafePointer[{element}")?;
                match origin {
                    crate::origin::PointerOrigin::Legacy => {}
                    crate::origin::PointerOrigin::Place { place, .. } => {
                        write!(f, ", origin@{}", place.root.0)?
                    }
                    crate::origin::PointerOrigin::Param { id, .. } => {
                        write!(f, ", origin#{}", id.0)?
                    }
                    crate::origin::PointerOrigin::Static => write!(f, ", StaticConstantOrigin")?,
                    crate::origin::PointerOrigin::Untracked { mutable: true } => {
                        write!(f, ", MutUntrackedOrigin")?
                    }
                    crate::origin::PointerOrigin::Untracked { mutable: false } => {
                        write!(f, ", ImmutUntrackedOrigin")?
                    }
                    crate::origin::PointerOrigin::UnsafeAny { mutable: true } => {
                        write!(f, ", MutUnsafeAnyOrigin")?
                    }
                    crate::origin::PointerOrigin::UnsafeAny { mutable: false } => {
                        write!(f, ", ImmutUnsafeAnyOrigin")?
                    }
                }
                write!(f, "]")
            }
            Ty::Ref(reference) => write!(f, "ref {}", reference.referent),
            Ty::List(elem) => write!(f, "List[{}]", elem),
            Ty::Set(elem) => write!(f, "Set[{}]", elem),
            Ty::Dict(key, value) => write!(f, "Dict[{}, {}]", key, value),
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
            Ty::Variant(alternatives) => {
                write!(f, "Variant[")?;
                for (i, ty) in alternatives.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", ty)?;
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
