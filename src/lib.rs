pub mod ast;
pub mod checker;
pub mod error;
pub mod evaluator;
pub mod lexer;
pub mod parser;
pub mod token;

// Re-export commonly used types at the crate root for convenience
pub use ast::{Dtype, Expr, ImportName, ImportNames, InfixOp, Param, PrefixOp, Stmt, Type, TypeParam};
pub use checker::{check, Checker};
pub use error::{LexError, ParseError, RuntimeError, TypeError};
pub use evaluator::{Closure, Evaluator, Value};
pub use lexer::Lexer;
pub use parser::Parser;
pub use token::Token;

/// Lex `source` into its full token stream (a convenience for the **lex-only**
/// use of mojo-lite as a syntax-analysis tool). Stops at the first `LexError`.
pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).collect()
}

/// Parse `source` into the program AST (the **parse-only** front end — no type
/// checking or evaluation). Surfaces lexer errors as `ParseError::LexerError`.
pub fn parse(source: &str) -> Result<Vec<Stmt>, ParseError> {
    Parser::new(Lexer::new(source)).parse_program()
}
