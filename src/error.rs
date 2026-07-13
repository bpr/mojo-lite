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
    UnterminatedIdentifier(usize),
    InvalidEscape(char, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    LexerError(LexError),
    UnexpectedToken(Token, String),
    UnexpectedEof(String),
    UnknownType(String),
    At {
        err: Box<ParseError>,
        span: crate::token::Span,
    },
}

impl LexError {
    pub fn byte_pos(&self) -> usize {
        match self {
            LexError::IndentationError(pos)
            | LexError::UnmatchedParenthesis(pos)
            | LexError::UnexpectedCharacter(_, pos)
            | LexError::InvalidInteger(pos)
            | LexError::InvalidFloat(pos)
            | LexError::UnterminatedString(pos)
            | LexError::UnterminatedIdentifier(pos)
            | LexError::InvalidEscape(_, pos) => *pos,
        }
    }
}

impl ParseError {
    pub fn at(self, span: crate::token::Span) -> Self {
        if self.byte_pos().is_some() {
            self
        } else {
            ParseError::At {
                err: Box::new(self),
                span,
            }
        }
    }

    pub fn byte_pos(&self) -> Option<usize> {
        match self {
            ParseError::LexerError(err) => Some(err.byte_pos()),
            ParseError::At { span, .. } => Some(span.0),
            ParseError::UnexpectedToken(_, _)
            | ParseError::UnexpectedEof(_)
            | ParseError::UnknownType(_) => None,
        }
    }
}

/// Errors from semantic checking, produced before HIR/MIR lowering. These cover
/// type, declaration, call, trait, convention, and locally decidable borrow rules.
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
    /// Re-declaring a name already bound in the same scope.
    Redeclaration(String),
    /// Assignment attempted through an immutable binding, such as an ordinary
    /// function parameter. Mojo function arguments are immutable unless their
    /// convention makes them writable (`mut`, `ref`, `out`).
    ImmutableBinding(String),
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
    /// Associated type/comptime member lookup in type position failed.
    NoSuchAssociatedType {
        object_type: String,
        member: String,
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
        /// The first concrete missing field or operation, when the checker can
        /// identify one without obscuring the primary bound failure.
        reason: Option<String>,
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
    /// A struct declares conformance to a trait but is missing a required
    /// associated compile-time member.
    MissingTraitComptimeMember {
        struct_name: String,
        trait_name: String,
        member: String,
    },
    /// A struct's associated compile-time member exists but has the wrong
    /// compile-time kind or type for the trait requirement.
    TraitComptimeMemberMismatch {
        struct_name: String,
        trait_name: String,
        member: String,
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
    /// than a plain list variable (mojito has no general member-write, so the
    /// receiver must be a variable whose list can be mutated in place).
    MutationRequiresVariable(String),
    /// A field of `self` was written (`self.x = e`) in a method whose receiver is
    /// a read-only `self`. Mutating `self` requires the `mut self` convention.
    ImmutableSelf,
    /// The left side of an assignment is not a valid place (a variable, or a
    /// field/index chain rooted at one).
    InvalidAssignTarget(String),
    /// A valid-Mojo construct that mojito **parses** (and the AST carries) but
    /// does not implement — flagged at check time because it can't be
    /// meaningfully type-checked (e.g. a `def` with `*args`, `**kwargs`, argument
    /// conventions, or `/`/`*` markers; a keyword-argument call to a method).
    /// Carries a message describing the feature. The runtime analogue is
    /// `RuntimeError::Unsupported`.
    Unsupported(String),
    /// A compiler phase received state that violates a contract established by
    /// an earlier phase. This is a Mojito bug, not an error in the source file.
    InvariantViolation(String),
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
    /// propagates through VM execution and is reported if it reaches the top.
    Raised(String),
    /// A valid-Mojo construct that reaches a compiler/backend boundary whose
    /// semantics Mojito does not implement. Carries a feature description.
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
            LexError::UnterminatedIdentifier(pos) => {
                write!(
                    f,
                    "Unterminated backtick identifier starting at byte {}",
                    pos
                )
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
            ParseError::At { err, span } => write!(f, "{err} at byte {}", span.0),
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
            } => {
                write!(
                    f,
                    "'{}' expects {} argument(s), got {}",
                    name, expected, got
                )
            }
            TypeError::Redeclaration(name) => {
                write!(f, "'{}' is already declared in this scope", name)
            }
            TypeError::ImmutableBinding(name) => {
                write!(f, "expression must be mutable in assignment ('{name}')")
            }
            TypeError::ClosureEscape => {
                write!(
                    f,
                    "closures cannot escape their defining scope (downward funargs only)"
                )
            }
            TypeError::ReturnOutsideFunction => write!(f, "'return' outside of a function"),
            TypeError::BreakOutsideLoop => write!(f, "'break' outside of a loop"),
            TypeError::ContinueOutsideLoop => write!(f, "'continue' outside of a loop"),
            TypeError::TypeMismatch {
                expected,
                found,
                context,
            } => {
                write!(
                    f,
                    "type mismatch for {}: expected {}, found {}",
                    context, expected, found
                )
            }
            TypeError::BadOperator { op, operands } => {
                write!(f, "operator '{}' is not defined for {}", op, operands)
            }
            TypeError::UnknownType(name) => write!(f, "unknown type '{}'", name),
            TypeError::NoSuchField { object_type, field } => {
                write!(f, "type '{}' has no field '{}'", object_type, field)
            }
            TypeError::NoSuchAssociatedType {
                object_type,
                member,
            } => {
                write!(
                    f,
                    "type '{}' has no associated type '{}'",
                    object_type, member
                )
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
            TypeError::UninitializedField {
                struct_name,
                method,
                field,
            } => {
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
            } => {
                write!(
                    f,
                    "type '{}' expects {} type argument(s), got {}",
                    name, expected, got
                )
            }
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
                reason,
            } => {
                write!(
                    f,
                    "type '{}' for parameter '{}' does not conform to trait '{}'",
                    ty, param, trait_name
                )?;
                if let Some(reason) = reason {
                    write!(f, ": {reason}")?;
                }
                Ok(())
            }
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
            TypeError::MissingTraitComptimeMember {
                struct_name,
                trait_name,
                member,
            } => write!(
                f,
                "struct '{}' declares conformance to trait '{}' but is missing comptime member '{}'",
                struct_name, trait_name, member
            ),
            TypeError::TraitComptimeMemberMismatch {
                struct_name,
                trait_name,
                member,
            } => write!(
                f,
                "struct '{}' comptime member '{}' does not match the requirement from trait '{}'",
                struct_name, member, trait_name
            ),
            TypeError::BadValueParamType { name, ty } => {
                write!(
                    f,
                    "value parameter '{}' must have type Int, not '{}'",
                    name, ty
                )
            }
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
            TypeError::InvariantViolation(detail) => {
                write!(f, "compiler invariant violated: {detail}")
            }
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
            } => {
                write!(
                    f,
                    "'{}' expects {} argument(s), got {}",
                    name, expected, got
                )
            }
            RuntimeError::ClosureEscape => {
                write!(
                    f,
                    "closures cannot escape their defining scope (downward funargs only)"
                )
            }
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
    /// Ownership analysis was requested for a program that did not pass semantic
    /// checking. The production compiler reports the earlier error directly;
    /// this variant protects the compatibility API from panicking.
    InvalidInput(String),
    /// A variable is used after it was transferred (`x^`) on every path to here.
    UseAfterMove {
        var: String,
        span: crate::token::SourceSpan,
    },
    /// A variable is used after being transferred on *some* (not all) paths — a
    /// move inside one branch of an `if`, then a use after the merge.
    ConditionallyMoved {
        var: String,
        span: crate::token::SourceSpan,
    },
}

impl fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnershipError::InvalidInput(error) => {
                write!(f, "ownership analysis requires a checked program: {error}")
            }
            OwnershipError::UseAfterMove { var, .. } => {
                write!(
                    f,
                    "use of '{var}' after it was transferred (moved) with '^'"
                )
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
            OwnershipError::InvalidInput(_) => crate::token::DUMMY_SPAN,
            OwnershipError::UseAfterMove { span, .. }
            | OwnershipError::ConditionallyMoved { span, .. } => span.span,
        }
    }

    pub fn source(&self) -> Option<&str> {
        match self {
            OwnershipError::InvalidInput(_) => None,
            OwnershipError::UseAfterMove { span, .. }
            | OwnershipError::ConditionallyMoved { span, .. } => span.source.as_deref(),
        }
    }
}

impl std::error::Error for LexError {}
impl std::error::Error for ParseError {}
impl std::error::Error for TypeError {}
impl std::error::Error for RuntimeError {}
impl std::error::Error for OwnershipError {}
