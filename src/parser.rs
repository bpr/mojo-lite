use std::iter::Peekable;

use crate::ast::{
    ArgConvention, Decorator, Expr, ExprKind, FnParam, InfixOp, KwArg, Method, Param, ParamKind,
    PrefixOp, Stmt, StmtKind, TStringPart, Type, WithItem,
};
use crate::error::{LexError, ParseError};
use crate::lexer::Lexer;
use crate::token::{Span, TStringChunk, Token};

/// A parsed parameter list: the parameters plus the positions of the `/`
/// (positional-only) and bare `*` (keyword-only) markers, if present.
struct ParamList {
    params: Vec<FnParam>,
    positional_only: Option<usize>,
    keyword_only: Option<usize>,
}

/// The parsed body of an `if`/`elif`/`else` chain: the `(condition, body)`
/// branches plus the optional `else` body. Shared by `if` and `comptime if`.
type IfChain = (Vec<(Expr, Vec<Stmt>)>, Option<Vec<Stmt>>);

/// Binding-power levels, lowest to highest. Mirrors Python/Mojo expression
/// precedence for the implemented operator set (no bitwise / `**` yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    Lowest,
    Walrus,      // name := value  (binds looser than everything else)
    Conditional, // a if c else b  (ternary; looser than `or`, tighter than walrus)
    Or,          // or
    And,         // and
    Not,         // not x  (prefix)
    Comparison,  // == != < > <= >=
    Sum,         // + -
    Product,     // * / // %
    Unary,       // -x  (prefix)
    Power,       // **  (right-associative, binds tighter than unary -)
    Call,        // f(...)  .field  .method(...)
}

// --- Recursive Descent + Pratt Parser ---

pub struct Parser<I: Iterator<Item = Result<(Token, Span), LexError>>> {
    tokens: Peekable<I>,
    /// Span of the most recently consumed token. Together with a `start` mark
    /// captured before a node begins, this yields each AST node's span (a node
    /// spans from its first token's start to its last token's end — see `node`).
    last_span: Span,
    /// End offset of the last *significant* token — i.e. excluding the layout
    /// tokens (`Newline`/`Indent`/`Dedent`/`Eof`). Used for statement spans, so a
    /// statement doesn't swallow the trailing newline `expect_stmt_end` consumes.
    last_significant_end: usize,
    /// Whether the most recent statement terminator was `;` rather than a newline
    /// or EOF. Used for one-line suites like `def f(): a(); b()`.
    last_stmt_ended_with_semicolon: bool,
}

impl<I: Iterator<Item = Result<(Token, Span), LexError>>> Parser<I> {
    pub fn new(tokens: I) -> Self {
        Self {
            tokens: tokens.peekable(),
            last_span: (0, 0),
            last_significant_end: 0,
            last_stmt_ended_with_semicolon: false,
        }
    }

    /// Helper to get the next token, propagating errors. Records the consumed
    /// token's span in `self.last_span` (and its end in `last_significant_end`
    /// unless it is a layout token).
    fn next_token(&mut self) -> Result<Token, ParseError> {
        match self.tokens.next() {
            Some(Ok((token, span))) => {
                self.last_span = span;
                if !matches!(
                    token,
                    Token::Newline | Token::Indent | Token::Dedent | Token::Eof
                ) {
                    self.last_significant_end = span.1;
                }
                Ok(token)
            }
            Some(Err(err)) => Err(ParseError::LexerError(err)),
            None => Err(ParseError::UnexpectedEof(
                "Expected a token, found EOF".into(),
            )),
        }
    }

    /// Helper to peek at the next token without consuming it
    fn peek_token(&mut self) -> Result<Option<&Token>, ParseError> {
        match self.tokens.peek() {
            Some(Ok((token, _))) => Ok(Some(token)),
            Some(Err(err)) => Err(ParseError::LexerError(err.clone())),
            None => Ok(None),
        }
    }

    /// The start byte offset of the next (unconsumed) token, or the end of the
    /// last consumed token at end of input. Used as a node's span start.
    fn peek_start(&mut self) -> usize {
        match self.tokens.peek() {
            Some(Ok((_, span))) => span.0,
            _ => self.last_span.1,
        }
    }

    /// Build a spanned expression: `kind` spanning from `start` (its first token's
    /// start offset) to the end of the most recently consumed token.
    fn node(&self, kind: ExprKind, start: usize) -> Expr {
        Expr {
            kind,
            span: (start, self.last_span.1),
        }
    }

    /// Consumes a token and ensures it matches the expected one
    fn expect(&mut self, expected: Token, context_msg: &str) -> Result<(), ParseError> {
        let token = self.next_token()?;
        if token == expected {
            Ok(())
        } else {
            Err(ParseError::UnexpectedToken(token, context_msg.to_string()))
        }
    }

    /// Consumes the next token, requiring it to be an identifier, and returns its text.
    fn expect_identifier(&mut self, context_msg: &str) -> Result<String, ParseError> {
        match self.next_token()? {
            Token::Identifier(id) => Ok(id),
            token => Err(ParseError::UnexpectedToken(token, context_msg.to_string())),
        }
    }

    /// Parses the top-level program
    pub fn parse_program(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();

        while let Some(token) = self.peek_token()? {
            match token {
                Token::Eof => break,
                Token::Newline => {
                    self.next_token()?; // Ignore empty lines at the top level
                }
                _ => {
                    stmts.push(self.parse_statement()?);
                }
            }
        }

        Ok(stmts)
    }

    // --- Statements ---

    /// Parses a single statement.
    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        // A statement spans from its first token to the last token consumed. Each
        // sub-parser returns a bare `StmtKind`; the span is stamped once, here.
        let start = self.peek_start();
        let kind = (|| -> Result<StmtKind, ParseError> {
            Ok(match self.peek_token()? {
                Some(Token::Var) => self.parse_var_decl()?,
                Some(Token::Def) => self.parse_def(Vec::new())?,
                Some(Token::Struct) => self.parse_struct(Vec::new())?,
                // A decorator list precedes a `def` or `struct`.
                Some(Token::At) => {
                    let decorators = self.parse_decorators()?;
                    match self.peek_token()? {
                        Some(Token::Def) => self.parse_def(decorators)?,
                        Some(Token::Struct) => self.parse_struct(decorators)?,
                        other => {
                            return Err(ParseError::UnexpectedToken(
                                other.cloned().unwrap_or(Token::Eof),
                                "a decorator must precede a 'def' or 'struct'".into(),
                            ));
                        }
                    }
                }
                Some(Token::Trait) => self.parse_trait()?,
                Some(Token::Comptime) => self.parse_comptime()?,
                Some(Token::If) => self.parse_if()?,
                Some(Token::While) => self.parse_while()?,
                Some(Token::For) => self.parse_for()?,
                Some(Token::With) => self.parse_with()?,
                Some(Token::Try) => self.parse_try()?,
                Some(Token::Return) => self.parse_return()?,
                Some(Token::Raise) => self.parse_raise()?,
                Some(Token::Import) => self.parse_import()?,
                Some(Token::From) => self.parse_from_import()?,
                Some(Token::Pass) => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
                    StmtKind::Pass
                }
                Some(Token::Break) => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
                    StmtKind::Break
                }
                Some(Token::Continue) => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
                    StmtKind::Continue
                }
                Some(Token::Ellipsis) => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
                    StmtKind::Pass
                }
                _ => self.parse_expr_or_assign()?,
            })
        })()
        .map_err(|err| err.at(self.last_span))?;
        // End at the last significant token so the trailing newline (consumed by
        // `expect_stmt_end`) isn't included in the statement's span.
        Ok(Stmt::new(kind, (start, self.last_significant_end)))
    }

    /// A bare expression statement, or an assignment `target = value`. The two
    /// share a leading expression, so we parse that first and then look for `=`.
    /// A target is a variable (`x = e`) or a **place** — a field/index chain
    /// rooted at a variable (`p.x = e`, `xs[i] = e`, `p.items[i].x = e`). A
    /// top-level comma after the first target starts a **tuple unpacking**
    /// (`a, b = t`).
    fn parse_expr_or_assign(&mut self) -> Result<StmtKind, ParseError> {
        let expr = self.parse_expression(Precedence::Lowest)?;

        // Tuple-unpacking target list: `a, b, … = value`. A top-level comma
        // (the Pratt parser never consumes one) after the first target starts an
        // unpack; collect the remaining comma-separated targets, then require `=`.
        if matches!(self.peek_token()?, Some(Token::Comma)) {
            let mut targets = vec![expr];
            while matches!(self.peek_token()?, Some(Token::Comma)) {
                // A trailing comma before `=` is allowed (`a, = t`, `a, b, = t`).
                self.next_token()?; // consume ','
                if matches!(self.peek_token()?, Some(Token::Assign)) {
                    break;
                }
                targets.push(self.parse_expression(Precedence::Lowest)?);
            }
            for target in &targets {
                if !matches!(
                    target.kind,
                    ExprKind::Identifier(_) | ExprKind::Member { .. } | ExprKind::Index { .. }
                ) {
                    return Err(ParseError::UnexpectedToken(
                        Token::Comma,
                        format!("invalid unpacking target: {:?}", target.kind),
                    ));
                }
            }
            self.expect(
                Token::Assign,
                "Expected '=' after the unpacking target list",
            )?;
            let value = self.parse_expression(Precedence::Lowest)?;
            self.expect_stmt_end()?;
            return Ok(StmtKind::Unpack { targets, value });
        }

        if matches!(self.peek_token()?, Some(Token::Assign)) {
            self.next_token()?; // consume '='
            let value = self.parse_expression(Precedence::Lowest)?;
            let stmt = if matches!(expr.kind, ExprKind::Identifier(_)) {
                let ExprKind::Identifier(name) = expr.kind else {
                    unreachable!()
                };
                StmtKind::Assign { name, value }
            } else if matches!(expr.kind, ExprKind::Member { .. } | ExprKind::Index { .. }) {
                // A field/index chain — the checker verifies its root is a
                // mutable variable (or `mut self`) and that the write is valid.
                StmtKind::SetPlace { place: expr, value }
            } else {
                return Err(ParseError::UnexpectedToken(
                    Token::Assign,
                    format!("invalid assignment target: {:?}", expr.kind),
                ));
            };
            self.expect_stmt_end()?;
            return Ok(stmt);
        }

        // Augmented assignment `target OP= value` (target is a NAME or place).
        if let Some(op) = self.peek_token()?.and_then(aug_assign_op) {
            self.next_token()?; // consume the `OP=` token
            if !matches!(
                expr.kind,
                ExprKind::Identifier(_) | ExprKind::Member { .. } | ExprKind::Index { .. }
            ) {
                return Err(ParseError::UnexpectedToken(
                    Token::Assign,
                    format!("invalid augmented-assignment target: {:?}", expr.kind),
                ));
            }
            let value = self.parse_expression(Precedence::Lowest)?;
            self.expect_stmt_end()?;
            return Ok(StmtKind::AugAssign {
                place: expr,
                op,
                value,
            });
        }

        self.expect_stmt_end()?;
        Ok(StmtKind::Expr(expr))
    }

    /// `var name[: Type] = value` — the annotation is optional (inferred `var`).
    fn parse_var_decl(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Var, "Statements must begin with a keyword")?;
        let name = self.expect_identifier("Expected identifier after 'var'")?;
        // An optional `: Type`; omitting it infers the type from `value`.
        let ty = if matches!(self.peek_token()?, Some(Token::Colon)) {
            self.next_token()?; // consume ':'
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(
            Token::Assign,
            "Expected '=' after the variable name (or its ': Type')",
        )?;
        let value = self.parse_expression(Precedence::Lowest)?;
        self.expect_stmt_end()?;
        Ok(StmtKind::VarDecl { name, ty, value })
    }

    /// `comptime NAME = value` — a compile-time constant.
    /// `comptime`, which introduces one of three forms: a compile-time constant
    /// `comptime NAME = expr`, a compile-time conditional `comptime if …`, or a
    /// compile-time (unrolled) loop `comptime for …` (Mojo's modern spellings —
    /// the older `@parameter if`/`@parameter for` are deprecated).
    fn parse_comptime(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Comptime, "Expected 'comptime'")?;
        match self.peek_token()? {
            Some(Token::If) => {
                let (branches, orelse) = self.parse_if_rest()?;
                Ok(StmtKind::ComptimeIf { branches, orelse })
            }
            Some(Token::For) => {
                let (var, iter, body) = self.parse_for_rest()?;
                Ok(StmtKind::ComptimeFor { var, iter, body })
            }
            _ => {
                let name =
                    self.expect_identifier("Expected a name, 'if', or 'for' after 'comptime'")?;
                self.expect(
                    Token::Assign,
                    "Expected '=' after the comptime constant name",
                )?;
                let value = self.parse_expression(Precedence::Lowest)?;
                self.expect_stmt_end()?;
                Ok(StmtKind::Comptime { name, value })
            }
        }
    }

    /// `def name(params) -> ret: <block>`
    /// Parses one or more decorators, each on its own line: `@` followed by a
    /// dotted name and optional call arguments. A general grammar — any name is
    /// accepted (only `@fieldwise_init` on a struct is acted on).
    fn parse_decorators(&mut self) -> Result<Vec<Decorator>, ParseError> {
        let mut decorators = Vec::new();
        while matches!(self.peek_token()?, Some(Token::At)) {
            self.next_token()?; // consume '@'
            let mut path = vec![self.expect_identifier("Expected a decorator name after '@'")?];
            while matches!(self.peek_token()?, Some(Token::Dot)) {
                self.next_token()?; // consume '.'
                path.push(self.expect_identifier("Expected a name after '.' in a decorator")?);
            }
            let (args, kwargs) = if matches!(self.peek_token()?, Some(Token::LParen)) {
                self.next_token()?; // consume '('
                let call = self.parse_call_args()?;
                self.expect(Token::RParen, "Expected ')' after decorator arguments")?;
                call
            } else {
                (Vec::new(), Vec::new())
            };
            self.expect_stmt_end()?;
            decorators.push(Decorator { path, args, kwargs });
        }
        Ok(decorators)
    }

    fn parse_def(&mut self, decorators: Vec<Decorator>) -> Result<StmtKind, ParseError> {
        self.expect(Token::Def, "Expected 'def'")?;
        let name = self.expect_identifier("Expected function name after 'def'")?;
        let type_params = self.parse_type_params()?;

        self.expect(Token::LParen, "Expected '(' after function name")?;
        let ParamList {
            params,
            positional_only,
            keyword_only,
        } = self.parse_params()?;
        self.expect(Token::RParen, "Expected ')' after parameters")?;

        let raises = self.parse_raises_effect()?;
        let ret = if matches!(self.peek_token()?, Some(Token::Arrow)) {
            self.next_token()?; // consume '->'
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(Token::Colon, "Expected ':' before the function body")?;
        let body = self.parse_suite()?;

        Ok(StmtKind::Def {
            name,
            decorators,
            type_params,
            params,
            positional_only,
            keyword_only,
            raises,
            ret,
            body,
        })
    }

    /// Parses an optional `raises` effect after a function's parameter list. An
    /// error type may follow (`raises ValidationError`); it is parsed and
    /// discarded (mojito models a single `Error` type). Returns whether the
    /// effect was present.
    fn parse_raises_effect(&mut self) -> Result<bool, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::Raises)) {
            return Ok(false);
        }
        // An optional error type follows, unless the next token ends the header.
        self.next_token()?; // consume 'raises'
        if !matches!(self.peek_token()?, Some(Token::Arrow | Token::Colon)) {
            self.parse_type()?; // discarded
        }
        Ok(true)
    }

    /// `raise expr`
    fn parse_raise(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Raise, "Expected 'raise'")?;
        let value = self.parse_expression(Precedence::Lowest)?;
        self.expect_stmt_end()?;
        Ok(StmtKind::Raise(value))
    }

    /// `import a.b.c [as alias]`
    fn parse_import(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Import, "Expected 'import'")?;
        let path = self.parse_dotted_name()?;
        let alias = self.parse_import_alias()?;
        self.expect_stmt_end()?;
        Ok(StmtKind::Import { path, alias })
    }

    /// `from [.]*module import <targets>`
    fn parse_from_import(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::From, "Expected 'from'")?;
        // Leading dots make the import relative. The lexer tokenizes `...` as one
        // ellipsis, which here counts as three dots.
        let mut level = 0usize;
        loop {
            match self.peek_token()? {
                Some(Token::Dot) => {
                    self.next_token()?;
                    level += 1;
                }
                Some(Token::Ellipsis) => {
                    self.next_token()?;
                    level += 3;
                }
                _ => break,
            }
        }
        // The module path is optional for a dots-only relative import (`from .`).
        let path = if matches!(self.peek_token()?, Some(Token::Identifier(_))) {
            self.parse_dotted_name()?
        } else {
            Vec::new()
        };
        if level == 0 && path.is_empty() {
            return Err(ParseError::UnexpectedToken(
                Token::From,
                "expected a module name after 'from'".into(),
            ));
        }
        self.expect(Token::Import, "Expected 'import' after the module name")?;

        let names = if matches!(self.peek_token()?, Some(Token::Star)) {
            self.next_token()?; // consume '*'
            crate::ast::ImportNames::Wildcard
        } else {
            let mut targets = Vec::new();
            loop {
                let name = self.expect_identifier("Expected an imported name")?;
                let alias = self.parse_import_alias()?;
                targets.push(crate::ast::ImportName { name, alias });
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?; // consume ','
                } else {
                    break;
                }
            }
            crate::ast::ImportNames::Names(targets)
        };
        self.expect_stmt_end()?;
        Ok(StmtKind::FromImport { level, path, names })
    }

    /// Parses a dotted module name `NAME ('.' NAME)*` into its segments.
    fn parse_dotted_name(&mut self) -> Result<Vec<String>, ParseError> {
        let mut segments = vec![self.expect_identifier("Expected a module name")?];
        while matches!(self.peek_token()?, Some(Token::Dot)) {
            self.next_token()?; // consume '.'
            segments.push(self.expect_identifier("Expected a name after '.'")?);
        }
        Ok(segments)
    }

    /// Parses an optional `as NAME` alias.
    fn parse_import_alias(&mut self) -> Result<Option<String>, ParseError> {
        if matches!(self.peek_token()?, Some(Token::As)) {
            self.next_token()?; // consume 'as'
            Ok(Some(
                self.expect_identifier("Expected an alias name after 'as'")?,
            ))
        } else {
            Ok(None)
        }
    }

    /// `try: <block> [except [NAME]: <block>] [else: <block>] [finally: <block>]`
    /// `with item (',' item)*: <block>`, where each `item` is
    /// `expression ['as' NAME]`. Multiple comma-separated managers are allowed;
    /// the `as` binding is optional. The parenthesized / tuple-target forms aren't
    /// in the Mojo docs, so they aren't parsed (strict-subset).
    fn parse_with(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::With, "Expected 'with'")?;
        let mut items = Vec::new();
        loop {
            let context = self.parse_expression(Precedence::Lowest)?;
            let var = if matches!(self.peek_token()?, Some(Token::As)) {
                self.next_token()?; // consume 'as'
                Some(self.expect_identifier("Expected a name after 'as' in a 'with' item")?)
            } else {
                None
            };
            items.push(WithItem { context, var });
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ',' and parse the next manager
            } else {
                break;
            }
        }
        self.expect(Token::Colon, "Expected ':' after the 'with' items")?;
        let body = self.parse_suite()?;
        Ok(StmtKind::With { items, body })
    }

    fn parse_try(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Try, "Expected 'try'")?;
        self.expect(Token::Colon, "Expected ':' after 'try'")?;
        let body = self.parse_suite()?;

        let except = if matches!(self.peek_token()?, Some(Token::Except)) {
            // An optional name binds the caught error.
            self.next_token()?; // consume 'except'
            let name = if matches!(self.peek_token()?, Some(Token::Identifier(_))) {
                Some(self.expect_identifier("unreachable")?)
            } else {
                None
            };
            self.expect(Token::Colon, "Expected ':' after 'except'")?;
            Some((name, self.parse_suite()?))
        } else {
            None
        };

        let orelse = if matches!(self.peek_token()?, Some(Token::Else)) {
            self.next_token()?; // consume 'else'
            self.expect(Token::Colon, "Expected ':' after 'else'")?;
            Some(self.parse_suite()?)
        } else {
            None
        };

        let finalbody = if matches!(self.peek_token()?, Some(Token::Finally)) {
            self.next_token()?; // consume 'finally'
            self.expect(Token::Colon, "Expected ':' after 'finally'")?;
            Some(self.parse_suite()?)
        } else {
            None
        };

        if except.is_none() && finalbody.is_none() {
            return Err(ParseError::UnexpectedToken(
                Token::Try,
                "a 'try' needs at least one of 'except' or 'finally'".into(),
            ));
        }
        Ok(StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        })
    }

    /// Parses a (possibly empty) comma-separated parameter list. The opening
    /// `(` has been consumed; stops at the closing `)` without consuming it.
    /// Parses a parameter list (after the `(`), returning the parameters plus the
    /// positions of the `/` (positional-only) and bare `*` (keyword-only) markers.
    /// Supports every Mojo parameter form — conventions, defaults, `*args`,
    /// `**kwargs`, and the `/`/`*` markers — all **parsed** (the checker flags the
    /// advanced ones as unsupported). Parsing is lenient about argument ordering.
    fn parse_params(&mut self) -> Result<ParamList, ParseError> {
        let mut params = Vec::new();
        let mut positional_only = None;
        let mut keyword_only = None;
        if matches!(self.peek_token()?, Some(Token::RParen)) {
            return Ok(ParamList {
                params,
                positional_only,
                keyword_only,
            });
        }
        loop {
            match self.peek_token()? {
                // `/` — positional-only marker (not a parameter).
                Some(Token::Slash) => {
                    self.next_token()?;
                    positional_only = Some(params.len());
                }
                // `**name: T` — keyword variadic.
                Some(Token::DoubleStar) => {
                    self.next_token()?;
                    let name = self.expect_identifier("Expected a name after '**'")?;
                    params.push(self.finish_param(name, ParamKind::KwVariadic, None)?);
                }
                Some(Token::Star) => {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::Identifier(_))) {
                        // `*name: T` — positional variadic.
                        let name = self.expect_identifier("Expected a name after '*'")?;
                        params.push(self.finish_param(name, ParamKind::Variadic, None)?);
                    } else {
                        // bare `*` — keyword-only marker (not a parameter).
                        keyword_only = Some(params.len());
                    }
                }
                // A regular parameter, with an optional convention prefix.
                _ => {
                    let (convention, name) = self.parse_convention_and_name()?;
                    params.push(self.finish_param(name, ParamKind::Regular, convention)?);
                }
            }
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
                if matches!(self.peek_token()?, Some(Token::RParen)) {
                    break; // trailing comma
                }
            } else {
                break;
            }
        }
        Ok(ParamList {
            params,
            positional_only,
            keyword_only,
        })
    }

    /// The optional argument convention (`read`/`mut`/`owned`/`out`) prefixing a
    /// regular parameter, plus its name. A convention word is only a convention
    /// when followed by the parameter name (another identifier); if it is followed
    /// by `:` it *is* the name (so `read` remains usable as a parameter name).
    fn parse_convention_and_name(&mut self) -> Result<(Option<ArgConvention>, String), ParseError> {
        let word = self.expect_identifier("Expected a parameter name")?;
        // `word :` → `word` is the parameter name, no convention.
        if matches!(self.peek_token()?, Some(Token::Colon)) {
            return Ok((None, word));
        }
        let Some(convention) = convention_word(&word) else {
            return Err(ParseError::UnexpectedToken(
                Token::Identifier(word),
                "expected a parameter name (or a convention: read/mut/owned/out/ref)".into(),
            ));
        };
        // A `ref` convention may carry an origin specifier: `ref[origin] name`.
        if convention == ArgConvention::Ref {
            self.parse_optional_origin_specifier()?;
        }
        let name = self.expect_identifier("Expected a parameter name after the convention")?;
        Ok((Some(convention), name))
    }

    /// An optional `[origin]` origin specifier following `ref` (in a `ref[origin]`
    /// argument convention or `ref[origin] T` return type). The specifier is a
    /// comma-separated list of origin expressions (an arbitrary expression, a named
    /// origin, or `_`); it is **parsed and discarded** — origins are not modeled
    /// (matching the discarded `abi(...)` effect and `raises T` type).
    fn parse_optional_origin_specifier(&mut self) -> Result<(), ParseError> {
        if !matches!(self.peek_token()?, Some(Token::LBracket)) {
            return Ok(());
        }
        self.next_token()?; // consume '['
        loop {
            self.parse_expression(Precedence::Lowest)?; // an origin — discarded
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?;
            } else {
                break;
            }
        }
        self.expect(Token::RBracket, "Expected ']' after the origin specifier")?;
        Ok(())
    }

    /// Finishes a parameter after its name: `: type [= default]`.
    fn finish_param(
        &mut self,
        name: String,
        kind: ParamKind,
        convention: Option<ArgConvention>,
    ) -> Result<FnParam, ParseError> {
        self.expect(Token::Colon, "Parameters require a type annotation")?;
        let ty = self.parse_type()?;
        let default = if matches!(self.peek_token()?, Some(Token::Assign)) {
            self.next_token()?; // consume '='
            Some(self.parse_expression(Precedence::Lowest)?)
        } else {
            None
        };
        Ok(FnParam {
            name,
            ty,
            default,
            kind,
            convention,
        })
    }

    /// `[@fieldwise_init] struct Name: <fields and methods>`
    fn parse_struct(&mut self, decorators: Vec<Decorator>) -> Result<StmtKind, ParseError> {
        // `@fieldwise_init` (the one modeled decorator) generates the constructor.
        let fieldwise_init = decorators
            .iter()
            .any(|d| d.path.len() == 1 && d.path[0] == "fieldwise_init");

        self.expect(Token::Struct, "Expected 'struct'")?;
        let name = self.expect_identifier("Expected a struct name after 'struct'")?;
        let type_params = self.parse_type_params()?;
        let conforms = self.parse_conformance()?;
        self.expect(Token::Colon, "Expected ':' after the struct name")?;
        self.expect_stmt_end()?;

        // Body: an indented block of `var` fields, `comptime` associated facts,
        // and `def` methods.
        self.expect(Token::Indent, "Expected an indented struct body")?;
        let mut fields = Vec::new();
        let mut associated = Vec::new();
        let mut methods = Vec::new();
        while let Some(token) = self.peek_token()? {
            match token {
                Token::Dedent => {
                    self.next_token()?;
                    break;
                }
                Token::Newline => {
                    self.next_token()?;
                }
                Token::Var => {
                    self.expect(Token::Var, "Expected 'var'")?;
                    let fname = self.expect_identifier("Expected a field name")?;
                    self.expect(Token::Colon, "Fields require a type annotation")?;
                    let ty = self.parse_type()?;
                    self.expect_stmt_end()?;
                    fields.push(Param { name: fname, ty });
                }
                Token::Comptime => associated.push(self.parse_struct_comptime()?),
                Token::Def => methods.push(self.parse_method(Vec::new())?),
                // Decorators before a method (`@staticmethod`, …).
                Token::At => {
                    let decos = self.parse_decorators()?;
                    if !matches!(self.peek_token()?, Some(Token::Def)) {
                        return Err(ParseError::UnexpectedToken(
                            self.peek_token()?.cloned().unwrap_or(Token::Eof),
                            "a decorator in a struct body must precede a 'def' method".into(),
                        ));
                    }
                    methods.push(self.parse_method(decos)?);
                }
                other => {
                    return Err(ParseError::UnexpectedToken(
                        other.clone(),
                        "struct body may only contain 'var' fields, 'comptime' associated facts, and 'def' methods".into(),
                    ));
                }
            }
        }

        Ok(StmtKind::Struct {
            name,
            decorators,
            type_params,
            conforms,
            fields,
            associated,
            methods,
            fieldwise_init,
        })
    }

    /// `comptime NAME = expr` — an associated compile-time fact inside a struct.
    fn parse_struct_comptime(&mut self) -> Result<crate::ast::StructComptime, ParseError> {
        self.expect(Token::Comptime, "Expected 'comptime'")?;
        let name = self.expect_identifier("Expected a name after 'comptime'")?;
        self.expect(Token::Assign, "Expected '=' after the comptime member name")?;
        let value = self.parse_expression(Precedence::Lowest)?;
        self.expect_stmt_end()?;
        Ok(crate::ast::StructComptime { name, value })
    }

    /// Parses an optional trait-conformance list `'(' NAME (',' NAME)* ')'`
    /// following a `struct` name. Returns an empty list if the next token is not
    /// `(`. Used for `struct Duck(Copyable, Quackable):`.
    fn parse_conformance(&mut self) -> Result<Vec<String>, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::LParen)) {
            return Ok(Vec::new());
        }
        self.next_token()?; // consume '('
        let mut traits = Vec::new();
        loop {
            traits.push(self.expect_identifier("Expected a trait name in the conformance list")?);
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
            } else {
                break;
            }
        }
        self.expect(Token::RParen, "Expected ')' after the conformance list")?;
        Ok(traits)
    }

    /// `trait Name[(Super, …)]: <members>` — a trait, optionally **refining**
    /// super-traits (`trait Bird(Animal):`, reusing the conformance-list parser).
    /// The body holds `def` method requirements (`...`) or default methods (a real
    /// body), and `comptime NAME: Type` member requirements. (Generic traits
    /// `trait T[U]:` are not valid current Mojo, so no `[type_params]` is parsed.)
    fn parse_trait(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Trait, "Expected 'trait'")?;
        let name = self.expect_identifier("Expected a trait name after 'trait'")?;
        let refines = self.parse_conformance()?;
        self.expect(Token::Colon, "Expected ':' after the trait name")?;
        self.expect_stmt_end()?;

        self.expect(Token::Indent, "Expected an indented trait body")?;
        let mut methods = Vec::new();
        let mut comptime_members = Vec::new();
        while let Some(token) = self.peek_token()? {
            match token {
                Token::Dedent => {
                    self.next_token()?;
                    break;
                }
                Token::Newline => {
                    self.next_token()?;
                }
                Token::Def => methods.push(self.parse_trait_method()?),
                Token::Comptime => comptime_members.push(self.parse_trait_comptime()?),
                other => {
                    return Err(ParseError::UnexpectedToken(
                        other.clone(),
                        "a trait body may only contain 'def' methods or 'comptime' members".into(),
                    ));
                }
            }
        }
        Ok(StmtKind::Trait {
            name,
            refines,
            methods,
            comptime_members,
        })
    }

    /// `comptime NAME: Type` — a compile-time member requirement inside a trait.
    fn parse_trait_comptime(&mut self) -> Result<crate::ast::TraitComptime, ParseError> {
        self.expect(Token::Comptime, "Expected 'comptime'")?;
        let name = self.expect_identifier("Expected a name after 'comptime'")?;
        self.expect(Token::Colon, "Expected ':' after the comptime member name")?;
        let ty = self.parse_type()?;
        self.expect_stmt_end()?;
        Ok(crate::ast::TraitComptime { name, ty })
    }

    /// `def name([convention] self [, params]) -> ret:` followed by an indented
    /// body that is either `...` (a pure requirement) or real statements (a
    /// **default implementation**, stored in `default_body`).
    fn parse_trait_method(&mut self) -> Result<crate::ast::TraitMethod, ParseError> {
        self.expect(Token::Def, "Expected 'def'")?;
        let name = self.expect_identifier("Expected a method name after 'def'")?;

        self.expect(Token::LParen, "Expected '(' after the method name")?;
        let first = self.expect_identifier("A method's first parameter must be 'self'")?;
        let (self_name, self_convention) = if let Some(conv) = convention_word(&first) {
            if conv == ArgConvention::Ref {
                self.parse_optional_origin_specifier()?;
            }
            (
                self.expect_identifier("Expected 'self' after the receiver convention")?,
                Some(conv),
            )
        } else {
            (first, None)
        };
        if self_name != "self" {
            return Err(ParseError::UnexpectedToken(
                Token::Identifier(self_name),
                "a method's first parameter must be 'self'".into(),
            ));
        }
        let ParamList {
            params,
            positional_only,
            keyword_only,
        } = if matches!(self.peek_token()?, Some(Token::Comma)) {
            self.next_token()?; // consume ','
            self.parse_params()?
        } else {
            ParamList {
                params: Vec::new(),
                positional_only: None,
                keyword_only: None,
            }
        };
        self.expect(Token::RParen, "Expected ')' after the parameters")?;

        let ret = if matches!(self.peek_token()?, Some(Token::Arrow)) {
            self.next_token()?;
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(Token::Colon, "Expected ':' before the method body")?;
        // A body of exactly `...` is a pure requirement; anything else is a
        // default implementation (parsed, flagged unsupported by the checker).
        let default_body = self.parse_trait_method_body()?;

        Ok(crate::ast::TraitMethod {
            name,
            self_convention,
            params,
            positional_only,
            keyword_only,
            ret,
            default_body,
        })
    }

    /// `def name([convention] self [, params]) -> ret: <block>` inside a struct.
    fn parse_method(&mut self, decorators: Vec<Decorator>) -> Result<Method, ParseError> {
        self.expect(Token::Def, "Expected 'def'")?;
        let name = self.expect_identifier("Expected a method name after 'def'")?;

        self.expect(Token::LParen, "Expected '(' after the method name")?;
        // Detect the receiver. An instance method starts with `self`, optionally
        // carrying a convention (`mut self`, `out self`, `owned self`, `read self`
        // — convention words are contextual identifiers). A `@staticmethod` has no
        // `self`: its parameters (if any) start immediately. (A convention word as
        // the first token is read as `<conv> self`, so a static method whose first
        // parameter carries a convention is not distinguished — a rare case.)
        let first_is_self =
            matches!(self.peek_token()?, Some(Token::Identifier(id)) if id == "self");
        let first_is_convention = matches!(self.peek_token()?, Some(Token::Identifier(id)) if convention_word(id).is_some());
        let (has_self, self_convention) = if first_is_self {
            self.next_token()?; // consume 'self'
            (true, None)
        } else if first_is_convention {
            let conv = match self.peek_token()? {
                Some(Token::Identifier(id)) => convention_word(id),
                _ => None,
            };
            // `ref self` may carry an origin specifier: `ref[origin] self`.
            self.next_token()?; // consume the convention word
            if conv == Some(ArgConvention::Ref) {
                self.parse_optional_origin_specifier()?;
            }
            let self_name =
                self.expect_identifier("Expected 'self' after the receiver convention")?;
            if self_name != "self" {
                return Err(ParseError::UnexpectedToken(
                    Token::Identifier(self_name),
                    "a receiver convention must be followed by 'self'".into(),
                ));
            }
            (true, conv)
        } else {
            // No receiver — a static method.
            (false, None)
        };
        // Parameters: for an instance method they follow an optional comma after
        // `self`; for a static method they are the whole list.
        let ParamList {
            params,
            positional_only,
            keyword_only,
        } = if has_self {
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
                self.parse_params()?
            } else {
                ParamList {
                    params: Vec::new(),
                    positional_only: None,
                    keyword_only: None,
                }
            }
        } else {
            self.parse_params()?
        };
        self.expect(Token::RParen, "Expected ')' after the parameters")?;

        let raises = self.parse_raises_effect()?;
        let ret = if matches!(self.peek_token()?, Some(Token::Arrow)) {
            self.next_token()?;
            Some(self.parse_type()?)
        } else {
            None
        };

        self.expect(Token::Colon, "Expected ':' before the method body")?;
        let body = self.parse_suite()?;

        Ok(Method {
            name,
            has_self,
            self_convention,
            decorators,
            params,
            positional_only,
            keyword_only,
            raises,
            ret,
            body,
        })
    }

    /// Parses a `cond ':' NEWLINE block` clause shared by `if`/`elif`/`while`.
    /// The leading keyword has already been consumed.
    fn parse_condition_block(&mut self, ctx: &str) -> Result<(Expr, Vec<Stmt>), ParseError> {
        let cond = self.parse_expression(Precedence::Lowest)?;
        self.expect(Token::Colon, ctx)?;
        let body = self.parse_suite()?;
        Ok((cond, body))
    }

    /// `if cond: <block> (elif cond: <block>)* (else: <block>)?`
    fn parse_if(&mut self) -> Result<StmtKind, ParseError> {
        let (branches, orelse) = self.parse_if_rest()?;
        Ok(StmtKind::If { branches, orelse })
    }

    /// Parses an `if`/`elif`/`else` chain — the current token must be `if`. Shared
    /// by the runtime `if` and the compile-time `comptime if` (which differ only in
    /// the wrapping `Stmt` variant).
    fn parse_if_rest(&mut self) -> Result<IfChain, ParseError> {
        self.expect(Token::If, "Expected 'if'")?;
        let mut branches = vec![self.parse_condition_block("Expected ':' after the if condition")?];

        while matches!(self.peek_token()?, Some(Token::Elif)) {
            self.next_token()?; // consume 'elif'
            branches.push(self.parse_condition_block("Expected ':' after the elif condition")?);
        }

        let orelse = if matches!(self.peek_token()?, Some(Token::Else)) {
            self.next_token()?; // consume 'else'
            self.expect(Token::Colon, "Expected ':' after 'else'")?;
            Some(self.parse_suite()?)
        } else {
            None
        };

        Ok((branches, orelse))
    }

    /// `while cond: <block>`
    fn parse_while(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::While, "Expected 'while'")?;
        let (cond, body) = self.parse_condition_block("Expected ':' after the while condition")?;
        Ok(StmtKind::While { cond, body })
    }

    /// `for var in iter: <block>`
    fn parse_for(&mut self) -> Result<StmtKind, ParseError> {
        let (var, iter, body) = self.parse_for_rest()?;
        Ok(StmtKind::For { var, iter, body })
    }

    /// Parses a `for var in iter: <block>` — the current token must be `for`.
    /// Shared by the runtime `for` and the compile-time `comptime for`.
    fn parse_for_rest(&mut self) -> Result<(String, Expr, Vec<Stmt>), ParseError> {
        self.expect(Token::For, "Expected 'for'")?;
        let var = self.expect_identifier("Expected a loop variable name after 'for'")?;
        self.expect(Token::In, "Expected 'in' after the loop variable")?;
        let iter = self.parse_expression(Precedence::Lowest)?;
        self.expect(Token::Colon, "Expected ':' after the for-loop iterable")?;
        let body = self.parse_suite()?;
        Ok((var, iter, body))
    }

    /// `return` or `return expr`
    fn parse_return(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Return, "Expected 'return'")?;
        let value = match self.peek_token()? {
            Some(Token::Newline) | Some(Token::Eof) | None => None,
            _ => Some(self.parse_expression(Precedence::Lowest)?),
        };
        self.expect_stmt_end()?;
        Ok(StmtKind::Return(value))
    }

    /// Parses an indented block: `INDENT statement+ DEDENT`.
    fn parse_block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect(Token::Indent, "Expected an indented block")?;
        self.parse_block_body()
    }

    /// Parses a Mojo/Python suite after `:`: either a newline followed by an
    /// indented block, or one simple statement on the same physical line.
    fn parse_suite(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if matches!(self.peek_token()?, Some(Token::Newline)) {
            self.next_token()?; // consume the newline after ':'
            self.parse_block()
        } else {
            let mut body = Vec::new();
            loop {
                body.push(self.parse_statement()?);
                if !self.last_stmt_ended_with_semicolon {
                    break;
                }
                if matches!(self.peek_token()?, Some(Token::Newline)) {
                    self.next_token()?; // consume the logical line after trailing ';'
                    self.last_stmt_ended_with_semicolon = false;
                    break;
                }
                if matches!(self.peek_token()?, Some(Token::Eof) | None) {
                    self.last_stmt_ended_with_semicolon = false;
                    break;
                }
            }
            Ok(body)
        }
    }

    /// Parses a trait method body, preserving `...` as a pure requirement.
    fn parse_trait_method_body(&mut self) -> Result<Option<Vec<Stmt>>, ParseError> {
        if matches!(self.peek_token()?, Some(Token::Ellipsis)) {
            self.next_token()?; // consume same-line '...'
            self.expect_stmt_end()?;
            return Ok(None);
        }

        if matches!(self.peek_token()?, Some(Token::Newline)) {
            self.next_token()?; // consume the newline after ':'
            self.expect(Token::Indent, "Expected an indented trait-method body")?;
            if matches!(self.peek_token()?, Some(Token::Ellipsis)) {
                self.next_token()?; // consume indented '...'
                self.expect_stmt_end()?;
                self.expect(
                    Token::Dedent,
                    "Expected the trait-method body to end after '...'",
                )?;
                Ok(None)
            } else {
                Ok(Some(self.parse_block_body()?))
            }
        } else {
            Ok(Some(vec![self.parse_statement()?]))
        }
    }

    /// The statements of a block up to (and consuming) the closing `DEDENT`; the
    /// opening `INDENT` must already have been consumed. Split out so a trait
    /// method's default body can be parsed after peeking past the `INDENT` for `...`.
    fn parse_block_body(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut body = Vec::new();
        while let Some(token) = self.peek_token()? {
            match token {
                Token::Dedent => {
                    self.next_token()?; // consume the dedent to end the block
                    break;
                }
                Token::Newline => {
                    self.next_token()?; // skip blank lines inside the block
                }
                _ => body.push(self.parse_statement()?),
            }
        }
        Ok(body)
    }

    /// Parses a type annotation: a scalar keyword, `Self.T`, associated lookups
    /// like `C.Element`, or a named type optionally applied to type arguments
    /// (`Pair[Int]`).
    fn parse_type(&mut self) -> Result<Type, ParseError> {
        let ty = match self.next_token()? {
            // A function type: `def(types) [effects] -> ret`.
            Token::Def => self.parse_function_type_tail(),
            Token::None => Ok(Type::None),
            Token::Identifier(id) => match id.as_str() {
                "Int" => Ok(Type::Int),
                "UInt" => Ok(Type::UInt),
                "Bool" => Ok(Type::Bool),
                "String" => Ok(Type::String),
                "Float64" => Ok(Type::Float64),
                // `Self.T` references one of the enclosing struct's type
                // parameters; bare `Self` is the enclosing struct/trait type.
                "Self" if matches!(self.peek_token()?, Some(Token::Dot)) => {
                    self.next_token()?; // consume '.'
                    let param =
                        self.expect_identifier("Expected a type parameter name after 'Self.'")?;
                    Ok(Type::SelfParam(param))
                }
                "Self" => Ok(Type::SelfType),
                // `ref [origin] T` — a reference type (parametric mutability). The
                // origin specifier is parsed and discarded; the referent follows.
                // (`ref` is contextual — a following `[` or type token, not `.`/end.)
                "ref"
                    if matches!(
                        self.peek_token()?,
                        Some(Token::LBracket | Token::Identifier(_) | Token::Def | Token::None)
                    ) =>
                {
                    self.parse_optional_origin_specifier()?;
                    Ok(Type::Ref(Box::new(self.parse_type()?)))
                }
                // Any other identifier names a struct type or an in-scope type
                // parameter (the checker decides), optionally with parameter args.
                _ => {
                    let args = if matches!(self.peek_token()?, Some(Token::LBracket)) {
                        self.parse_param_args()?
                    } else {
                        Vec::new()
                    };
                    Ok(Type::Named(id, args))
                }
            },
            token => Err(ParseError::UnexpectedToken(
                token,
                "Expected a type name".into(),
            )),
        }?;

        self.parse_type_assoc_tail(ty)
    }

    /// Parse zero or more `.Member` suffixes after a type atom.
    fn parse_type_assoc_tail(&mut self, mut ty: Type) -> Result<Type, ParseError> {
        while matches!(self.peek_token()?, Some(Token::Dot)) {
            self.next_token()?; // consume '.'
            let name = self.expect_identifier("Expected an associated type name after '.'")?;
            ty = Type::Assoc {
                base: Box::new(ty),
                name,
            };
        }
        Ok(ty)
    }

    /// Parses a function type after its leading `def` has been consumed:
    /// `'(' [type (',' type)*] ')' effects '->' type`. Effects between `)` and
    /// `->` are `thin`, `raises`, and `abi(...)` (the last parsed and discarded).
    fn parse_function_type_tail(&mut self) -> Result<Type, ParseError> {
        self.expect(Token::LParen, "Expected '(' in a function type")?;
        let mut params = Vec::new();
        if !matches!(self.peek_token()?, Some(Token::RParen)) {
            loop {
                params.push(self.parse_type()?);
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?; // consume ','
                } else {
                    break;
                }
            }
        }
        self.expect(Token::RParen, "Expected ')' after function-type parameters")?;

        // Effects: `thin` / `raises` / `abi("…")` in any order, until `->`.
        let mut thin = false;
        let mut raises = false;
        loop {
            match self.peek_token()? {
                Some(Token::Identifier(id)) if id == "thin" => {
                    self.next_token()?;
                    thin = true;
                }
                Some(Token::Raises) => {
                    self.next_token()?;
                    raises = true;
                }
                Some(Token::Identifier(id)) if id == "abi" => {
                    self.next_token()?; // consume 'abi'
                    self.expect(Token::LParen, "Expected '(' after 'abi'")?;
                    // Discard the abi specifier's contents.
                    while !matches!(self.peek_token()?, Some(Token::RParen) | None) {
                        self.next_token()?;
                    }
                    self.expect(Token::RParen, "Expected ')' to close 'abi(...)'")?;
                }
                _ => break,
            }
        }

        self.expect(Token::Arrow, "Expected '->' in a function type")?;
        let ret = self.parse_type()?;
        Ok(Type::Func {
            params,
            ret: Box::new(ret),
            thin,
            raises,
        })
    }

    /// Parses a parameter-argument list `'[' param_arg (',' param_arg)* ']'`. The
    /// next token must be `[`. Used for `Pair[Int]` / `FixedBuffer[8]`.
    fn parse_param_args(&mut self) -> Result<Vec<crate::ast::ParamArg>, ParseError> {
        self.expect(Token::LBracket, "Expected '[' to begin parameter arguments")?;
        let mut args = Vec::new();
        loop {
            args.push(self.parse_param_arg()?);
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
            } else {
                break;
            }
        }
        self.expect(Token::RBracket, "Expected ']' after parameter arguments")?;
        Ok(args)
    }

    /// Parses a single parameter argument: a `Type` (for a type parameter) or a
    /// comptime value `Expr` (for a value parameter). A leading type keyword,
    /// `None`, or `Self` is unambiguously a type. A bare identifier followed by
    /// `[` is a parameterized type (`Foo[Int]`); otherwise an identifier starts a
    /// value expression (a lone identifier is left for the checker to reinterpret
    /// as a type when the parameter is a type one). Anything else is a value.
    fn parse_param_arg(&mut self) -> Result<crate::ast::ParamArg, ParseError> {
        use crate::ast::ParamArg;
        if self.peek_starts_type()? {
            return Ok(ParamArg::Type(self.parse_type()?));
        }
        if let Some(Token::Identifier(_)) = self.peek_token()? {
            let id = self.expect_identifier("unreachable: peeked identifier")?;
            let id_span = self.last_span;
            if matches!(self.peek_token()?, Some(Token::LBracket)) {
                let args = self.parse_param_args()?;
                return Ok(ParamArg::Type(Type::Named(id, args)));
            }
            // A value expression whose first atom is this identifier.
            let atom = Expr::new(ExprKind::Identifier(id), id_span);
            let expr = self.parse_expression_from(atom, Precedence::Lowest)?;
            return Ok(ParamArg::Value(expr));
        }
        Ok(ParamArg::Value(self.parse_expression(Precedence::Lowest)?))
    }

    /// Whether the next token unambiguously begins a *type* (a scalar keyword,
    /// `None`, or `Self`) — used to classify a parameter argument.
    fn peek_starts_type(&mut self) -> Result<bool, ParseError> {
        Ok(match self.peek_token()? {
            Some(Token::None) => true,
            Some(Token::Identifier(id)) => {
                matches!(
                    id.as_str(),
                    "Int" | "UInt" | "Bool" | "String" | "Float64" | "Self"
                )
            }
            _ => false,
        })
    }

    /// Parses an optional type-parameter list `'[' type_param (',' type_param)* ']'`
    /// following a `struct`/`def` name. Returns an empty list if the next token is
    /// not `[`. Each parameter must carry a `: bound` (one or more trait names
    /// joined by `&`) — Mojo has no unconstrained type parameters.
    fn parse_type_params(&mut self) -> Result<Vec<crate::ast::TypeParam>, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::LBracket)) {
            return Ok(Vec::new());
        }
        self.next_token()?; // consume '['
        let mut params = Vec::new();
        loop {
            let name = self.expect_identifier("Expected a type-parameter name")?;
            self.expect(
                Token::Colon,
                "A type parameter requires a ': bound' (e.g. 'T: Copyable')",
            )?;
            let mut bounds =
                vec![self.expect_identifier("Expected a trait name in the type-parameter bound")?];
            while matches!(self.peek_token()?, Some(Token::Amp)) {
                self.next_token()?; // consume '&'
                bounds.push(self.expect_identifier("Expected a trait name after '&'")?);
            }
            params.push(crate::ast::TypeParam { name, bounds });
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
            } else {
                break;
            }
        }
        self.expect(Token::RBracket, "Expected ']' after type parameters")?;
        Ok(params)
    }

    // --- Expressions (Pratt parser) ---

    /// Parses an expression whose operators all bind more tightly than
    /// `min_precedence` (precedence climbing).
    fn parse_expression(&mut self, min_precedence: Precedence) -> Result<Expr, ParseError> {
        let left = self.parse_prefix()?;
        self.parse_expression_from(left, min_precedence)
    }

    /// Continues precedence climbing from an already-parsed `left` operand. Used
    /// when a leading atom has been consumed elsewhere (parameter-argument
    /// disambiguation).
    fn parse_expression_from(
        &mut self,
        mut left: Expr,
        min_precedence: Precedence,
    ) -> Result<Expr, ParseError> {
        while min_precedence < self.peek_precedence()? {
            left = self.parse_infix(left)?;
        }
        Ok(left)
    }

    /// Builds an `ExprKind::TString` from a lexed t-string, parsing each
    /// interpolation chunk's raw source into a real sub-expression. `start` is the
    /// t-string token's start offset (its end is the last-consumed token).
    fn build_tstring(
        &self,
        chunks: Vec<TStringChunk>,
        raw: bool,
        start: usize,
    ) -> Result<Expr, ParseError> {
        let mut parts = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            match chunk {
                TStringChunk::Text(text) => parts.push(TStringPart::Literal(text)),
                TStringChunk::Interp(src) => {
                    parts.push(TStringPart::Expr(Box::new(parse_interpolation(&src)?)));
                }
            }
        }
        Ok(self.node(ExprKind::TString { parts, raw }, start))
    }

    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek_start();
        let token = self.next_token()?;
        match token {
            Token::IntLiteral(val) => Ok(self.node(ExprKind::Int(val), start)),
            Token::FloatLiteral(val) => Ok(self.node(ExprKind::Float(val), start)),
            Token::BoolLiteral(val) => Ok(self.node(ExprKind::Bool(val), start)),
            Token::StringLiteral(val) => Ok(self.node(ExprKind::Str(val), start)),
            Token::TString { chunks, raw } => self.build_tstring(chunks, raw, start),
            Token::None => Ok(self.node(ExprKind::None, start)),
            Token::Identifier(id) => Ok(self.node(ExprKind::Identifier(id), start)),
            Token::Minus => {
                let operand = self.parse_expression(Precedence::Unary)?;
                Ok(self.node(ExprKind::Prefix(PrefixOp::Neg, Box::new(operand)), start))
            }
            Token::Not => {
                let operand = self.parse_expression(Precedence::Not)?;
                Ok(self.node(ExprKind::Prefix(PrefixOp::Not, Box::new(operand)), start))
            }
            Token::LParen => {
                // `()` — the empty tuple.
                if matches!(self.peek_token()?, Some(Token::RParen)) {
                    self.next_token()?; // consume ')'
                    return Ok(self.node(ExprKind::TupleLit(Vec::new()), start));
                }
                let first = self.parse_expression(Precedence::Lowest)?;
                // A comma makes it a tuple: `(a,)`, `(a, b)`, `(a, b,)`. Without a
                // comma it is plain grouping `(e)`.
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    let mut elems = vec![first];
                    while matches!(self.peek_token()?, Some(Token::Comma)) {
                        self.next_token()?; // consume ','
                        if matches!(self.peek_token()?, Some(Token::RParen)) {
                            break; // trailing comma
                        }
                        elems.push(self.parse_expression(Precedence::Lowest)?);
                    }
                    self.expect(Token::RParen, "Expected ')' after tuple elements")?;
                    Ok(self.node(ExprKind::TupleLit(elems), start))
                } else {
                    self.expect(Token::RParen, "Expected closing ')' after expression")?;
                    Ok(first)
                }
            }
            // A list literal `[a, b, …]`. Empty `[]` can't infer an element type,
            // so it is rejected — use `List[T]()`.
            Token::LBracket => {
                if matches!(self.peek_token()?, Some(Token::RBracket)) {
                    self.next_token()?; // consume ']'
                    return Err(ParseError::UnexpectedToken(
                        Token::RBracket,
                        "an empty list literal '[]' has no element type; use List[T]()".into(),
                    ));
                }
                let elems = self.parse_args()?;
                self.expect(Token::RBracket, "Expected ']' after list elements")?;
                Ok(self.node(ExprKind::ListLit(elems), start))
            }
            token => Err(ParseError::UnexpectedToken(
                token,
                "Expected an expression".into(),
            )),
        }
    }

    /// Parses an infix/postfix continuation of `left`: either a binary operator
    /// or a call `(...)`. Only invoked when the next token is such an operator.
    fn parse_infix(&mut self, left: Expr) -> Result<Expr, ParseError> {
        // Every node built here spans from `left`'s start to the last token consumed.
        let start = left.span.0;
        // Postfix transfer sigil `expr '^'`.
        if matches!(self.peek_token()?, Some(Token::Caret)) {
            self.next_token()?; // consume '^'
            return Ok(self.node(ExprKind::Transfer(Box::new(left)), start));
        }
        // Postfix member access `expr '.' NAME` or method call `expr '.' NAME (args)`.
        if matches!(self.peek_token()?, Some(Token::Dot)) {
            self.next_token()?; // consume '.'
            let field = self.expect_identifier("Expected a field or method name after '.'")?;
            if matches!(self.peek_token()?, Some(Token::LParen)) {
                self.next_token()?; // consume '('
                let (args, kwargs) = self.parse_call_args()?;
                self.expect(Token::RParen, "Expected ')' after arguments")?;
                return Ok(self.node(
                    ExprKind::MethodCall {
                        object: Box::new(left),
                        method: field,
                        args,
                        kwargs,
                    },
                    start,
                ));
            }
            return Ok(self.node(
                ExprKind::Member {
                    object: Box::new(left),
                    field,
                },
                start,
            ));
        }

        // Postfix `[`: a **slice** (`obj[lower:upper:step]`, any bound optional),
        // a call's explicit compile-time parameters (`NAME '[' args ']' '(' … ')'`),
        // or a plain subscript (`obj '[' index ']'`). A top-level `:` inside the
        // brackets marks a slice; otherwise `(` following decides call vs subscript.
        if matches!(self.peek_token()?, Some(Token::LBracket)) {
            // A leading `:` is a slice with no lower bound (`obj[:j]`, `obj[::2]`).
            self.next_token()?; // consume '['
            if matches!(self.peek_token()?, Some(Token::Colon)) {
                return self.parse_slice_rest(left, None);
            }
            let first = self.parse_param_arg()?;
            // A `:` after the first entry makes this a slice; the entry is its lower
            // bound and must be a value expression.
            if matches!(self.peek_token()?, Some(Token::Colon)) {
                let lower = match first {
                    crate::ast::ParamArg::Value(expr) => Some(expr),
                    crate::ast::ParamArg::Type(_) => {
                        return Err(ParseError::UnexpectedToken(
                            Token::Colon,
                            "a slice bound must be an expression".into(),
                        ));
                    }
                };
                return self.parse_slice_rest(left, lower);
            }
            // Otherwise collect any remaining comma-separated entries and finish as
            // a generic call (if `(` follows) or a single-index subscript.
            let mut param_args = vec![first];
            while matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
                param_args.push(self.parse_param_arg()?);
            }
            self.expect(Token::RBracket, "Expected ']' after a subscript")?;
            if matches!(self.peek_token()?, Some(Token::LParen)) {
                let name = call_name(left)?;
                self.next_token()?; // consume '('
                let (args, kwargs) = self.parse_call_args()?;
                self.expect(Token::RParen, "Expected ')' after arguments")?;
                return Ok(self.node(
                    ExprKind::Call {
                        name,
                        param_args,
                        args,
                        kwargs,
                    },
                    start,
                ));
            }
            // A single value entry is an ordinary subscript `obj[i]`; anything with a
            // type argument (`UnsafePointer[Int]`) is a parameterized-type reference
            // (`TypeApply`) — valid as a static-method receiver, e.g. `.alloc(n)`.
            match <[_; 1]>::try_from(param_args) {
                Ok([crate::ast::ParamArg::Value(expr)]) => {
                    return Ok(self.node(
                        ExprKind::Index {
                            object: Box::new(left),
                            index: Box::new(expr),
                        },
                        start,
                    ));
                }
                Ok([other]) => {
                    return Ok(self.node(
                        ExprKind::TypeApply {
                            name: call_name(left)?,
                            args: vec![other],
                        },
                        start,
                    ));
                }
                Err(param_args) => {
                    return Ok(self.node(
                        ExprKind::TypeApply {
                            name: call_name(left)?,
                            args: param_args,
                        },
                        start,
                    ));
                }
            }
        }

        // Postfix call without explicit parameters: `IDENT '(' args ')'`.
        if matches!(self.peek_token()?, Some(Token::LParen)) {
            let name = call_name(left)?;
            self.next_token()?; // consume '('
            let (args, kwargs) = self.parse_call_args()?;
            self.expect(Token::RParen, "Expected ')' after arguments")?;
            return Ok(self.node(
                ExprKind::Call {
                    name,
                    param_args: Vec::new(),
                    args,
                    kwargs,
                },
                start,
            ));
        }

        // Walrus / named expression: `name := value`. The target must be a bare
        // name. (Parsed for completeness; the evaluator flags it as unsupported.)
        if matches!(self.peek_token()?, Some(Token::ColonEq)) {
            self.next_token()?; // consume ':='
            let ExprKind::Identifier(name) = left.kind else {
                return Err(ParseError::UnexpectedToken(
                    Token::ColonEq,
                    format!("the walrus ':=' target must be a name, got {:?}", left.kind),
                ));
            };
            let value = self.parse_expression(Precedence::Lowest)?;
            return Ok(self.node(
                ExprKind::Named {
                    name,
                    value: Box::new(value),
                },
                start,
            ));
        }

        // Conditional expression (ternary): `then_branch if cond else else_branch`.
        // The condition is parsed at `Conditional` (an or-test — it won't grab the
        // `else`); the else branch is a full expression (so ternaries nest right).
        if matches!(self.peek_token()?, Some(Token::If)) {
            self.next_token()?; // consume 'if'
            let cond = self.parse_expression(Precedence::Conditional)?;
            self.expect(Token::Else, "Expected 'else' in a conditional expression")?;
            let else_branch = self.parse_expression(Precedence::Lowest)?;
            return Ok(self.node(
                ExprKind::IfExpr {
                    cond: Box::new(cond),
                    then_branch: Box::new(left),
                    else_branch: Box::new(else_branch),
                },
                start,
            ));
        }

        // Comparison, possibly chained: `a < b`, `a in b`, `a not in b`, and chains
        // like `a < b <= c` or `0 <= i < n`. Each operand is parsed up to the next
        // comparison operator. A single comparison stays an `Infix` (so existing
        // behavior is unchanged); a chain of length ≥ 2 becomes an `Expr::Compare`.
        // In infix position, `not` can only begin `not in`.
        if self.peek_is_comparison()? {
            let mut rest: Vec<(InfixOp, Expr)> = Vec::new();
            loop {
                let op = self.parse_comparison_op()?;
                let right = self.parse_expression(Precedence::Comparison)?;
                rest.push((op, right));
                if !self.peek_is_comparison()? {
                    break;
                }
            }
            if rest.len() == 1 {
                let (op, right) = rest.into_iter().next().unwrap();
                return Ok(self.node(ExprKind::Infix(op, Box::new(left), Box::new(right)), start));
            }
            return Ok(self.node(
                ExprKind::Compare {
                    first: Box::new(left),
                    rest,
                },
                start,
            ));
        }

        let op_token = self.next_token()?;
        let op = match op_token {
            Token::Plus => InfixOp::Add,
            Token::Minus => InfixOp::Sub,
            Token::Star => InfixOp::Mul,
            Token::Slash => InfixOp::Div,
            Token::DoubleSlash => InfixOp::FloorDiv,
            Token::Percent => InfixOp::Mod,
            Token::DoubleStar => InfixOp::Pow,
            // Comparisons (`== != < > <= >=`, `in`, `not in`) are handled by the
            // chained-comparison path above, never here.
            Token::And => InfixOp::And,
            Token::Or => InfixOp::Or,
            token => {
                return Err(ParseError::UnexpectedToken(
                    token,
                    "Expected a binary operator".into(),
                ));
            }
        };

        // Left-associative: parse the right operand at the operator's own
        // precedence so equal-precedence operators don't get reabsorbed.
        let right = self.parse_expression(infix_precedence(op))?;
        Ok(self.node(ExprKind::Infix(op, Box::new(left), Box::new(right)), start))
    }

    /// Finishes a slice subscript once a `:` has been seen (the parser is
    /// positioned at that first `:`), with `lower` already parsed. Grammar:
    /// `':' [upper] [':' [step]] ']'`.
    fn parse_slice_rest(&mut self, object: Expr, lower: Option<Expr>) -> Result<Expr, ParseError> {
        let start = object.span.0;
        self.expect(Token::Colon, "Expected ':' in a slice")?;
        // Optional upper bound — absent if the next token is `:` or `]`.
        let upper = if matches!(self.peek_token()?, Some(Token::Colon | Token::RBracket)) {
            None
        } else {
            Some(self.parse_expression(Precedence::Lowest)?)
        };
        // Optional step after a second `:`.
        let step = if matches!(self.peek_token()?, Some(Token::Colon)) {
            self.next_token()?; // consume the second ':'
            if matches!(self.peek_token()?, Some(Token::RBracket)) {
                None
            } else {
                Some(self.parse_expression(Precedence::Lowest)?)
            }
        } else {
            None
        };
        self.expect(Token::RBracket, "Expected ']' after a slice")?;
        Ok(self.node(
            ExprKind::Slice {
                object: Box::new(object),
                lower: lower.map(Box::new),
                upper: upper.map(Box::new),
                step: step.map(Box::new),
            },
            start,
        ))
    }

    /// Whether the next token begins a comparison operator (`== != < > <= >=`,
    /// `in`, or `not` — which in infix position can only start `not in`).
    fn peek_is_comparison(&mut self) -> Result<bool, ParseError> {
        Ok(matches!(
            self.peek_token()?,
            Some(
                Token::EqEq
                    | Token::NotEq
                    | Token::Lt
                    | Token::Gt
                    | Token::Le
                    | Token::Ge
                    | Token::In
                    | Token::Not
            )
        ))
    }

    /// Consume one comparison operator, resolving `not in` (two words).
    fn parse_comparison_op(&mut self) -> Result<InfixOp, ParseError> {
        let op = match self.next_token()? {
            Token::EqEq => InfixOp::Eq,
            Token::NotEq => InfixOp::Ne,
            Token::Lt => InfixOp::Lt,
            Token::Gt => InfixOp::Gt,
            Token::Le => InfixOp::Le,
            Token::Ge => InfixOp::Ge,
            Token::In => InfixOp::In,
            Token::Not => {
                self.expect(Token::In, "Expected 'in' after 'not' in a membership test")?;
                InfixOp::NotIn
            }
            other => {
                return Err(ParseError::UnexpectedToken(
                    other,
                    "Expected a comparison operator".into(),
                ));
            }
        };
        Ok(op)
    }

    /// Precedence of whatever operator is next, or `Lowest` if the next token
    /// does not continue an expression (so the climbing loop stops).
    fn peek_precedence(&mut self) -> Result<Precedence, ParseError> {
        let prec = match self.peek_token()? {
            Some(Token::ColonEq) => Precedence::Walrus,
            // `if` in infix position (after an operand) begins a ternary.
            Some(Token::If) => Precedence::Conditional,
            Some(Token::Or) => Precedence::Or,
            Some(Token::And) => Precedence::And,
            Some(Token::EqEq | Token::NotEq | Token::Lt | Token::Gt | Token::Le | Token::Ge) => {
                Precedence::Comparison
            }
            // Membership `in` / `not in` share comparison precedence. In infix
            // position (after an operand) `not` can only start `not in`.
            Some(Token::In | Token::Not) => Precedence::Comparison,
            Some(Token::Plus | Token::Minus) => Precedence::Sum,
            Some(Token::Star | Token::Slash | Token::DoubleSlash | Token::Percent) => {
                Precedence::Product
            }
            Some(Token::DoubleStar) => Precedence::Power,
            // `[` begins an explicit compile-time parameter list on a call; `^`
            // is the postfix transfer sigil.
            Some(Token::LParen | Token::LBracket | Token::Dot | Token::Caret) => Precedence::Call,
            _ => Precedence::Lowest,
        };
        Ok(prec)
    }

    /// Parses a (possibly empty) comma-separated argument list. The opening `(`
    /// has been consumed; stops at the closing `)` without consuming it.
    fn parse_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        if matches!(self.peek_token()?, Some(Token::RParen)) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expression(Precedence::Lowest)?);
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
            } else {
                break;
            }
        }
        Ok(args)
    }

    /// Parses call arguments: positional expressions and keyword arguments. Both
    /// the older Python-like `name=value` spelling and Mojo's `name: value`
    /// spelling are accepted and represented as [`KwArg`]. A positional argument
    /// may not follow a keyword one.
    fn parse_call_args(&mut self) -> Result<(Vec<Expr>, Vec<KwArg>), ParseError> {
        let mut args = Vec::new();
        let mut kwargs = Vec::new();
        if matches!(self.peek_token()?, Some(Token::RParen)) {
            return Ok((args, kwargs));
        }
        loop {
            let expr = self.parse_expression(Precedence::Lowest)?;
            if matches!(expr.kind, ExprKind::Identifier(_))
                && matches!(self.peek_token()?, Some(Token::Assign) | Some(Token::Colon))
            {
                let ExprKind::Identifier(name) = expr.kind else {
                    unreachable!()
                };
                self.next_token()?; // consume '=' or ':'
                let value = self.parse_expression(Precedence::Lowest)?;
                kwargs.push(KwArg { name, value });
            } else {
                if !kwargs.is_empty() {
                    return Err(ParseError::UnexpectedToken(
                        Token::Comma,
                        "a positional argument cannot follow a keyword argument".into(),
                    ));
                }
                args.push(expr);
            }
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
                if matches!(self.peek_token()?, Some(Token::RParen)) {
                    break; // trailing comma
                }
            } else {
                break;
            }
        }
        Ok((args, kwargs))
    }

    /// Ensures a statement is cleanly terminated by a Newline or EOF
    fn expect_stmt_end(&mut self) -> Result<(), ParseError> {
        let token = self.next_token()?;
        match token {
            Token::Semicolon => {
                self.last_stmt_ended_with_semicolon = true;
                Ok(())
            }
            Token::Newline | Token::Eof => {
                self.last_stmt_ended_with_semicolon = false;
                Ok(())
            }
            _ => Err(ParseError::UnexpectedToken(
                token,
                "Expected newline, ';', or EOF at the end of statement".into(),
            )),
        }
    }
}

/// Parses a t-string interpolation's raw source as a single expression, on a
/// fresh sub-lexer/parser. The whole fragment must be one expression (trailing
/// tokens are an error).
fn parse_interpolation(src: &str) -> Result<Expr, ParseError> {
    let mut sub = Parser::new(Lexer::new(src));
    let expr = sub.parse_expression(Precedence::Lowest)?;
    sub.expect_stmt_end()?; // reject leftover tokens (e.g. `{a b}`)
    Ok(expr)
}

/// Maps a contextual convention word (`read`/`mut`/`owned`/`out`/`ref`) to its
/// `ArgConvention`, or `None` for any other identifier.
fn convention_word(word: &str) -> Option<ArgConvention> {
    match word {
        "read" => Some(ArgConvention::Read),
        "mut" => Some(ArgConvention::Mut),
        "owned" => Some(ArgConvention::Owned),
        "out" => Some(ArgConvention::Out),
        "ref" => Some(ArgConvention::Ref),
        "deinit" => Some(ArgConvention::Deinit),
        _ => None,
    }
}

/// The callee name of a call: the callee must be a bare identifier (closures
/// can't escape to become arbitrary callee expressions).
fn call_name(callee: Expr) -> Result<String, ParseError> {
    match callee.kind {
        ExprKind::Identifier(name) => Ok(name),
        other => Err(ParseError::UnexpectedToken(
            Token::LParen,
            format!("only named functions can be called, found {:?}", other),
        )),
    }
}

/// The infix operator an augmented-assignment token applies (`+=` → `Add`, …),
/// or `None` if the token is not an augmented-assignment operator.
fn aug_assign_op(tok: &Token) -> Option<InfixOp> {
    Some(match tok {
        Token::PlusEq => InfixOp::Add,
        Token::MinusEq => InfixOp::Sub,
        Token::StarEq => InfixOp::Mul,
        Token::SlashEq => InfixOp::Div,
        Token::DoubleSlashEq => InfixOp::FloorDiv,
        Token::PercentEq => InfixOp::Mod,
        Token::DoubleStarEq => InfixOp::Pow,
        _ => return None,
    })
}

/// The precedence an infix operator parses its right operand at (left-assoc).
fn infix_precedence(op: InfixOp) -> Precedence {
    match op {
        InfixOp::Or => Precedence::Or,
        InfixOp::And => Precedence::And,
        InfixOp::Eq
        | InfixOp::Ne
        | InfixOp::Lt
        | InfixOp::Gt
        | InfixOp::Le
        | InfixOp::Ge
        | InfixOp::In
        | InfixOp::NotIn => Precedence::Comparison,
        InfixOp::Add | InfixOp::Sub => Precedence::Sum,
        InfixOp::Mul | InfixOp::Div | InfixOp::FloorDiv | InfixOp::Mod => Precedence::Product,
        // Right-associative: parse the right operand one level below `**` so that
        // a following `**` (Power > Unary) is re-absorbed (`a ** b ** c` = `a ** (b ** c)`).
        InfixOp::Pow => Precedence::Unary,
    }
}
