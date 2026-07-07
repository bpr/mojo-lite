//! The abstract syntax tree for the implemented subset of Mojo.
//!
//! Kept separate from the parser so the tree can be consumed (by the evaluator,
//! tests, future type checker) without depending on parsing details.
//!
//! **Spans.** Every [`Expr`] carries a source [`Span`] (`token::Span`), stamped by
//! the parser, so later stages (the MIR's `SpanTable`) can point diagnostics back
//! at source. The span is *metadata*: [`Expr`]'s `PartialEq` compares only the
//! `kind`, so AST-literal assertions in the tests stay span-agnostic.

use crate::token::Span;

/// A type annotation. Covers the scalar types plus nominal (`struct`) types,
/// which may carry type arguments (`Pair[Int]`), and references to a type
/// parameter. Function/closure types (`def(Int) -> Int`) are not yet parsed.
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
    SelfParam(String),
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
    Ref(Box<Type>),
}

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

/// A SIMD element type — the `<dt>` in `SIMD[DType.<dt>, width]`. mojo-lite
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
/// `**opts: Int`, or `mut x: Int`. The richer forms (default, variadic kind,
/// convention) are **parsed** but not yet implemented — the checker flags a
/// signature that uses any of them as an unsupported feature.
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
/// parametric-mutability reference). Parsed, not modeled. A `ref` convention may
/// carry an origin specifier (`ref[origin] x`), which is parsed and discarded.
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

/// A keyword argument at a call site: `name=value`. Parsed, not modeled.
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
    /// (no `self`); parsed but its semantics are deferred (checker flags it).
    pub has_self: bool,
    /// The receiver's argument convention: `None` = plain read-only `self`,
    /// `Some(Mut)` = `mut self` (writable, mutations persist), and `Some(Out)` /
    /// `Some(Owned)` / `Some(Read)` are parsed but not modeled (deferred).
    /// Meaningful only when `has_self`.
    pub self_convention: Option<ArgConvention>,
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
    pub params: Vec<FnParam>,
    pub positional_only: Option<usize>,
    pub keyword_only: Option<usize>,
    pub ret: Option<Type>,
    /// The method body. `None` for a pure requirement (`...`); `Some(body)` for a
    /// **default implementation** — parsed, but the checker flags a trait that
    /// declares one as unsupported ("parse now, run later").
    pub default_body: Option<Vec<Stmt>>,
}

/// A `comptime NAME: Type` **member requirement** inside a `trait` body — a
/// compile-time constant / associated alias a conforming struct must define
/// (`comptime count: Int`, `comptime EltType: Copyable`). Parsed; the checker
/// flags a trait that declares one as unsupported.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitComptime {
    pub name: String,
    pub ty: Type,
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
}

impl Stmt {
    /// Construct a spanned statement.
    pub fn new(kind: StmtKind, span: Span) -> Self {
        Stmt { kind, span }
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
    /// value should evaluate to a tuple of matching arity. Parsed and grammar-
    /// documented, but the checker flags it as unsupported ("parse now, run later").
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
    /// `[decorators] struct Name[type_params](conforms): <fields and methods>`
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
        methods: Vec<Method>,
        /// Whether `@fieldwise_init` was present (so `Name(...)` constructs).
        fieldwise_init: bool,
    },
    /// `trait Name[(Super, …)]: <members>` — declares method (and `comptime`
    /// member) requirements that a conforming struct must implement. `refines` is
    /// the optional parenthesized list of **super-traits** (trait inheritance);
    /// `comptime_members` are `comptime NAME: Type` requirements. A pure trait
    /// (only `...`-bodied methods, no `refines`, no comptime members) is fully
    /// modeled; the checker flags the parse-only extensions (`refines`, a default
    /// method body, or a comptime member) as unsupported. (Generic traits
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
    /// conditional** (Mojo's modern spelling; the older `@parameter if` is
    /// deprecated). Same shape as `If`, but the branch is meant to be selected at
    /// compile time. Parsed and grammar-documented, but the checker flags it as
    /// unsupported (comptime branch selection is deferred — "parse now, run later").
    ComptimeIf {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Option<Vec<Stmt>>,
    },
    /// `comptime for var in iter: <body>` — a **compile-time (unrolled) loop**
    /// (Mojo's modern spelling; the older `@parameter for` is deprecated). Same
    /// shape as `For`. Parsed and grammar-documented, but the checker flags it as
    /// unsupported (comptime unrolling is deferred).
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
    /// `import a.b.c [as alias]`. Parsed but not resolved (no module system yet).
    Import {
        path: Vec<String>,
        alias: Option<String>,
    },
    /// `from [.]*module import <targets>`. `level` is the number of leading dots
    /// (0 = absolute; relative imports raise it); `path` is the dotted module
    /// name (possibly empty for `from . import x`). Parsed but not resolved.
    FromImport {
        level: usize,
        path: Vec<String>,
        names: ImportNames,
    },
    /// `with item (',' item)*: <body>` — a context-manager block, where each
    /// `item` is a `WithItem` (a context expression + optional `as NAME`). Parsed
    /// and grammar-documented, but the checker flags it as unsupported (the
    /// `__enter__`/`__exit__` protocol is deferred — "parse now, run later").
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
}

impl Expr {
    /// Construct a spanned expression.
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Expr { kind, span }
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
    /// A unary operator applied to an operand, e.g. `-x` or `not ok`.
    Prefix(PrefixOp, Box<Expr>),
    /// A binary operator applied to two operands, e.g. `a + b`.
    Infix(InfixOp, Box<Expr>, Box<Expr>),
    /// A call by name: `name[param_args](args)`. The callee is resolved in scope,
    /// so this covers `def`s, closures passed in as arguments, built-ins, and
    /// struct construction. `param_args` are the explicit compile-time parameter
    /// arguments (`Pair[Int](...)`, `FixedBuffer[8](...)`); empty when omitted, in
    /// which case the checker infers the type parameters from `args`.
    /// `kwargs` are keyword arguments (`name=value`) — parsed, not modeled (a call
    /// using them is flagged unsupported by the checker).
    Call {
        name: String,
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
    /// A subscript `object[index]` — a SIMD lane read (the only subscript form).
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    /// The transfer sigil `expr^` (ownership move). Parsed for completeness but
    /// not modeled — evaluates to its operand (value semantics unchanged).
    Transfer(Box<Expr>),
    /// A non-empty list literal `[a, b, …]` — a `List` whose element type is
    /// inferred from the elements.
    ListLit(Vec<Expr>),
    /// A tuple literal `(a, b, …)` — a fixed-size, heterogeneous `Tuple`. `()` is
    /// the empty tuple and `(a,)` a 1-tuple; a plain `(e)` is grouping, not a tuple.
    TupleLit(Vec<Expr>),
    /// A named expression (walrus) `name := value` — assigns `value` to `name`
    /// and evaluates to it. Parsed and type-checked (as `value`'s type), but the
    /// evaluator reports it as unsupported ("parse now, run later").
    Named {
        name: String,
        value: Box<Expr>,
    },
    /// A conditional expression (ternary) `then_branch if cond else else_branch`.
    /// Parsed; semantics deferred (the checker flags it unsupported).
    IfExpr {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// A chained comparison `a < b < c …` (Python semantics: `a < b and b < c`,
    /// each operand evaluated once). `rest` is the `(op, operand)` sequence after
    /// `first`. A single comparison stays an `Infix`; this node is only built for a
    /// chain of length ≥ 2. Parsed; semantics deferred.
    Compare {
        first: Box<Expr>,
        rest: Vec<(InfixOp, Expr)>,
    },
    /// A slice subscript `object[lower:upper:step]`, each bound optional
    /// (`a[1:2]`, `a[:j]`, `a[::2]`). Parsed; semantics deferred.
    Slice {
        object: Box<Expr>,
        lower: Option<Box<Expr>>,
        upper: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    /// A t-string `t"…{expr}…"` (or raw `rt"…"`, `raw = true`): alternating
    /// literal text and interpolated expressions. Each `{…}` is a fully parsed
    /// sub-expression. Parsed; semantics deferred (checker flags it).
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
