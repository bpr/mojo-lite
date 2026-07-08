use crate::token::Token;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum LexError {
    IndentationError(usize),
    UnmatchedParenthesis(usize),
    UnexpectedCharacter(char, usize),
    InvalidInteger(usize),
    InvalidFloat(usize),
    UnterminatedString(usize),
    InvalidEscape(char, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    LexerError(LexError),
    UnexpectedToken(Token, String),
    UnexpectedEof(String),
    UnknownType(String),
}

/// Errors from the static type checker (`checker.rs`), which runs after parsing
/// and before evaluation. These are the compile-time analogues of the
/// `RuntimeError`s the dynamically typed evaluator would otherwise raise — plus
/// rules Mojo enforces statically that the evaluator does not (same-scope
/// redeclaration, `return` outside a function).
#[derive(Debug, Clone, PartialEq)]
pub enum TypeError {
    UndefinedVariable(String),
    /// A non-`Copyable` value is used where it would be copied (bound to a new
    /// variable, passed by value, returned, …). Mojo move-only semantics: transfer
    /// it with `^`, or make the type `Copyable`. `ty` is the type; `context` is the
    /// site (e.g. `variable 'b'`).
    NonCopyable {
        ty: String,
        context: String,
    },
    /// Aliasing violation: a variable is borrowed mutably (`mut`/`ref`) and also
    /// borrowed (mutably or shared) at the same call — Mojo's borrow rule is
    /// mutable-XOR-shared. E.g. `f(mut a, mut a)` or `f(mut a, a)`.
    AliasingViolation {
        var: String,
    },
    /// A name used in call position is not function-typed.
    NotCallable {
        name: String,
        ty: String,
    },
    ArityMismatch {
        name: String,
        expected: usize,
        got: usize,
    },
    /// Re-declaring a name already bound in the same scope. Mojo rejects this;
    /// the evaluator used to silently overwrite the binding.
    Redeclaration(String),
    /// A function value tried to escape by being returned (downward funargs
    /// only). The static counterpart of `RuntimeError::ClosureEscape`.
    ClosureEscape,
    /// `return` appearing outside any function body.
    ReturnOutsideFunction,
    /// `break` appearing outside any loop.
    BreakOutsideLoop,
    /// `continue` appearing outside any loop.
    ContinueOutsideLoop,
    /// An inferred type did not match the one required by its context (a
    /// variable annotation, a declared return type, or a parameter type).
    TypeMismatch {
        expected: String,
        found: String,
        context: String,
    },
    /// An operator applied to operand type(s) it is not defined for.
    BadOperator {
        op: String,
        operands: String,
    },
    /// A type annotation named an identifier that is not a known type/struct.
    UnknownType(String),
    /// Field access on a value whose type has no such field.
    NoSuchField {
        object_type: String,
        field: String,
    },
    /// Method call on a value whose type has no such method.
    NoSuchMethod {
        object_type: String,
        method: String,
    },
    /// Constructing a struct that has no constructor (no `@fieldwise_init`).
    NoConstructor(String),
    /// An `out self` lifecycle method (`__init__`/`__copyinit__`/`__moveinit__`)
    /// leaves a declared field unassigned (definite initialization: every field must
    /// be initialized in the body).
    UninitializedField {
        struct_name: String,
        method: String,
        field: String,
    },
    /// A struct declares both `@fieldwise_init` and a hand-written `__init__`
    /// (each defines a constructor — the decorator *generates* `__init__`).
    ConflictingConstructor(String),
    /// A type-parameter bound named a trait that is not a recognized built-in
    /// (user-defined traits are not supported yet).
    UnknownTrait(String),
    /// A parameterized type was applied to the wrong number of type arguments
    /// (e.g. `Pair[Int, Int]` for a one-parameter `Pair`, or type arguments on a
    /// non-generic type).
    WrongTypeArgCount {
        name: String,
        expected: usize,
        got: usize,
    },
    /// `Self.T` used where `T` is not a type parameter of the enclosing struct
    /// (or outside any struct).
    UnknownSelfParam(String),
    /// A generic call/construction could not solve a type parameter from the
    /// argument types (no explicit type-argument syntax exists to supply it).
    CannotInferTypeParam {
        name: String,
        param: String,
    },
    /// A solved type argument does not conform to a type parameter's declared
    /// trait bound (`f[T: Quackable](...)` called with a non-`Quackable` type).
    TraitNotSatisfied {
        param: String,
        ty: String,
        trait_name: String,
    },
    /// A struct declares conformance to a trait but is missing a required method.
    MissingTraitMethod {
        struct_name: String,
        trait_name: String,
        method: String,
    },
    /// A struct's method exists but does not match the trait's required signature.
    TraitMethodMismatch {
        struct_name: String,
        trait_name: String,
        method: String,
    },
    /// A value parameter was declared with a type other than `Int` (the only
    /// value-parameter type supported).
    BadValueParamType {
        name: String,
        ty: String,
    },
    /// An expression used in a compile-time position (`comptime NAME = …`, or a
    /// value-parameter argument) is not a constant `Int` expression.
    NotComptime(String),
    /// A `SIMD` element-type argument was not a recognized `DType.<name>`.
    BadDtype(String),
    /// A SIMD width was not a positive power of two.
    BadSimdWidth(String),
    /// A SIMD construction had the wrong number of element arguments (must be the
    /// width, or exactly one to splat).
    SimdArity {
        width: i64,
        got: usize,
    },
    /// A subscript `v[i]` was applied to a non-SIMD value.
    NotIndexable(String),
    /// A function with a non-`None` return type can fall off the end without
    /// returning (does not return on every path).
    MissingReturn(String),
    /// A mutating `List` method (`append`/`pop`) was called on something other
    /// than a plain list variable (mojo-lite has no general member-write, so the
    /// receiver must be a variable whose list can be mutated in place).
    MutationRequiresVariable(String),
    /// A field of `self` was written (`self.x = e`) in a method whose receiver is
    /// a read-only `self`. Mutating `self` requires the `mut self` convention.
    ImmutableSelf,
    /// The left side of an assignment is not a valid place (a variable, or a
    /// field/index chain rooted at one).
    InvalidAssignTarget(String),
    /// A valid-Mojo construct that mojo-lite **parses** (and the AST carries) but
    /// does not implement — flagged at check time because it can't be
    /// meaningfully type-checked (e.g. a `def` with `*args`, `**kwargs`, argument
    /// conventions, or `/`/`*` markers; a keyword-argument call to a method).
    /// Carries a message describing the feature. The runtime analogue is
    /// `RuntimeError::Unsupported`.
    Unsupported(String),
    /// A call whose arguments don't match the callee's parameters in a way arity
    /// alone doesn't capture: an unknown keyword name, a parameter bound twice
    /// (positionally and by keyword, or a duplicate keyword), or a required
    /// parameter left unbound. `reason` describes the specific problem.
    BadCall {
        func: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeError {
    UndefinedVariable(String),
    TypeError(String),
    NotCallable(String),
    ArityMismatch {
        name: String,
        expected: usize,
        got: usize,
    },
    /// A closure value tried to escape its defining scope (e.g. by being
    /// returned). Mojo does not support escaping closures — downward funargs
    /// only. See the strict-subset design notes.
    ClosureEscape,
    /// An error raised by `raise` that was not caught by any `try`/`except`. It
    /// propagates through the evaluator via `?` and, if it reaches the top, is
    /// reported here. Carries the error message.
    Raised(String),
    /// A valid-Mojo construct that mojo-lite **parses** (and the AST/checker carry)
    /// but the evaluator does not implement yet — the "parse now, run later"
    /// gaps (e.g. `var`-less variable introduction, the walrus operator `:=`).
    /// Carries a message describing the feature.
    Unsupported(String),
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexError::IndentationError(pos) => write!(f, "Indentation error at byte {}", pos),
            LexError::UnmatchedParenthesis(pos) => {
                write!(f, "Unmatched closing parenthesis at byte {}", pos)
            }
            LexError::UnexpectedCharacter(c, pos) => {
                write!(f, "Unexpected character '{}' at byte {}", c, pos)
            }
            LexError::InvalidInteger(pos) => {
                write!(f, "Invalid integer literal starting at byte {}", pos)
            }
            LexError::InvalidFloat(pos) => {
                write!(f, "Invalid float literal starting at byte {}", pos)
            }
            LexError::UnterminatedString(pos) => {
                write!(f, "Unterminated string literal starting at byte {}", pos)
            }
            LexError::InvalidEscape(c, pos) => {
                write!(f, "Invalid string escape '\\{}' at byte {}", c, pos)
            }
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::LexerError(err) => write!(f, "Lexer error: {}", err),
            ParseError::UnexpectedToken(token, msg) => {
                write!(f, "Unexpected token {:?}: {}", token, msg)
            }
            ParseError::UnexpectedEof(msg) => write!(f, "Unexpected EOF: {}", msg),
            ParseError::UnknownType(name) => write!(f, "Unknown type '{}'", name),
        }
    }
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::UndefinedVariable(name) => write!(f, "Undefined variable '{}'", name),
            TypeError::NonCopyable { ty, context } => write!(
                f,
                "cannot copy non-Copyable type '{ty}' ({context}); transfer it with '^' \
                 or make '{ty}' Copyable"
            ),
            TypeError::AliasingViolation { var } => write!(
                f,
                "'{var}' is borrowed mutably and also used at the same call \
                 (a mutable borrow must be exclusive)"
            ),
            TypeError::NotCallable { name, ty } => {
                write!(f, "'{}' has type {} and is not callable", name, ty)
            }
            TypeError::ArityMismatch {
                name,
                expected,
                got,
            } => write!(
                f,
                "'{}' expects {} argument(s), got {}",
                name, expected, got
            ),
            TypeError::Redeclaration(name) => {
                write!(f, "'{}' is already declared in this scope", name)
            }
            TypeError::ClosureEscape => write!(
                f,
                "closures cannot escape their defining scope (downward funargs only)"
            ),
            TypeError::ReturnOutsideFunction => write!(f, "'return' outside of a function"),
            TypeError::BreakOutsideLoop => write!(f, "'break' outside of a loop"),
            TypeError::ContinueOutsideLoop => write!(f, "'continue' outside of a loop"),
            TypeError::TypeMismatch {
                expected,
                found,
                context,
            } => write!(
                f,
                "type mismatch for {}: expected {}, found {}",
                context, expected, found
            ),
            TypeError::BadOperator { op, operands } => {
                write!(f, "operator '{}' is not defined for {}", op, operands)
            }
            TypeError::UnknownType(name) => write!(f, "unknown type '{}'", name),
            TypeError::NoSuchField { object_type, field } => {
                write!(f, "type '{}' has no field '{}'", object_type, field)
            }
            TypeError::NoSuchMethod {
                object_type,
                method,
            } => {
                write!(f, "type '{}' has no method '{}'", object_type, method)
            }
            TypeError::NoConstructor(name) => {
                write!(
                    f,
                    "struct '{}' has no constructor (add @fieldwise_init)",
                    name
                )
            }
            TypeError::UninitializedField { struct_name, method, field } => {
                write!(
                    f,
                    "'{struct_name}.{method}' does not initialize field '{field}'"
                )
            }
            TypeError::ConflictingConstructor(name) => {
                write!(
                    f,
                    "struct '{name}' has both @fieldwise_init and a hand-written __init__"
                )
            }
            TypeError::UnknownTrait(name) => {
                write!(f, "unknown trait '{}' in a type-parameter bound", name)
            }
            TypeError::WrongTypeArgCount {
                name,
                expected,
                got,
            } => write!(
                f,
                "type '{}' expects {} type argument(s), got {}",
                name, expected, got
            ),
            TypeError::UnknownSelfParam(name) => {
                write!(
                    f,
                    "'Self.{}' is not a type parameter of the enclosing struct",
                    name
                )
            }
            TypeError::CannotInferTypeParam { name, param } => write!(
                f,
                "cannot infer type parameter '{}' of '{}' from the arguments",
                param, name
            ),
            TypeError::TraitNotSatisfied {
                param,
                ty,
                trait_name,
            } => write!(
                f,
                "type '{}' for parameter '{}' does not conform to trait '{}'",
                ty, param, trait_name
            ),
            TypeError::MissingTraitMethod {
                struct_name,
                trait_name,
                method,
            } => write!(
                f,
                "struct '{}' declares conformance to trait '{}' but is missing method '{}'",
                struct_name, trait_name, method
            ),
            TypeError::TraitMethodMismatch {
                struct_name,
                trait_name,
                method,
            } => write!(
                f,
                "struct '{}' method '{}' does not match the signature required by trait '{}'",
                struct_name, method, trait_name
            ),
            TypeError::BadValueParamType { name, ty } => write!(
                f,
                "value parameter '{}' must have type Int, not '{}'",
                name, ty
            ),
            TypeError::NotComptime(what) => {
                write!(f, "not a compile-time Int constant: {}", what)
            }
            TypeError::BadDtype(what) => write!(f, "not a valid SIMD element type: {}", what),
            TypeError::BadSimdWidth(w) => {
                write!(f, "SIMD width must be a positive power of two, got {}", w)
            }
            TypeError::SimdArity { width, got } => write!(
                f,
                "SIMD construction expects {} element(s) or 1 to splat, got {}",
                width, got
            ),
            TypeError::NotIndexable(ty) => {
                write!(f, "type '{}' cannot be indexed here", ty)
            }
            TypeError::MissingReturn(name) => {
                write!(f, "'{}' does not return a value on every path", name)
            }
            TypeError::MutationRequiresVariable(method) => write!(
                f,
                "'{}' must be called on a plain list variable (mutating a temporary or field is not supported)",
                method
            ),
            TypeError::ImmutableSelf => write!(
                f,
                "cannot assign to a field of 'self' in a method with a read-only receiver (use 'mut self')"
            ),
            TypeError::InvalidAssignTarget(what) => {
                write!(f, "invalid assignment target: {}", what)
            }
            TypeError::Unsupported(what) => write!(f, "unsupported feature: {}", what),
            TypeError::BadCall { func, reason } => {
                write!(f, "call to '{}': {}", func, reason)
            }
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::UndefinedVariable(name) => write!(f, "Undefined variable '{}'", name),
            RuntimeError::TypeError(msg) => write!(f, "Type error: {}", msg),
            RuntimeError::NotCallable(name) => write!(f, "'{}' is not callable", name),
            RuntimeError::ArityMismatch {
                name,
                expected,
                got,
            } => write!(
                f,
                "'{}' expects {} argument(s), got {}",
                name, expected, got
            ),
            RuntimeError::ClosureEscape => write!(
                f,
                "closures cannot escape their defining scope (downward funargs only)"
            ),
            RuntimeError::Raised(msg) => write!(f, "unhandled error: {}", msg),
            RuntimeError::Unsupported(what) => write!(f, "unsupported feature: {}", what),
        }
    }
}

/// Errors from the ownership analysis (`analysis`), a compiler pass over the MIR
/// that runs after type-checking. These model Mojo's move semantics — a value
/// transferred with `^` is left uninitialized, so using it again is an error.
/// Each carries the source `Span` (byte range) of the offending use, recovered
/// from the MIR `SpanTable`.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnershipError {
    /// A variable is used after it was transferred (`x^`) on every path to here.
    UseAfterMove { var: String, span: (usize, usize) },
    /// A variable is used after being transferred on *some* (not all) paths — a
    /// move inside one branch of an `if`, then a use after the merge.
    ConditionallyMoved { var: String, span: (usize, usize) },
}

impl fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnershipError::UseAfterMove { var, .. } => {
                write!(f, "use of '{var}' after it was transferred (moved) with '^'")
            }
            OwnershipError::ConditionallyMoved { var, .. } => write!(
                f,
                "'{var}' is used here but may have been transferred (moved) on some paths"
            ),
        }
    }
}

impl OwnershipError {
    /// The source span (byte range) of the offending use.
    pub fn span(&self) -> (usize, usize) {
        match self {
            OwnershipError::UseAfterMove { span, .. }
            | OwnershipError::ConditionallyMoved { span, .. } => *span,
        }
    }
}

impl std::error::Error for LexError {}
impl std::error::Error for ParseError {}
impl std::error::Error for TypeError {}
impl std::error::Error for RuntimeError {}
impl std::error::Error for OwnershipError {}
