//! The abstract syntax tree for the implemented subset of Mojo.
//!
//! Kept separate from the parser so linking, compile-time elaboration, semantic
//! checking, lowering, and syntax-oriented tools do not depend on parser internals.
//!
//! **Spans.** Every [`Expr`] carries a source [`Span`] (`token::Span`), stamped by
//! the parser, so later stages (the MIR's `SpanTable`) can point diagnostics back
//! at source. The span is *metadata*: [`Expr`]'s `PartialEq` compares only the
//! `kind`, so AST-literal assertions in the tests stay span-agnostic.

use crate::token::Span;

/// A type annotation. Covers the scalar types plus nominal (`struct`) types,
/// which may carry type arguments (`Pair[Int]`), and references to a type
/// parameter. Function/closure and reference types are represented even though
/// their runtime semantics are not yet supported.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    UInt,
    Bool,
    String,
    Float64,
    None,
    /// A name that resolves to a `struct` type, optionally applied to parameter
    /// arguments (`Point` is `Named("Point", [])`, `Pair[Int]` is
    /// `Named("Pair", [Type(Int)])`, `FixedBuffer[8]` is
    /// `Named("FixedBuffer", [Value(Int(8))])`). A bare `Named(name, [])` may also
    /// resolve to a type *parameter* in scope (a `T` inside a generic `def`); the
    /// checker decides. The checker validates the name and the argument kinds.
    Named(String, Vec<ParamArg>),
    /// `Self.T` — one of the enclosing struct's own type parameters, referenced
    /// from inside its body (Mojo spelling; a bare `T` is not in scope there).
    /// The checker also accepts it as a shorthand for a type-valued associated
    /// member when there is no struct parameter by that name.
    SelfParam(String),
    /// `Base.Member` in type position — an associated type/comptime member lookup
    /// on a type parameter, `Self`, or a concrete type.
    Assoc {
        base: Box<Type>,
        name: String,
    },
    /// Bare `Self` — the enclosing struct type (in a struct method) or the
    /// conforming type (in a trait method requirement).
    SelfType,
    /// A **function type** annotation: `def(T1, T2, …) [thin] [raises] -> R`
    /// (parameters are types only — no names or conventions). `thin` marks a
    /// non-capturing function pointer (default is capturing). Parsed; the checker
    /// flags a function-typed binding as unsupported (semantics deferred). Any
    /// `abi("…")` effect is parsed and discarded.
    Func {
        params: Vec<Type>,
        ret: Box<Type>,
        thin: bool,
        raises: bool,
    },
    /// A **reference type** `ref [origin] T` (Mojo's parametric-mutability
    /// reference — used in a `ref[origin]` return type). The origin specifier is
    /// parsed but **discarded** (origins are not modeled); `ty` is the referent
    /// type. Parsed; the checker flags a `ref` annotation as unsupported.
    Ref {
        referent: Box<Type>,
        origin: Option<OriginSpec>,
    },
}

/// Source syntax inside a `ref[...]` clause. Expressions are deliberately
/// retained verbatim: `origin_of` arguments are never evaluated, and semantic
/// resolution belongs to the checker where bindings have stable identities.
pub type OriginSpec = Vec<Expr>;

/// Explicit name for [`Type`] when code handles parsed source annotations rather
/// than the checked semantic lattice in [`crate::types::Ty`]. `Type` remains as a
/// compatibility name for the public AST API.
pub type SourceType = Type;

/// A compile-time **parameter** declared in a `[...]` list on a `struct` or `def`
/// header. Syntactically uniform (`NAME: X`): it is a **type parameter** when `X`
/// is one or more trait names (`T: Copyable & Movable`) or a **value parameter**
/// when `X` is a concrete type (`n: Int`). The checker classifies by resolving
/// `bounds` — a single entry naming a type means a value parameter; trait names
/// mean a type parameter. Value parameters are restricted to type `Int`.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    /// The trait bound(s), or (for a value parameter) the single value type name.
    pub bounds: Vec<String>,
    /// `Some(expr)` for `Origin[mut=expr]`; `None` for type/value parameters.
    pub origin_mutability: Option<Expr>,
    /// Whether this parameter follows the `//` infer-only marker.
    pub infer_only: bool,
}

/// A parameter **argument** supplied in a `[...]` list at a use site, i.e. a
/// `Pair[Int]` / `FixedBuffer[8]`. A type parameter takes a `Type`; a value
/// parameter takes a comptime value `Expr`. The two forms are distinguished by
/// the parser where it can (a leading type keyword / `Self` is a `Type`); a bare
/// identifier or an arithmetic expression parses as `Value`, and the checker
/// reinterprets a lone identifier as a type when the parameter is a type one.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamArg {
    Type(Type),
    Value(Expr),
}

/// A SIMD element type — the `<dt>` in `SIMD[DType.<dt>, width]`. mojito
/// supports the fixed-width integers, `float32`, and `bool` (real Mojo has more:
/// `float64`, wider integers, low-precision floats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float32,
    Float64,
    Bool,
}

impl Dtype {
    /// The `DType.<name>` spelling → the dtype (e.g. `"int32"` → `Int32`).
    pub fn from_name(name: &str) -> Option<Dtype> {
        Some(match name {
            "int8" => Dtype::Int8,
            "int16" => Dtype::Int16,
            "int32" => Dtype::Int32,
            "int64" => Dtype::Int64,
            "uint8" => Dtype::UInt8,
            "uint16" => Dtype::UInt16,
            "uint32" => Dtype::UInt32,
            "uint64" => Dtype::UInt64,
            "float32" => Dtype::Float32,
            "float64" => Dtype::Float64,
            "bool" => Dtype::Bool,
            _ => return None,
        })
    }

    /// The `DType.<name>` spelling of this dtype.
    pub fn name(self) -> &'static str {
        match self {
            Dtype::Int8 => "int8",
            Dtype::Int16 => "int16",
            Dtype::Int32 => "int32",
            Dtype::Int64 => "int64",
            Dtype::UInt8 => "uint8",
            Dtype::UInt16 => "uint16",
            Dtype::UInt32 => "uint32",
            Dtype::UInt64 => "uint64",
            Dtype::Float32 => "float32",
            Dtype::Float64 => "float64",
            Dtype::Bool => "bool",
        }
    }

    /// The scalar-alias spelling (`Int32`, `Float32`, …) that means
    /// `SIMD[DType.<self>, 1]`, or `None` for `bool` (which has no alias).
    pub fn scalar_alias(self) -> Option<&'static str> {
        Some(match self {
            Dtype::Int8 => "Int8",
            Dtype::Int16 => "Int16",
            Dtype::Int32 => "Int32",
            Dtype::Int64 => "Int64",
            Dtype::UInt8 => "UInt8",
            Dtype::UInt16 => "UInt16",
            Dtype::UInt32 => "UInt32",
            Dtype::UInt64 => "UInt64",
            Dtype::Float32 => "Float32",
            // `float64` is spelled by the native `Float64` type (with which it is
            // unified), and `bool` by `Bool` — neither is a SIMD *alias* name here.
            Dtype::Float64 | Dtype::Bool => return None,
        })
    }

    /// The dtype a scalar alias names (`"Int32"` → `Int32`), or `None`.
    pub fn from_scalar_alias(name: &str) -> Option<Dtype> {
        [
            Dtype::Int8,
            Dtype::Int16,
            Dtype::Int32,
            Dtype::Int64,
            Dtype::UInt8,
            Dtype::UInt16,
            Dtype::UInt32,
            Dtype::UInt64,
            Dtype::Float32,
        ]
        .into_iter()
        .find(|d| d.scalar_alias() == Some(name))
    }

    /// Whether this dtype's lanes are floating-point.
    pub fn is_float(self) -> bool {
        matches!(self, Dtype::Float32 | Dtype::Float64)
    }
}

/// A typed struct field, e.g. `a: Int`. (Function parameters use `FnParam`.)
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Type,
}

/// A function/method parameter, e.g. `a: Int`, `b: Int = 2`, `*rest: Int`,
/// `**opts: Int`, or `mut x: Int`. Defaults, variadics, and the supported
/// conventions participate in checking and VM argument binding.
#[derive(Debug, Clone, PartialEq)]
pub struct FnParam {
    pub name: String,
    pub ty: Type,
    /// A default value (`= expr`), for an optional argument. `None` if required.
    pub default: Option<Expr>,
    /// `*args` (variadic) / `**kwargs` (keyword variadic) / a plain parameter.
    pub kind: ParamKind,
    /// An argument convention (`read`/`mut`/`owned`/`out`); `None` = default.
    pub convention: Option<ArgConvention>,
    /// Retained source origin clause for a `ref[...]` convention.
    pub origin: Option<OriginSpec>,
}

/// Whether a `FnParam` is a plain parameter or a variadic (`*args`/`**kwargs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Regular,
    /// `*name: T` — a positional variadic parameter.
    Variadic,
    /// `**name: T` — a keyword variadic parameter.
    KwVariadic,
}

/// An argument-passing convention on an ordinary parameter (Mojo's `read`
/// (a.k.a. borrowed, the default), `mut`, `owned`, `out`, and `ref` — a
/// parametric-mutability reference). `read`, `mut`, `owned`, and call-scoped
/// `ref` behavior are modeled; ordinary `out` and origin semantics are not. A
/// parsed origin specifier is not retained by this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgConvention {
    Read,
    Mut,
    Owned,
    Out,
    Ref,
    /// `deinit` — the destructor/consuming-move convention: grants exclusive
    /// ownership and marks the value destroyed at function end. Current Mojo's
    /// `def __del__(deinit self)` (superseding the older `owned self`).
    Deinit,
}

/// A keyword argument at a call site: `name=value`. Structural binding is owned
/// by `crate::call`; the checker and VM interpret the matched argument slot.
#[derive(Debug, Clone, PartialEq)]
pub struct KwArg {
    pub name: String,
    pub value: Expr,
}

/// A decorator `@name`, `@dotted.name`, or `@name(args)` preceding a `def` or
/// `struct` (or a struct method). Parsed into the AST; only `@fieldwise_init` on a
/// struct is acted on (it sets `Stmt::Struct.fieldwise_init`) — the rest are
/// recorded but not modeled (syntax-first phase).
#[derive(Debug, Clone, PartialEq)]
pub struct Decorator {
    /// The dotted name parts, e.g. `["always_inline"]` or `["a", "b"]`.
    pub path: Vec<String>,
    /// Positional arguments in `@deco(args)`; empty if no `(...)`.
    pub args: Vec<Expr>,
    /// Keyword arguments in `@deco(k=v)`; empty if none.
    pub kwargs: Vec<KwArg>,
}

/// A `def` method inside a `struct`. `self` (the implicit, untyped first
/// parameter) is not stored in `params`.
#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    pub name: String,
    /// Whether the method has a `self` receiver. `false` for a `@staticmethod`
    /// (no `self`); static method semantics are currently rejected by the checker.
    pub has_self: bool,
    /// The receiver's argument convention: `None` = plain read-only `self`,
    /// `Some(Mut)` = `mut self` (writable, mutations persist). `out self` is the
    /// constructor convention, `deinit self` the destructor convention, and
    /// owned/read receivers participate in lifecycle and call checking.
    pub self_convention: Option<ArgConvention>,
    pub self_origin: Option<OriginSpec>,
    /// Decorators preceding the method (`@staticmethod`, …). Parsed, not modeled.
    pub decorators: Vec<Decorator>,
    pub params: Vec<FnParam>,
    /// Index of a `/` (positional-only) marker in `params`, if present (parsed).
    pub positional_only: Option<usize>,
    /// Index of a bare `*` (keyword-only) marker in `params`, if present (parsed).
    pub keyword_only: Option<usize>,
    /// Whether the `raises` effect was declared (parsed, not enforced).
    pub raises: bool,
    pub ret: Option<Type>,
    pub body: Vec<Stmt>,
}

/// A method in a `trait`: either a **requirement** (`def …:` with a `...` body,
/// which a conforming struct must supply) or a **default implementation** (a real
/// body). Like `Method`, `self` is the implicit first parameter and is not stored
/// in `params`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitMethod {
    pub name: String,
    /// The receiver's argument convention: `None` = plain read-only `self`,
    /// or one of Mojo's explicit receiver conventions (`mut self`, `owned self`,
    /// `ref self`, ...). Like `Method`, `self` is implicit and not stored in
    /// `params`.
    pub self_convention: Option<ArgConvention>,
    pub self_origin: Option<OriginSpec>,
    pub params: Vec<FnParam>,
    pub positional_only: Option<usize>,
    pub keyword_only: Option<usize>,
    pub ret: Option<Type>,
    /// The method body. `None` for a pure requirement (`...`); `Some(body)` for a
    /// **default implementation**. The checker currently rejects default bodies.
    pub default_body: Option<Vec<Stmt>>,
}

/// A `comptime NAME: Type` **member requirement** inside a `trait` body — a
/// compile-time constant / associated alias a conforming struct must define
/// (`comptime count: Int`, `comptime EltType: Copyable`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraitComptime {
    pub name: String,
    pub ty: Type,
}

/// A `comptime NAME = expr` associated compile-time fact inside a `struct` body.
/// These are declarations on the type, not runtime statements.
#[derive(Debug, Clone, PartialEq)]
pub struct StructComptime {
    pub name: String,
    pub value: Expr,
}

/// The names imported by a `from ... import ...` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportNames {
    /// `import *`
    Wildcard,
    /// `import name [as alias], ...`
    Names(Vec<ImportName>),
}

/// One `name [as alias]` in a `from ... import` target list.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportName {
    pub name: String,
    pub alias: Option<String>,
}

/// One context manager in a `with` statement: an expression whose value is a
/// context manager, and an optional `as NAME` binding for its `__enter__` result
/// (`with open(p) as f`, or bindingless `with lock()`). A single `with` may carry
/// several, comma-separated (`with a() as x, b() as y:`). The `as` target is a
/// plain `NAME` — the parenthesized and tuple-target forms aren't in the Mojo docs,
/// so (strict-subset) they aren't parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct WithItem {
    pub context: Expr,
    pub var: Option<String>,
}

/// A statement node: its [`StmtKind`] plus the source [`Span`] it was parsed from.
/// Like [`Expr`], equality ignores the span (it is metadata), so AST-literal
/// assertions stay span-agnostic.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
    /// Source module identity assigned by the linker. Parser-created statements
    /// use `None`; linked entry/import declarations carry their originating path.
    pub module: Option<String>,
}

impl Stmt {
    /// Construct a spanned statement.
    pub fn new(kind: StmtKind, span: Span) -> Self {
        Stmt {
            kind,
            span,
            module: None,
        }
    }

    pub fn source_span(&self) -> crate::token::SourceSpan {
        crate::token::SourceSpan::new(self.module.clone(), self.span)
    }
}

/// Span-agnostic structural equality: two statements are equal iff their kinds are.
impl PartialEq for Stmt {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

/// Wrap a bare [`StmtKind`] into a [`Stmt`] with a dummy span. Convenient for
/// building AST literals in tests (where the span is irrelevant).
impl From<StmtKind> for Stmt {
    fn from(kind: StmtKind) -> Self {
        Stmt::new(kind, crate::token::DUMMY_SPAN)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// `var name[: Type] = value` — declares (and initializes) a new variable.
    /// The annotation is optional: `ty: None` is an **inferred** `var` (the type
    /// comes from `value`, materializing a literal to its default kind).
    VarDecl {
        name: String,
        ty: Option<Type>,
        value: Expr,
    },
    /// `ref name = place` — a local reference binding. It aliases the source
    /// place without copying and participates in persistent loan analysis.
    RefDecl { name: String, value: Expr },
    /// `name = value` — re-assigns an already-declared variable (every `var` is
    /// mutable). Distinct from `VarDecl`; the value must keep the declared type.
    Assign { name: String, value: Expr },
    /// `target OP= value` — augmented assignment, where `target` is a `NAME` or a
    /// **place** (`Expr::Identifier`/`Member`/`Index`). Means `target = target OP
    /// value`, but the target (and any index within it) is evaluated once. `op` is
    /// one of `+ - * / // % **`.
    AugAssign {
        place: Expr,
        op: InfixOp,
        value: Expr,
    },
    /// `a, b, … = value` — **tuple-unpacking assignment** (bare form, no `var`;
    /// the `var a, b = …` form is an open Mojo feature request, not valid yet).
    /// `targets` is the comma-separated target list (each a `NAME` or place); the
    /// value evaluates to a tuple of matching arity and lowers to independent
    /// place writes.
    Unpack { targets: Vec<Expr>, value: Expr },
    /// `place = value` — assign through a **place expression** rooted at a
    /// mutable variable (or `mut self`): a field write `p.x = e`, a list-element
    /// write `xs[i] = e`, or any nesting (`p.items[i].x = e`). The place is a
    /// chain of `Expr::Member` / `Expr::Index` over an `Expr::Identifier` root.
    /// Mutation happens in place in the root variable's binding (value semantics
    /// preserved: only that binding changes).
    SetPlace { place: Expr, value: Expr },
    /// `[decorators] def name[type_params](params) [raises] -> ret: <body>`
    Def {
        name: String,
        /// Decorators preceding the function (`@staticmethod`, …). Parsed, not modeled.
        decorators: Vec<Decorator>,
        /// Type parameters (`[T: Trait]`); empty for a non-generic function.
        type_params: Vec<TypeParam>,
        params: Vec<FnParam>,
        /// Index of a `/` (positional-only) marker in `params`, if present (parsed).
        positional_only: Option<usize>,
        /// Index of a bare `*` (keyword-only) marker in `params`, if present (parsed).
        keyword_only: Option<usize>,
        /// Whether the `raises` effect was declared. Parsed (incl. a discarded
        /// error type after `raises`) but not enforced — the effect analysis is
        /// deferred (a `raises` function is not required to raise, and a call to
        /// one is not required to be handled).
        raises: bool,
        ret: Option<Type>,
        body: Vec<Stmt>,
    },
    /// `[decorators] struct Name[type_params](conforms): <fields, associated
    /// comptime facts, and methods>`
    Struct {
        name: String,
        /// Decorators preceding the struct (`@value`, …). Parsed; only
        /// `@fieldwise_init` is acted on (via `fieldwise_init`).
        decorators: Vec<Decorator>,
        /// Type parameters (`[T: Trait]`); empty for a non-generic struct.
        type_params: Vec<TypeParam>,
        /// Traits this struct declares conformance to (`struct S(A, B):`); empty
        /// if none. Nominal — a `[T: A]` bound is satisfied only if `A` is here.
        conforms: Vec<String>,
        fields: Vec<Param>,
        associated: Vec<StructComptime>,
        methods: Vec<Method>,
        /// Whether `@fieldwise_init` was present (so `Name(...)` constructs).
        fieldwise_init: bool,
    },
    /// `trait Name[(Super, …)]: <members>` — declares method (and `comptime`
    /// member) requirements that a conforming struct must implement. `refines` is
    /// the optional parenthesized list of **super-traits** (trait inheritance);
    /// `comptime_members` are `comptime NAME: Type` requirements. A trait with
    /// method requirements and associated comptime requirements is modeled; the
    /// checker still flags parse-only extensions (`refines` and a default method
    /// body) as unsupported. (Generic traits
    /// `trait T[U]:` are not valid current Mojo, so they are not parsed.)
    Trait {
        name: String,
        refines: Vec<String>,
        methods: Vec<TraitMethod>,
        comptime_members: Vec<TraitComptime>,
    },
    /// `comptime NAME = value` — a compile-time `Int` constant (Mojo removed
    /// `alias`; `comptime` replaces it). `value` must be a comptime `Int`
    /// expression; the constant is usable as a value-parameter argument and as an
    /// ordinary `Int` at runtime.
    Comptime { name: String, value: Expr },
    /// `comptime if cond: ... (elif cond: ...)* (else: ...)?` — a **compile-time
    /// conditional**. Compile-time elaboration selects one branch before semantic
    /// checking, so discarded branches do not need to type-check.
    ComptimeIf {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Option<Vec<Stmt>>,
    },
    /// `comptime for var in iter: <body>` — a **compile-time (unrolled) loop**
    /// (Mojo's modern spelling; the older `@parameter for` is deprecated). Same
    /// shape as `For`. Compile-time elaboration unrolls it under a fuel quota.
    ComptimeFor {
        var: String,
        iter: Expr,
        body: Vec<Stmt>,
    },
    /// `if cond: ... (elif cond: ...)* (else: ...)?`
    ///
    /// `branches` holds the `if` plus each `elif` as a `(condition, body)` pair,
    /// tried in order; `orelse` is the optional `else` body.
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Option<Vec<Stmt>>,
    },
    /// `while cond: <body>`
    While { cond: Expr, body: Vec<Stmt> },
    /// `for var in iter: <body>` — `iter` must evaluate to a `range(...)`.
    For {
        var: String,
        iter: Expr,
        body: Vec<Stmt>,
    },
    /// `return` or `return expr`
    Return(Option<Expr>),
    /// `raise expr` — raise an error (an `Error` value, or a `String` shorthand).
    Raise(Expr),
    /// `import a.b.c [as alias]`. The linker currently treats this as a no-op;
    /// qualified module namespaces and aliases are not implemented.
    Import {
        path: Vec<String>,
        alias: Option<String>,
    },
    /// `from [.]*module import <targets>`. `level` is the number of leading dots
    /// (0 = absolute; relative imports raise it); `path` is the dotted module
    /// name (possibly empty for `from . import x`). The linker resolves wildcard
    /// and selective imports; imported aliases are still deferred.
    FromImport {
        level: usize,
        path: Vec<String>,
        names: ImportNames,
    },
    /// `with item (',' item)*: <body>` — a context-manager block, where each
    /// `item` is a `WithItem` (a context expression + optional `as NAME`). Parsed
    /// and grammar-documented; the checker rejects it until the
    /// `__enter__`/`__exit__` protocol is implemented.
    With {
        items: Vec<WithItem>,
        body: Vec<Stmt>,
    },
    /// `try: <body> [except [e]: ...] [else: ...] [finally: ...]`. At least one of
    /// `except`/`finally` is present. `except` optionally binds the error name.
    Try {
        body: Vec<Stmt>,
        except: Option<(Option<String>, Vec<Stmt>)>,
        orelse: Option<Vec<Stmt>>,
        finalbody: Option<Vec<Stmt>>,
    },
    /// `pass`
    Pass,
    /// `break` — exit the innermost loop.
    Break,
    /// `continue` — skip to the next iteration of the innermost loop.
    Continue,
    /// A bare expression used for its side effects, e.g. `f(1)`.
    Expr(Expr),
}

/// An expression node: its [`ExprKind`] plus the source [`Span`] it was parsed
/// from. Equality ignores the span (see the module note), so `a == b` iff their
/// kinds are structurally equal.
#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
    pub source: Option<String>,
}

impl Expr {
    /// Construct a spanned expression.
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Expr {
            kind,
            span,
            source: None,
        }
    }

    pub fn source_span(&self) -> crate::token::SourceSpan {
        crate::token::SourceSpan::new(self.source.clone(), self.span)
    }
}

/// Wrap a bare [`ExprKind`] into an [`Expr`] with a dummy span. Convenient for
/// synthesizing expressions and for building AST literals in tests (where the
/// span is irrelevant — [`Expr`]'s equality ignores it).
impl From<ExprKind> for Expr {
    fn from(kind: ExprKind) -> Self {
        Expr::new(kind, crate::token::DUMMY_SPAN)
    }
}

/// Span-agnostic structural equality: two expressions are equal iff their kinds
/// are equal, regardless of where in the source each was parsed.
impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    None,
    Identifier(String),
    /// A type used as a compile-time value, such as a function/closure signature.
    /// Parsed for source compatibility; semantic treatment as a first-class
    /// compile-time value is deferred.
    TypeValue(Type),
    /// A unary operator applied to an operand, e.g. `-x` or `not ok`.
    Prefix(PrefixOp, Box<Expr>),
    /// A binary operator applied to two operands, e.g. `a + b`.
    Infix(InfixOp, Box<Expr>, Box<Expr>),
    /// A call by name: `name[param_args](args)`. The callee is resolved in scope,
    /// so this covers `def`s, supported nested defs, built-ins, and struct
    /// construction. `param_args` are the explicit compile-time parameter
    /// arguments (`Pair[Int](...)`, `FixedBuffer[8](...)`); empty when omitted, in
    /// which case the checker infers the type parameters from `args`.
    /// `kwargs` are keyword arguments (`name=value`) and use the same structural
    /// matcher as positional/default/variadic arguments.
    Call {
        name: String,
        param_args: Vec<ParamArg>,
        args: Vec<Expr>,
        kwargs: Vec<KwArg>,
    },
    /// A call whose callee is an expression rather than a bare source name,
    /// including calls through fields and parameterized callable values. Parsed
    /// for Mojo source compatibility; callable-expression semantics are deferred.
    Invoke {
        callee: Box<Expr>,
        param_args: Vec<ParamArg>,
        args: Vec<Expr>,
        kwargs: Vec<KwArg>,
    },
    /// Field access on a struct value: `object.field`.
    Member {
        object: Box<Expr>,
        field: String,
    },
    /// A method call on a struct value: `object.method(args)`.
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        kwargs: Vec<KwArg>,
    },
    /// A subscript `object[index]` — supported for lists, tuples, SIMD, unsafe
    /// pointers, and user types implementing `__getitem__`.
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    /// A parameterized type in expression position — `Name[param_args]` — used as
    /// the receiver of a static method call, e.g. `UnsafePointer[T].alloc(n)`.
    /// Only meaningful there; a bare `TypeApply` is a type, not a runtime value.
    TypeApply {
        name: String,
        args: Vec<ParamArg>,
    },
    /// The transfer sigil `expr^`: an ownership move checked by MIR dataflow,
    /// including field-sensitive partial moves.
    Transfer(Box<Expr>),
    /// A non-empty list literal `[a, b, …]` — a `List` whose element type is
    /// inferred from the elements.
    ListLit(Vec<Expr>),
    /// A brace-delimited set/dictionary literal. Entries retain an optional
    /// value (`key: value`); semantics are deferred.
    BraceLit(Vec<(Expr, Option<Expr>)>),
    /// A tuple literal `(a, b, …)` — a fixed-size, heterogeneous `Tuple`. `()` is
    /// the empty tuple and `(a,)` a 1-tuple; a plain `(e)` is grouping, not a tuple.
    TupleLit(Vec<Expr>),
    /// A named expression (walrus) `name := value`. It is parsed and typed as the
    /// value, but MIR deliberately emits an unsupported operation.
    Named {
        name: String,
        value: Box<Expr>,
    },
    /// A conditional expression (ternary) `then_branch if cond else else_branch`.
    /// Checked and lowered with ordinary conditional control flow.
    IfExpr {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// A chained comparison `a < b < c …` (Python semantics: `a < b and b < c`,
    /// each operand evaluated once). `rest` is the `(op, operand)` sequence after
    /// `first`. A single comparison stays an `Infix`; this node is only built for a
    /// chain of length ≥ 2. Lowering evaluates every operand at most once and
    /// short-circuits the comparisons.
    Compare {
        first: Box<Expr>,
        rest: Vec<(InfixOp, Expr)>,
    },
    /// A slice subscript `object[lower:upper:step]`, each bound optional
    /// (`a[1:2]`, `a[:j]`, `a[::2]`). Supported for lists and strings.
    Slice {
        object: Box<Expr>,
        lower: Option<Box<Expr>>,
        upper: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    /// A t-string `t"…{expr}…"` (or raw `rt"…"`, `raw = true`): alternating
    /// literal text and interpolated expressions. Each `{…}` is a fully parsed
    /// sub-expression. The checker currently rejects t-string semantics.
    TString {
        parts: Vec<TStringPart>,
        raw: bool,
    },
}

/// One piece of a t-string: literal text or an interpolated expression.
#[derive(Debug, Clone, PartialEq)]
pub enum TStringPart {
    Literal(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrefixOp {
    /// Arithmetic negation, `-`.
    Neg,
    /// Logical negation, `not`.
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InfixOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    Pow,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    /// Membership: `x in container` → `Bool`.
    In,
    /// Non-membership: `x not in container` → `Bool`.
    NotIn,
}

impl InfixOp {
    /// The dunder method a binary operator dispatches to on a user `struct` left
    /// operand (`a + b` → `a.__add__(b)`), or `None` for operators that don't
    /// dispatch this way (`and`/`or` short-circuit; `in`/`not in` dispatch to
    /// `__contains__` on the *right* operand — see the checker/VM). Shared by the
    /// checker (typing) and the VM (runtime dispatch) so they agree.
    pub fn dunder(self) -> Option<&'static str> {
        Some(match self {
            InfixOp::Add => "__add__",
            InfixOp::Sub => "__sub__",
            InfixOp::Mul => "__mul__",
            InfixOp::Div => "__truediv__",
            InfixOp::FloorDiv => "__floordiv__",
            InfixOp::Mod => "__mod__",
            InfixOp::Shl => "__lshift__",
            InfixOp::Shr => "__rshift__",
            InfixOp::BitAnd => "__and__",
            InfixOp::BitOr => "__or__",
            InfixOp::Pow => "__pow__",
            InfixOp::Eq => "__eq__",
            InfixOp::Ne => "__ne__",
            InfixOp::Lt => "__lt__",
            InfixOp::Gt => "__gt__",
            InfixOp::Le => "__le__",
            InfixOp::Ge => "__ge__",
            InfixOp::And | InfixOp::Or | InfixOp::In | InfixOp::NotIn => return None,
        })
    }
}

/// Attach one linked source path to an entire AST subtree. Spans remain local
/// byte ranges; `source_span()` combines them with this provenance.
pub(crate) fn stamp_source(statements: &mut [Stmt], source: &str) {
    for statement in statements {
        statement.module = Some(source.to_string());
        stamp_stmt_kind(&mut statement.kind, source);
    }
}

fn stamp_block(block: &mut [Stmt], source: &str) {
    stamp_source(block, source);
}

fn stamp_expr(expr: &mut Expr, source: &str) {
    expr.source = Some(source.to_string());
    match &mut expr.kind {
        ExprKind::Prefix(_, value) | ExprKind::Transfer(value) => stamp_expr(value, source),
        ExprKind::Infix(_, left, right)
        | ExprKind::Index {
            object: left,
            index: right,
        } => {
            stamp_expr(left, source);
            stamp_expr(right, source);
        }
        ExprKind::Call {
            param_args,
            args,
            kwargs,
            ..
        } => {
            stamp_param_args(param_args, source);
            for arg in args {
                stamp_expr(arg, source);
            }
            for arg in kwargs {
                stamp_expr(&mut arg.value, source);
            }
        }
        ExprKind::Invoke {
            callee,
            param_args,
            args,
            kwargs,
        } => {
            stamp_expr(callee, source);
            stamp_param_args(param_args, source);
            for arg in args {
                stamp_expr(arg, source);
            }
            for arg in kwargs {
                stamp_expr(&mut arg.value, source);
            }
        }
        ExprKind::Member { object, .. } => stamp_expr(object, source),
        ExprKind::MethodCall {
            object,
            args,
            kwargs,
            ..
        } => {
            stamp_expr(object, source);
            for arg in args {
                stamp_expr(arg, source);
            }
            for arg in kwargs {
                stamp_expr(&mut arg.value, source);
            }
        }
        ExprKind::TypeApply { args, .. } => stamp_param_args(args, source),
        ExprKind::ListLit(values) | ExprKind::TupleLit(values) => {
            for value in values {
                stamp_expr(value, source);
            }
        }
        ExprKind::BraceLit(entries) => {
            for (key, value) in entries {
                stamp_expr(key, source);
                if let Some(value) = value {
                    stamp_expr(value, source);
                }
            }
        }
        ExprKind::Named { value, .. } => stamp_expr(value, source),
        ExprKind::IfExpr {
            cond,
            then_branch,
            else_branch,
        } => {
            stamp_expr(cond, source);
            stamp_expr(then_branch, source);
            stamp_expr(else_branch, source);
        }
        ExprKind::Compare { first, rest } => {
            stamp_expr(first, source);
            for (_, value) in rest {
                stamp_expr(value, source);
            }
        }
        ExprKind::Slice {
            object,
            lower,
            upper,
            step,
        } => {
            stamp_expr(object, source);
            for value in [lower, upper, step].into_iter().flatten() {
                stamp_expr(value, source);
            }
        }
        ExprKind::TString { parts, .. } => {
            for part in parts {
                if let TStringPart::Expr(value) = part {
                    stamp_expr(value, source);
                }
            }
        }
        ExprKind::TypeValue(ty) => stamp_type(ty, source),
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::None
        | ExprKind::Identifier(_) => {}
    }
}

fn stamp_param_args(args: &mut [ParamArg], source: &str) {
    for arg in args {
        if let ParamArg::Value(value) = arg {
            stamp_expr(value, source);
        }
    }
}

fn stamp_type(ty: &mut Type, source: &str) {
    match ty {
        Type::Named(_, args) => stamp_param_args(args, source),
        Type::Assoc { base, .. } => stamp_type(base, source),
        Type::Ref { referent, origin } => {
            stamp_type(referent, source);
            if let Some(origins) = origin {
                for expression in origins {
                    stamp_expr(expression, source);
                }
            }
        }
        Type::Func { params, ret, .. } => {
            for param in params {
                stamp_type(param, source);
            }
            stamp_type(ret, source);
        }
        Type::Int
        | Type::UInt
        | Type::Bool
        | Type::String
        | Type::Float64
        | Type::None
        | Type::SelfParam(_)
        | Type::SelfType => {}
    }
}

fn stamp_fn_param(param: &mut FnParam, source: &str) {
    stamp_type(&mut param.ty, source);
    if let Some(value) = &mut param.default {
        stamp_expr(value, source);
    }
}

fn stamp_decorators(decorators: &mut [Decorator], source: &str) {
    for decorator in decorators {
        for arg in &mut decorator.args {
            stamp_expr(arg, source);
        }
        for arg in &mut decorator.kwargs {
            stamp_expr(&mut arg.value, source);
        }
    }
}

fn stamp_stmt_kind(kind: &mut StmtKind, source: &str) {
    match kind {
        StmtKind::VarDecl { ty, value, .. } => {
            if let Some(ty) = ty {
                stamp_type(ty, source);
            }
            stamp_expr(value, source);
        }
        StmtKind::RefDecl { value, .. }
        | StmtKind::Assign { value, .. }
        | StmtKind::Comptime { value, .. }
        | StmtKind::Raise(value)
        | StmtKind::Return(Some(value))
        | StmtKind::Expr(value) => stamp_expr(value, source),
        StmtKind::SetPlace { place, value } | StmtKind::AugAssign { place, value, .. } => {
            stamp_expr(place, source);
            stamp_expr(value, source);
        }
        StmtKind::Unpack { targets, value } => {
            for target in targets {
                stamp_expr(target, source);
            }
            stamp_expr(value, source);
        }
        StmtKind::If { branches, orelse } | StmtKind::ComptimeIf { branches, orelse } => {
            for (condition, block) in branches {
                stamp_expr(condition, source);
                stamp_block(block, source);
            }
            if let Some(block) = orelse {
                stamp_block(block, source);
            }
        }
        StmtKind::While { cond, body } => {
            stamp_expr(cond, source);
            stamp_block(body, source);
        }
        StmtKind::For { iter, body, .. } | StmtKind::ComptimeFor { iter, body, .. } => {
            stamp_expr(iter, source);
            stamp_block(body, source);
        }
        StmtKind::With { items, body } => {
            for item in items {
                stamp_expr(&mut item.context, source);
            }
            stamp_block(body, source);
        }
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            stamp_block(body, source);
            if let Some((_, block)) = except {
                stamp_block(block, source);
            }
            if let Some(block) = orelse {
                stamp_block(block, source);
            }
            if let Some(block) = finalbody {
                stamp_block(block, source);
            }
        }
        StmtKind::Def {
            params,
            ret,
            body,
            decorators,
            ..
        } => {
            for param in params {
                stamp_fn_param(param, source);
            }
            if let Some(ret) = ret {
                stamp_type(ret, source);
            }
            stamp_decorators(decorators, source);
            stamp_block(body, source);
        }
        StmtKind::Struct {
            fields,
            associated,
            methods,
            decorators,
            ..
        } => {
            for field in fields {
                stamp_type(&mut field.ty, source);
            }
            for member in associated {
                stamp_expr(&mut member.value, source);
            }
            stamp_decorators(decorators, source);
            for method in methods {
                for param in &mut method.params {
                    stamp_fn_param(param, source);
                }
                if let Some(ret) = &mut method.ret {
                    stamp_type(ret, source);
                }
                stamp_decorators(&mut method.decorators, source);
                stamp_block(&mut method.body, source);
            }
        }
        StmtKind::Trait {
            methods,
            comptime_members,
            ..
        } => {
            for method in methods {
                for param in &mut method.params {
                    stamp_fn_param(param, source);
                }
                if let Some(ret) = &mut method.ret {
                    stamp_type(ret, source);
                }
                if let Some(body) = &mut method.default_body {
                    stamp_block(body, source);
                }
            }
            for member in comptime_members {
                stamp_type(&mut member.ty, source);
            }
        }
        StmtKind::Return(None)
        | StmtKind::Import { .. }
        | StmtKind::FromImport { .. }
        | StmtKind::Pass
        | StmtKind::Break
        | StmtKind::Continue => {}
    }
}
