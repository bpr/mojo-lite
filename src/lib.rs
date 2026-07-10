pub mod analysis;
pub mod ast;
pub mod backend;
pub mod checker;
pub mod comptime;
pub mod ct;
pub mod error;
pub mod hir;
pub mod lexer;
pub mod mir;
pub mod module;
pub mod parser;
pub mod runtime;
pub mod token;
pub mod types;

// Re-export commonly used types at the crate root for convenience
pub use analysis::check_ownership;
pub use ast::{
    Dtype, Expr, ImportName, ImportNames, InfixOp, Param, PrefixOp, Stmt, Type, TypeParam,
};
pub use backend::{Backend, BackendKind};
pub use checker::{Checker, check};
pub use comptime::{ComptimeError, elaborate};
pub use ct::CtValue;
pub use error::{LexError, OwnershipError, ParseError, RuntimeError, TypeError};
pub use lexer::Lexer;
pub use module::{
    LinkOptions, ModuleError, link, link_source, link_source_with_options, link_with_options,
};
pub use parser::Parser;
pub use runtime::Value;
pub use token::Token;
pub use types::{ParamDecl, Ty, TyArg};

/// Lex `source` into its full token stream (a convenience for the **lex-only**
/// use of mojo-lite as a syntax-analysis tool). Stops at the first `LexError`.
pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).map(|r| r.map(|(t, _)| t)).collect()
}

/// Parse `source` into the program AST (the **parse-only** front end — no type
/// checking or evaluation). Surfaces lexer errors as `ParseError::LexerError`.
pub fn parse(source: &str) -> Result<Vec<Stmt>, ParseError> {
    Parser::new(Lexer::new(source)).parse_program()
}
