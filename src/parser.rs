//! Hand-written parser for Mojito's indentation-sensitive Mojo subset.
//!
//! Statement, declaration, type, and suite parsing use recursive descent;
//! expressions use precedence climbing with postfix call/member/index tails.
//! [`parse`](crate::parse) is fail-fast, while the diagnostic entry point
//! recovers at statement boundaries so it can report multiple syntax errors.

use std::iter::Peekable;

use crate::ast::{
    ArgConvention, Capture, CaptureKind, CaptureList, Decorator, Expr, ExprKind, FnParam, InfixOp,
    KwArg, Method, Param, ParamKind, PrefixOp, Stmt, StmtKind, SubscriptArg, TStringPart, Type,
    WithItem,
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
type StructConformanceList = (Vec<String>, Vec<(String, Expr)>, Option<Type>);
type ParsedSliceTail = (Option<Box<Expr>>, Option<Box<Expr>>, bool);

enum ParsedBracketItem {
    Param(crate::ast::ParamArg),
    Slice {
        lower: Option<Box<Expr>>,
        upper: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
        explicit_step: bool,
    },
}

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

/// A syntax-only recovery result. `program` is deliberately partial and must not
/// be sent to semantic phases when `errors` is non-empty.
#[derive(Debug)]
pub struct ParseReport {
    pub program: Vec<Stmt>,
    pub errors: Vec<ParseError>,
    pub truncated: bool,
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
            source: None,
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

    /// Parse for diagnostics, recovering at layout-level statement boundaries.
    /// Normal compilation continues to use `parse_program` and remains fail-fast.
    pub fn parse_program_diagnostic(&mut self, max_errors: usize) -> ParseReport {
        let mut program = Vec::new();
        let mut errors = Vec::new();
        let mut truncated = false;

        loop {
            let token = match self.peek_token() {
                Ok(Some(token)) => token.clone(),
                Ok(None) => break,
                Err(err) => {
                    errors.push(err.at(self.last_span));
                    self.discard_one();
                    if errors.len() >= max_errors {
                        truncated = true;
                        break;
                    }
                    continue;
                }
            };
            match token {
                Token::Eof => break,
                Token::Newline | Token::Indent | Token::Dedent => self.discard_one(),
                _ => match self.parse_statement() {
                    Ok(stmt) => program.push(stmt),
                    Err(err) => {
                        errors.push(err);
                        if errors.len() >= max_errors {
                            truncated = true;
                            break;
                        }
                        self.synchronize_statement();
                    }
                },
            }
        }
        ParseReport {
            program,
            errors,
            truncated,
        }
    }

    /// Consume one raw lexer item even when it is an error, guaranteeing forward
    /// progress during recovery.
    fn discard_one(&mut self) {
        if let Some(Ok((token, span))) = self.tokens.next() {
            self.last_span = span;
            if !matches!(
                token,
                Token::Newline | Token::Indent | Token::Dedent | Token::Eof
            ) {
                self.last_significant_end = span.1;
            }
        }
    }

    /// Panic-mode synchronization. Newlines and layout boundaries are reliable
    /// because the lexer suppresses newlines while delimiters remain open.
    fn synchronize_statement(&mut self) {
        loop {
            match self.tokens.next() {
                Some(Ok((token, span))) => {
                    self.last_span = span;
                    if matches!(
                        token,
                        Token::Newline | Token::Semicolon | Token::Dedent | Token::Eof
                    ) {
                        break;
                    }
                }
                Some(Err(_)) => continue,
                None => break,
            }
        }
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
                Some(Token::Identifier(word)) if word == "ref" => self.parse_ref_decl()?,
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
                // Mojo docstrings are triple-quoted string statements placed at
                // the start of a module/declaration body. Documentation metadata
                // is not retained yet, but it has no runtime effect.
                Some(Token::TripleStringLiteral(_)) => {
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
                // Without a following `=`, the same comma syntax is a bare tuple
                // display (`a, b`), because the comma creates the tuple.
                self.next_token()?; // consume ','
                if matches!(
                    self.peek_token()?,
                    Some(Token::Assign | Token::Newline | Token::Semicolon | Token::Eof) | None
                ) {
                    break;
                }
                targets.push(self.parse_expression(Precedence::Lowest)?);
            }
            if !matches!(self.peek_token()?, Some(Token::Assign)) {
                let start = targets[0].span.0;
                let display = self.node(ExprKind::TupleLit(targets), start);
                self.expect_stmt_end()?;
                return Ok(StmtKind::Expr(display));
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
            let value = self.parse_tuple_display()?;
            self.expect_stmt_end()?;
            return Ok(StmtKind::Unpack { targets, value });
        }

        if matches!(self.peek_token()?, Some(Token::Assign)) {
            self.next_token()?; // consume '='
            let value = self.parse_tuple_display()?;
            let stmt = if let ExprKind::Identifier(name) = expr.kind {
                StmtKind::Assign { name, value }
            } else if matches!(
                expr.kind,
                ExprKind::Member { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::Slice { .. }
                    | ExprKind::MultiIndex { .. }
                    | ExprKind::TypeApply { .. }
            ) {
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
                ExprKind::Identifier(_)
                    | ExprKind::Member { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::TypeApply { .. }
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
        if matches!(self.peek_token()?, Some(Token::Comma)) {
            let mut targets = vec![Expr::new(ExprKind::Identifier(name), self.last_span)];
            while matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?;
                let target = self.expect_identifier("Expected another variable name")?;
                targets.push(Expr::new(ExprKind::Identifier(target), self.last_span));
            }
            self.expect(
                Token::Assign,
                "Expected '=' after variable unpacking targets",
            )?;
            let value = self.parse_tuple_display()?;
            self.expect_stmt_end()?;
            return Ok(StmtKind::Unpack { targets, value });
        }
        // An optional `: Type`; omitting it infers the type from `value`.
        let ty = if matches!(self.peek_token()?, Some(Token::Colon)) {
            self.next_token()?; // consume ':'
            Some(self.parse_type()?)
        } else {
            None
        };
        let value = if matches!(self.peek_token()?, Some(Token::Assign)) {
            self.next_token()?;
            self.parse_tuple_display()?
        } else {
            Expr::new(ExprKind::Uninitialized, self.last_span)
        };
        self.expect_stmt_end()?;
        Ok(StmtKind::VarDecl { name, ty, value })
    }

    /// Parse an expression in a statement-RHS position, where a top-level comma
    /// forms a tuple without requiring parentheses (`var pair = 1, "one"`).
    /// Delimited expression lists (call arguments, list elements, and so on) keep
    /// using `parse_expression` so their commas remain delimiters.
    fn parse_tuple_display(&mut self) -> Result<Expr, ParseError> {
        let first = self.parse_expression(Precedence::Lowest)?;
        if !matches!(self.peek_token()?, Some(Token::Comma)) {
            return Ok(first);
        }
        let start = first.span.0;
        let mut elements = vec![first];
        while matches!(self.peek_token()?, Some(Token::Comma)) {
            self.next_token()?;
            if matches!(
                self.peek_token()?,
                Some(Token::Newline | Token::Semicolon | Token::Eof) | None
            ) {
                break;
            }
            elements.push(self.parse_expression(Precedence::Lowest)?);
        }
        Ok(self.node(ExprKind::TupleLit(elements), start))
    }

    /// `ref name = expression` — Mojo's explicit reference binding. The AST
    /// preserves the distinction from an owned `var` so later phases cannot
    /// accidentally give it copy semantics.
    fn parse_ref_decl(&mut self) -> Result<StmtKind, ParseError> {
        let keyword = self.expect_identifier("Expected 'ref'")?;
        debug_assert_eq!(keyword, "ref");
        let name = self.expect_identifier("Expected a name after 'ref'")?;
        self.expect(Token::Assign, "Expected '=' after the reference name")?;
        let value = self.parse_expression(Precedence::Lowest)?;
        self.expect_stmt_end()?;
        Ok(StmtKind::RefDecl { name, value })
    }

    /// `comptime NAME[: Type] = value` — a compile-time constant.
    /// `comptime`, which introduces one of three forms: a compile-time constant
    /// `comptime NAME[: Type] = expr`, a compile-time conditional `comptime if …`,
    /// or a compile-time (unrolled) loop `comptime for …` (Mojo's modern spellings
    /// — the older `@parameter if`/`@parameter for` are deprecated).
    fn parse_comptime(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Comptime, "Expected 'comptime'")?;
        match self.peek_token()? {
            Some(Token::If) => {
                let (branches, orelse) = self.parse_if_rest()?;
                Ok(StmtKind::ComptimeIf { branches, orelse })
            }
            Some(Token::For) => {
                let (var, reference, owned, iter, body) = self.parse_for_rest()?;
                if reference || owned {
                    return Err(ParseError::UnexpectedToken(
                        Token::For,
                        "comptime for cannot use an explicit ref/var binding".to_string(),
                    ));
                }
                Ok(StmtKind::ComptimeFor { var, iter, body })
            }
            _ => {
                let name =
                    self.expect_identifier("Expected a name, 'if', or 'for' after 'comptime'")?;
                // Directive form, e.g. `comptime assert(condition), message`.
                if name == "assert" {
                    let mut args = if matches!(self.peek_token()?, Some(Token::LParen)) {
                        self.next_token()?;
                        let (args, _) = self.parse_call_args()?;
                        self.expect(Token::RParen, "Expected ')' after comptime directive")?;
                        args
                    } else {
                        vec![self.parse_expression(Precedence::Lowest)?]
                    };
                    if matches!(self.peek_token()?, Some(Token::Comma)) {
                        self.next_token()?;
                        args.push(self.parse_expression(Precedence::Lowest)?);
                    }
                    self.expect_stmt_end()?;
                    return Ok(StmtKind::Expr(Expr::new(
                        ExprKind::Call {
                            name,
                            param_args: Vec::new(),
                            args,
                            kwargs: Vec::new(),
                        },
                        self.last_span,
                    )));
                }
                if matches!(self.peek_token()?, Some(Token::LBracket)) {
                    self.parse_type_params()?;
                }
                // Mojo permits an optional annotation (`comptime N: Int = 1`).
                // The current AST stores the folded value only; parsing the type
                // here keeps syntax compatibility and validates that the annotation
                // itself is well formed.
                if matches!(self.peek_token()?, Some(Token::Colon)) {
                    self.next_token()?; // consume ':'
                    self.parse_type()?;
                }
                self.expect(
                    Token::Assign,
                    "Expected '=' after the comptime constant name (or its ': Type')",
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

        let captures = self.parse_unified_captures()?;

        let (raises, raises_type) = self.parse_raises_effect()?;
        if matches!(self.peek_token()?, Some(Token::Identifier(id)) if id == "abi") {
            self.next_token()?;
            self.expect(Token::LParen, "Expected '(' after abi")?;
            while !matches!(self.peek_token()?, Some(Token::RParen) | None) {
                self.next_token()?;
            }
            self.expect(Token::RParen, "Expected ')' after abi")?;
        }
        let ret = if matches!(self.peek_token()?, Some(Token::Arrow)) {
            self.next_token()?; // consume '->'
            Some(self.parse_type()?)
        } else {
            None
        };

        if matches!(self.peek_token()?, Some(Token::LBrace)) {
            self.next_token()?;
            while !matches!(self.peek_token()?, Some(Token::RBrace) | None) {
                self.next_token()?;
            }
            self.expect(Token::RBrace, "Expected '}' after function effects")?;
        }

        let where_clause = if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "where")
        {
            self.next_token()?;
            Some(self.parse_expression(Precedence::Lowest)?)
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
            captures,
            raises,
            raises_type,
            ret,
            where_clause,
            body,
        })
    }

    fn parse_unified_captures(&mut self) -> Result<Option<CaptureList>, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "unified") {
            return Ok(None);
        }
        self.next_token()?;
        self.expect(Token::LBrace, "Expected '{' after 'unified'")?;
        let mut entries = Vec::new();
        let mut default_read = false;
        while !matches!(self.peek_token()?, Some(Token::RBrace)) {
            let mutable =
                matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "mut");
            if mutable {
                self.next_token()?;
            }
            let mut name = self.expect_identifier("Expected a captured name")?;
            let immutable = matches!(name.as_str(), "imm" | "read")
                && matches!(self.peek_token()?, Some(Token::Identifier(_)));
            if immutable {
                name = self.expect_identifier("Expected a name after the capture convention")?;
            }
            let moved = matches!(self.peek_token()?, Some(Token::Caret));
            if moved {
                self.next_token()?;
            }
            if matches!(name.as_str(), "imm" | "read") && !mutable && !immutable && !moved {
                default_read = true;
            } else {
                if entries.iter().any(|capture: &Capture| capture.name == name) {
                    return Err(ParseError::UnexpectedToken(
                        Token::Identifier(name.clone()),
                        format!("duplicate capture '{name}'"),
                    ));
                }
                entries.push(Capture {
                    name,
                    kind: if moved {
                        CaptureKind::Move
                    } else if mutable {
                        CaptureKind::Mut
                    } else {
                        CaptureKind::Read
                    },
                });
            }
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?;
            } else if !matches!(self.peek_token()?, Some(Token::RBrace)) {
                return Err(ParseError::UnexpectedToken(
                    self.next_token()?,
                    "Expected ',' or '}' in capture list".to_string(),
                ));
            }
        }
        self.next_token()?;
        Ok(Some(CaptureList {
            entries,
            default_read,
        }))
    }

    /// Parses an optional `raises` effect after a function's parameter list. An
    /// error type may follow (`raises ValidationError`).
    fn parse_raises_effect(&mut self) -> Result<(bool, Option<Type>), ParseError> {
        if !matches!(self.peek_token()?, Some(Token::Raises)) {
            return Ok((false, None));
        }
        // An optional error type follows, unless the next token ends the header.
        self.next_token()?; // consume 'raises'
        let error = if !matches!(self.peek_token()?, Some(Token::Arrow | Token::Colon)) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok((true, error))
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

        let parenthesized = matches!(self.peek_token()?, Some(Token::LParen));
        if parenthesized {
            self.next_token()?;
        }
        let names = if matches!(self.peek_token()?, Some(Token::Star)) {
            self.next_token()?; // consume '*'
            crate::ast::ImportNames::Wildcard
        } else {
            let mut targets = Vec::new();
            while !parenthesized || !matches!(self.peek_token()?, Some(Token::RParen)) {
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
        if parenthesized {
            self.expect(Token::RParen, "Expected ')' after imported names")?;
        }
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
                    params.push(self.finish_param(name, ParamKind::KwVariadic, None, None)?);
                }
                Some(Token::Star) => {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::Identifier(_))) {
                        // `*name: T` — positional variadic.
                        let name = self.expect_identifier("Expected a name after '*'")?;
                        params.push(self.finish_param(name, ParamKind::Variadic, None, None)?);
                    } else {
                        // bare `*` — keyword-only marker (not a parameter).
                        keyword_only = Some(params.len());
                    }
                }
                // Current Mojo places the ownership convention before the pack
                // marker: `var *args: *Ts` (not `*var args`).
                Some(Token::Var) => {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::Star)) {
                        self.next_token()?;
                        let name = self.expect_identifier("Expected a name after 'var *'")?;
                        params.push(self.finish_param(
                            name,
                            ParamKind::Variadic,
                            Some(ArgConvention::Var),
                            None,
                        )?);
                    } else {
                        let name = self
                            .expect_identifier("Expected a parameter name after the convention")?;
                        params.push(self.finish_param(
                            name,
                            ParamKind::Regular,
                            Some(ArgConvention::Var),
                            None,
                        )?);
                    }
                }
                // A regular parameter, with an optional convention prefix.
                _ => {
                    let (convention, origin, name) = self.parse_convention_and_name()?;
                    params.push(self.finish_param(name, ParamKind::Regular, convention, origin)?);
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

    /// The optional argument convention (`read`/`mut`/`var`/`out`) prefixing a
    /// regular parameter, plus its name. A convention word is only a convention
    /// when followed by the parameter name (another identifier); if it is followed
    /// by `:` it *is* the name (so `read` remains usable as a parameter name).
    fn parse_convention_and_name(
        &mut self,
    ) -> Result<
        (
            Option<ArgConvention>,
            Option<crate::ast::OriginSpec>,
            String,
        ),
        ParseError,
    > {
        let word = if matches!(self.peek_token()?, Some(Token::Var)) {
            self.next_token()?;
            "var".to_string()
        } else {
            self.expect_identifier("Expected a parameter name")?
        };
        // `word :` → `word` is the parameter name, no convention.
        if matches!(self.peek_token()?, Some(Token::Colon)) {
            return Ok((None, None, word));
        }
        let Some(convention) = (if word == "var" {
            Some(ArgConvention::Var)
        } else {
            convention_word(&word)
        }) else {
            return Err(ParseError::UnexpectedToken(
                Token::Identifier(word),
                "expected a parameter name (or a convention: imm/mut/var/out/ref)".into(),
            ));
        };
        // A `ref` convention may carry an origin specifier: `ref[origin] name`.
        let origin = if convention == ArgConvention::Ref {
            self.parse_optional_origin_specifier()?
        } else {
            None
        };
        let name = self.expect_identifier("Expected a parameter name after the convention")?;
        Ok((Some(convention), origin, name))
    }

    /// An optional `[origin]` origin specifier following `ref` (in a `ref[origin]`
    /// argument convention or `ref[origin] T` return type). The specifier is a
    /// comma-separated list of origin expressions (an arbitrary expression, a named
    /// origin, or `_`); it is retained for semantic resolution by the checker.
    fn parse_optional_origin_specifier(
        &mut self,
    ) -> Result<Option<crate::ast::OriginSpec>, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::LBracket)) {
            return Ok(None);
        }
        self.next_token()?; // consume '['
        let mut origins = Vec::new();
        loop {
            origins.push(self.parse_expression(Precedence::Lowest)?);
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?;
            } else {
                break;
            }
        }
        self.expect(Token::RBracket, "Expected ']' after the origin specifier")?;
        Ok(Some(origins))
    }

    /// Finishes a parameter after its name: `: type [= default]`.
    fn finish_param(
        &mut self,
        name: String,
        kind: ParamKind,
        convention: Option<ArgConvention>,
        origin: Option<crate::ast::OriginSpec>,
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
            origin,
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
        let (conforms, conformance_conditions, callable_conformance) =
            self.parse_struct_conformance()?;
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
                Token::TripleStringLiteral(_) => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
                }
                Token::Var => {
                    self.expect(Token::Var, "Expected 'var'")?;
                    let fname = self.expect_identifier("Expected a field name")?;
                    self.expect(Token::Colon, "Fields require a type annotation")?;
                    let ty = self.parse_type()?;
                    if matches!(self.peek_token()?, Some(Token::Assign)) {
                        self.next_token()?;
                        self.parse_expression(Precedence::Lowest)?;
                    }
                    self.expect_stmt_end()?;
                    fields.push(Param { name: fname, ty });
                }
                Token::Pass => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
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
            callable_conformance,
            conformance_conditions,
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
                if matches!(self.peek_token()?, Some(Token::RParen)) {
                    break;
                }
            } else {
                break;
            }
        }
        self.expect(Token::RParen, "Expected ')' after the conformance list")?;
        Ok(traits)
    }

    /// Current Mojo permits a predicate after an individual struct conformance:
    /// `Trait where conforms_to(T, Trait)`. Conditions are retained separately
    /// while the nominal trait-name list remains compatible with existing passes.
    fn parse_struct_conformance(&mut self) -> Result<StructConformanceList, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::LParen)) {
            return Ok((Vec::new(), Vec::new(), None));
        }
        self.next_token()?;
        let mut traits = Vec::new();
        let mut conditions = Vec::new();
        let mut callable = None;
        loop {
            if matches!(self.peek_token()?, Some(Token::Def)) {
                if callable.is_some() {
                    return Err(ParseError::UnexpectedToken(
                        Token::Def,
                        "a struct may declare only one def(...) callable conformance".into(),
                    ));
                }
                callable = Some(self.parse_type()?);
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::RParen)) {
                        break;
                    }
                    continue;
                }
                break;
            }
            let trait_name =
                self.expect_identifier("Expected a trait name in the conformance list")?;
            traits.push(trait_name.clone());
            if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "where") {
                self.next_token()?;
                let condition = self.parse_expression(Precedence::Lowest)?;
                conditions.push((trait_name, condition));
            }
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?;
                if matches!(self.peek_token()?, Some(Token::RParen)) {
                    break;
                }
            } else {
                break;
            }
        }
        self.expect(Token::RParen, "Expected ')' after the conformance list")?;
        Ok((traits, conditions, callable))
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
                Token::TripleStringLiteral(_) => {
                    self.next_token()?;
                    self.expect_stmt_end()?;
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
        let first = self.parse_type()?;
        let mut bounds = vec![first];
        while matches!(self.peek_token()?, Some(Token::Amp)) {
            self.next_token()?;
            bounds.push(self.parse_type()?);
        }
        let ty = if bounds.len() == 1 {
            bounds.pop().expect("one associated member annotation")
        } else {
            crate::ast::Type::Named(
                "$trait_composition".to_string(),
                bounds.into_iter().map(crate::ast::ParamArg::Type).collect(),
            )
        };
        self.expect_stmt_end()?;
        Ok(crate::ast::TraitComptime { name, ty })
    }

    /// `def name([convention] self [, params]) -> ret:` followed by an indented
    /// body that is either `...` (a pure requirement) or real statements (a
    /// **default implementation**, stored in `default_body`).
    fn parse_trait_method(&mut self) -> Result<crate::ast::TraitMethod, ParseError> {
        self.expect(Token::Def, "Expected 'def'")?;
        let name = self.expect_identifier("Expected a method name after 'def'")?;
        let type_params = self.parse_type_params()?;

        self.expect(Token::LParen, "Expected '(' after the method name")?;
        let first = if matches!(self.peek_token()?, Some(Token::Var)) {
            self.next_token()?;
            "var".to_string()
        } else {
            self.expect_identifier("A method's first parameter must be 'self'")?
        };
        let explicit = if first == "var" {
            Some(ArgConvention::Var)
        } else {
            convention_word(&first)
        };
        let (self_name, self_convention, self_origin) = if let Some(conv) = explicit {
            let origin = if conv == ArgConvention::Ref {
                self.parse_optional_origin_specifier()?
            } else {
                None
            };
            (
                self.expect_identifier("Expected 'self' after the receiver convention")?,
                Some(conv),
                origin,
            )
        } else {
            (first, None, None)
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

        let (raises, raises_type) = self.parse_raises_effect()?;
        let ret = if matches!(self.peek_token()?, Some(Token::Arrow)) {
            self.next_token()?;
            Some(self.parse_type()?)
        } else {
            None
        };

        if matches!(self.peek_token()?, Some(Token::LBrace)) {
            self.next_token()?;
            while !matches!(self.peek_token()?, Some(Token::RBrace) | None) {
                self.next_token()?;
            }
            self.expect(Token::RBrace, "Expected '}' after method effects")?;
        }
        let where_clause = if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "where")
        {
            self.next_token()?;
            Some(self.parse_expression(Precedence::Lowest)?)
        } else {
            None
        };
        self.expect(Token::Colon, "Expected ':' before the method body")?;
        // A body of exactly `...` is a pure requirement; anything else is a
        // default implementation (parsed, flagged unsupported by the checker).
        let default_body = self.parse_trait_method_body()?;

        Ok(crate::ast::TraitMethod {
            name,
            type_params,
            self_convention,
            self_origin,
            params,
            positional_only,
            keyword_only,
            raises,
            raises_type,
            ret,
            where_clause,
            default_body,
        })
    }

    /// `def name([convention] self [, params]) -> ret: <block>` inside a struct.
    fn parse_method(&mut self, decorators: Vec<Decorator>) -> Result<Method, ParseError> {
        self.expect(Token::Def, "Expected 'def'")?;
        let name = self.expect_identifier("Expected a method name after 'def'")?;
        let type_params = self.parse_type_params()?;

        self.expect(Token::LParen, "Expected '(' after the method name")?;
        // Detect the receiver. An instance method starts with `self`, optionally
        // carrying a convention (`mut self`, `out self`, `var self`, `imm self`
        // — convention words are contextual identifiers). A `@staticmethod` has no
        // `self`: its parameters (if any) start immediately. (A convention word as
        // the first token is read as `<conv> self`, so a static method whose first
        // parameter carries a convention is not distinguished — a rare case.)
        let first_is_self =
            matches!(self.peek_token()?, Some(Token::Identifier(id)) if id == "self");
        let first_is_convention = matches!(self.peek_token()?, Some(Token::Identifier(id)) if convention_word(id).is_some())
            || matches!(self.peek_token()?, Some(Token::Var));
        let (has_self, self_convention, self_origin) = if first_is_self {
            self.next_token()?; // consume 'self'
            (true, None, None)
        } else if first_is_convention {
            let conv = match self.peek_token()? {
                Some(Token::Identifier(id)) => convention_word(id),
                Some(Token::Var) => Some(ArgConvention::Var),
                _ => None,
            };
            // `ref self` may carry an origin specifier: `ref[origin] self`.
            self.next_token()?; // consume the convention word
            let origin = if conv == Some(ArgConvention::Ref) {
                self.parse_optional_origin_specifier()?
            } else {
                None
            };
            let self_name =
                self.expect_identifier("Expected 'self' after the receiver convention")?;
            if self_name != "self" {
                return Err(ParseError::UnexpectedToken(
                    Token::Identifier(self_name),
                    "a receiver convention must be followed by 'self'".into(),
                ));
            }
            (true, conv, origin)
        } else {
            // No receiver — a static method.
            (false, None, None)
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

        let (raises, raises_type) = self.parse_raises_effect()?;
        let ret = if matches!(self.peek_token()?, Some(Token::Arrow)) {
            self.next_token()?;
            Some(self.parse_type()?)
        } else {
            None
        };

        let where_clause = if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "where")
        {
            self.next_token()?;
            Some(self.parse_expression(Precedence::Lowest)?)
        } else {
            None
        };

        self.expect(Token::Colon, "Expected ':' before the method body")?;
        let body = self.parse_suite()?;

        Ok(Method {
            name,
            type_params,
            has_self,
            self_convention,
            self_origin,
            decorators,
            params,
            positional_only,
            keyword_only,
            raises,
            raises_type,
            ret,
            where_clause,
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
        let orelse = self.parse_loop_else()?;
        Ok(StmtKind::While { cond, body, orelse })
    }

    /// `for var in iter: <block>`
    fn parse_for(&mut self) -> Result<StmtKind, ParseError> {
        let (var, reference, owned, iter, body) = self.parse_for_rest()?;
        let orelse = self.parse_loop_else()?;
        Ok(StmtKind::For {
            var,
            reference,
            owned,
            iter,
            body,
            orelse,
        })
    }

    fn parse_loop_else(&mut self) -> Result<Option<Vec<Stmt>>, ParseError> {
        if !matches!(self.peek_token()?, Some(Token::Else)) {
            return Ok(None);
        }
        self.next_token()?;
        self.expect(Token::Colon, "Expected ':' after loop 'else'")?;
        Ok(Some(self.parse_suite()?))
    }

    /// Parses a `for var in iter: <block>` — the current token must be `for`.
    /// Shared by the runtime `for` and the compile-time `comptime for`.
    fn parse_for_rest(&mut self) -> Result<(String, bool, bool, Expr, Vec<Stmt>), ParseError> {
        self.expect(Token::For, "Expected 'for'")?;
        let reference = if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "ref")
        {
            self.next_token()?;
            true
        } else {
            false
        };
        let owned = if !reference && matches!(self.peek_token()?, Some(Token::Var)) {
            self.next_token()?;
            true
        } else {
            false
        };
        let var = self.expect_identifier("Expected a loop variable name after 'for'")?;
        self.expect(Token::In, "Expected 'in' after the loop variable")?;
        let iter = self.parse_expression(Precedence::Lowest)?;
        self.expect(Token::Colon, "Expected ':' after the for-loop iterable")?;
        let body = self.parse_suite()?;
        Ok((var, reference, owned, iter, body))
    }

    /// `return` or `return expr`
    fn parse_return(&mut self) -> Result<StmtKind, ParseError> {
        self.expect(Token::Return, "Expected 'return'")?;
        let value = match self.peek_token()? {
            Some(Token::Newline) | Some(Token::Eof) | None => None,
            _ => Some(self.parse_tuple_display()?),
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
            // A variadic type-pack reference in `*args: *ArgTypes`.
            Token::Star => {
                let name = self.expect_identifier("Expected a type-pack name after '*'")?;
                Ok(Type::Named(format!("*{name}"), Vec::new()))
            }
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
                    let origin = self.parse_optional_origin_specifier()?;
                    Ok(Type::Ref {
                        referent: Box::new(self.parse_type()?),
                        origin,
                    })
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
        // Function signatures may themselves be parameterized, e.g.
        // `def[width: Int](Int) capturing[_] -> None`. Their compile-time
        // parameter declarations do not yet affect the syntax-only `Type` AST.
        if matches!(self.peek_token()?, Some(Token::LBracket)) {
            self.parse_type_params()?;
        }
        self.expect(Token::LParen, "Expected '(' in a function type")?;
        let mut params = Vec::new();
        if !matches!(self.peek_token()?, Some(Token::RParen)) {
            loop {
                if matches!(self.peek_token()?, Some(Token::Slash | Token::DoubleSlash)) {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::Comma)) {
                        self.next_token()?;
                    }
                    continue;
                }
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
        let mut raises_type = None;
        loop {
            match self.peek_token()? {
                Some(Token::Identifier(id)) if id == "thin" => {
                    self.next_token()?;
                    thin = true;
                }
                Some(Token::Raises) => {
                    self.next_token()?;
                    raises = true;
                    if !matches!(self.peek_token()?, Some(Token::Arrow)) {
                        raises_type = Some(Box::new(self.parse_type()?));
                    }
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
                Some(Token::Identifier(id)) if id == "capturing" => {
                    self.next_token()?;
                    self.expect(Token::LBracket, "Expected '[' after 'capturing'")?;
                    while !matches!(self.peek_token()?, Some(Token::RBracket) | None) {
                        self.next_token()?;
                    }
                    self.expect(Token::RBracket, "Expected ']' after capturing origins")?;
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
            raises_type,
        })
    }

    /// Parses a parameter-argument list `'[' param_arg (',' param_arg)* ']'`. The
    /// next token must be `[`. Used for `Pair[Int]` / `FixedBuffer[8]`.
    fn parse_param_args(&mut self) -> Result<Vec<crate::ast::ParamArg>, ParseError> {
        self.expect(Token::LBracket, "Expected '[' to begin parameter arguments")?;
        let mut args = Vec::new();
        loop {
            let mut arg = self.parse_param_arg()?;
            if matches!(self.peek_token()?, Some(Token::Assign)) {
                let name = match &arg {
                    crate::ast::ParamArg::Value(Expr {
                        kind: ExprKind::Identifier(name),
                        ..
                    }) => name.clone(),
                    _ => {
                        return Err(ParseError::UnexpectedToken(
                            Token::Assign,
                            "a compile-time keyword argument requires a name".into(),
                        ));
                    }
                };
                self.next_token()?;
                arg = crate::ast::ParamArg::Named {
                    name,
                    value: Box::new(crate::ast::ParamArg::Value(
                        self.parse_expression(Precedence::Lowest)?,
                    )),
                };
            }
            args.push(arg);
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
                if matches!(self.peek_token()?, Some(Token::RBracket)) {
                    break;
                }
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
            let start = self.peek_start();
            let ty = self.parse_type()?;
            if matches!(self.peek_token()?, Some(Token::LParen)) {
                let name = match &ty {
                    Type::Int => Some("Int"),
                    Type::UInt => Some("UInt"),
                    Type::Bool => Some("Bool"),
                    Type::String => Some("String"),
                    Type::Float64 => Some("Float64"),
                    _ => None,
                };
                if let Some(name) = name {
                    let atom = Expr::new(
                        ExprKind::Identifier(name.to_string()),
                        (start, self.last_span.1),
                    );
                    return Ok(ParamArg::Value(
                        self.parse_expression_from(atom, Precedence::Lowest)?,
                    ));
                }
            }
            return Ok(ParamArg::Type(ty));
        }
        if let Some(Token::Identifier(_)) = self.peek_token()? {
            let id = self.expect_identifier("unreachable: peeked identifier")?;
            let id_span = self.last_span;
            if matches!(self.peek_token()?, Some(Token::LBracket)) {
                let args = self.parse_param_args()?;
                if matches!(self.peek_token()?, Some(Token::LParen)) {
                    self.next_token()?;
                    let (call_args, kwargs) = self.parse_call_args()?;
                    self.expect(Token::RParen, "Expected ')' after arguments")?;
                    return Ok(ParamArg::Value(Expr::new(
                        ExprKind::Call {
                            name: id,
                            param_args: args,
                            args: call_args,
                            kwargs,
                        },
                        (id_span.0, self.last_span.1),
                    )));
                }
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
    /// `None`, `Self`, a function type, or Mojito's reference-type extension) —
    /// used to classify a parameter argument.
    fn peek_starts_type(&mut self) -> Result<bool, ParseError> {
        Ok(match self.peek_token()? {
            Some(Token::None | Token::Def | Token::Star) => true,
            Some(Token::Identifier(id)) => {
                matches!(
                    id.as_str(),
                    "Int" | "UInt" | "Bool" | "String" | "Float64" | "Self" | "ref"
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
        let mut infer_only = false;
        loop {
            // Mojo's `//` marker makes following parameters infer-only. Keep the
            // syntax even though inference policy is not represented yet.
            if matches!(self.peek_token()?, Some(Token::DoubleSlash)) {
                self.next_token()?;
                infer_only = true;
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?;
                }
                if matches!(self.peek_token()?, Some(Token::RBracket)) {
                    break;
                }
                continue;
            }
            // Variadic compile-time parameter pack marker.
            let variadic = if matches!(self.peek_token()?, Some(Token::Star)) {
                self.next_token()?;
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?;
                    continue;
                }
                true
            } else {
                false
            };
            let mut name = self.expect_identifier("Expected a type-parameter name")?;
            if variadic {
                name.insert(0, '*');
            }
            self.expect(
                Token::Colon,
                "A type parameter requires a ': bound' (e.g. 'T: Copyable')",
            )?;
            let first_bound = if matches!(self.peek_token()?, Some(Token::Def)) {
                self.parse_type()?;
                "<function type>".to_string()
            } else {
                self.expect_identifier("Expected a trait or type in the type-parameter bound")?
            };
            // Origin parameters use `Origin[mut=<bool expression>]`. Preserve the
            // Origin classification and parse the mutability expression; semantic
            // origin parameters are deliberately deferred.
            let mut value_type = None;
            let origin_mutability =
                if first_bound == "Origin" && matches!(self.peek_token()?, Some(Token::LBracket)) {
                    self.next_token()?;
                    let key = self.expect_identifier("Expected 'mut' in Origin[mut=...]")?;
                    if key != "mut" {
                        return Err(ParseError::UnexpectedToken(
                            Token::Identifier(key),
                            "expected 'mut' in Origin[mut=...]".into(),
                        ));
                    }
                    self.expect(Token::Assign, "Expected '=' after 'mut' in Origin")?;
                    let mutability = self.parse_expression(Precedence::Lowest)?;
                    self.expect(Token::RBracket, "Expected ']' after Origin mutability")?;
                    Some(mutability)
                } else if matches!(self.peek_token()?, Some(Token::LBracket)) {
                    let args = self.parse_param_args()?;
                    value_type = Some(Type::Named(first_bound.clone(), args));
                    None
                } else {
                    None
                };
            let mut bounds = vec![first_bound];
            while matches!(self.peek_token()?, Some(Token::Amp)) {
                self.next_token()?; // consume '&'
                bounds.push(self.expect_identifier("Expected a trait name after '&'")?);
            }
            if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "where") {
                return Err(ParseError::UnexpectedToken(
                    self.next_token()?,
                    "parameter-list 'where' clauses were removed; place the constraint after the function return type"
                        .into(),
                ));
            }
            let default = if matches!(self.peek_token()?, Some(Token::Assign)) {
                self.next_token()?;
                Some(self.parse_expression(Precedence::Lowest)?)
            } else {
                None
            };
            params.push(crate::ast::TypeParam {
                name,
                bounds,
                value_type,
                origin_mutability,
                infer_only,
                default,
                constraints: Vec::new(),
            });
            if matches!(self.peek_token()?, Some(Token::Comma)) {
                self.next_token()?; // consume ','
                if matches!(self.peek_token()?, Some(Token::RBracket)) {
                    break;
                }
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

    /// Parse the postfix generator/filter sequence shared by list, set, and
    /// dictionary comprehensions. Expressions use `Conditional` as their stop
    /// precedence so the next clause's `if` is not mistaken for a ternary.
    fn parse_comprehension_clauses(
        &mut self,
    ) -> Result<Vec<crate::ast::ComprehensionClause>, ParseError> {
        use crate::ast::ComprehensionClause;

        let mut clauses = Vec::new();
        loop {
            match self.peek_token()? {
                Some(Token::For) => {
                    self.next_token()?;
                    let reference = if matches!(self.peek_token()?, Some(Token::Identifier(word)) if word == "ref")
                    {
                        self.next_token()?;
                        true
                    } else {
                        false
                    };
                    let owned = if !reference && matches!(self.peek_token()?, Some(Token::Var)) {
                        self.next_token()?;
                        true
                    } else {
                        false
                    };
                    let var = self.expect_identifier(
                        "Expected a comprehension variable after 'for'",
                    )?;
                    self.expect(Token::In, "Expected 'in' in comprehension")?;
                    let iter = self.parse_expression(Precedence::Conditional)?;
                    clauses.push(ComprehensionClause::For {
                        var,
                        reference,
                        owned,
                        iter: Box::new(iter),
                    });
                }
                Some(Token::If) => {
                    if clauses.is_empty() {
                        return Err(ParseError::UnexpectedToken(
                            Token::If,
                            "a comprehension must begin with a 'for' clause".to_string(),
                        ));
                    }
                    self.next_token()?;
                    let condition = self.parse_expression(Precedence::Conditional)?;
                    clauses.push(ComprehensionClause::If(Box::new(condition)));
                }
                _ => break,
            }
        }
        if clauses.is_empty() {
            return Err(ParseError::UnexpectedToken(
                self.peek_token()?.cloned().unwrap_or(Token::Eof),
                "expected a comprehension 'for' clause".to_string(),
            ));
        }
        Ok(clauses)
    }

    /// Parse one or more adjacent ordinary/triple string tokens, or one or more
    /// adjacent t-string tokens. Mojo concatenates within each family; a regular
    /// string and a `TString` remain distinct types and cannot form one literal.
    fn build_string_sequence(&mut self, first: Token, start: usize) -> Result<Expr, ParseError> {
        let tstring_sequence = matches!(first, Token::TString { .. });
        let mut tokens = vec![first];
        while if tstring_sequence {
            matches!(self.peek_token()?, Some(Token::TString { .. }))
        } else {
            matches!(
                self.peek_token()?,
                Some(Token::StringLiteral(_)) | Some(Token::TripleStringLiteral(_))
            )
        } {
            tokens.push(self.next_token()?);
        }

        if !tstring_sequence {
            let mut value = String::new();
            for token in tokens {
                match token {
                    Token::StringLiteral(piece) | Token::TripleStringLiteral(piece) => {
                        value.push_str(&piece);
                    }
                    _ => unreachable!("t-string entered an ordinary literal sequence"),
                }
            }
            return Ok(self.node(ExprKind::Str(value), start));
        }

        let mut parts = Vec::new();
        let mut all_tstrings_raw = true;
        for token in tokens {
            match token {
                Token::TString { chunks, raw } => {
                    all_tstrings_raw &= raw;
                    for chunk in chunks {
                        match chunk {
                            TStringChunk::Text(text) => push_tstring_literal(&mut parts, text),
                            TStringChunk::Interp(src) => {
                                parts.push(TStringPart::Expr(Box::new(parse_interpolation(&src)?)))
                            }
                        }
                    }
                }
                _ => unreachable!("ordinary string entered a t-string sequence"),
            }
        }
        Ok(self.node(
            ExprKind::TString {
                parts,
                raw: all_tstrings_raw,
            },
            start,
        ))
    }

    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek_start();
        let token = self.next_token()?;
        match token {
            Token::IntLiteral(val) => Ok(self.node(ExprKind::Int(val), start)),
            Token::FloatLiteral(val) => Ok(self.node(ExprKind::Float(val), start)),
            Token::BoolLiteral(val) => Ok(self.node(ExprKind::Bool(val), start)),
            token @ (Token::StringLiteral(_)
            | Token::TripleStringLiteral(_)
            | Token::TString { .. }) => self.build_string_sequence(token, start),
            Token::None => Ok(self.node(ExprKind::None, start)),
            Token::Ellipsis => Ok(self.node(ExprKind::Identifier("...".into()), start)),
            Token::Identifier(id) => Ok(self.node(ExprKind::Identifier(id), start)),
            Token::Def => {
                let ty = self.parse_function_type_tail()?;
                Ok(self.node(ExprKind::TypeValue(ty), start))
            }
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
            // but is retained so an enclosing type annotation can supply it.
            Token::LBracket => {
                if matches!(self.peek_token()?, Some(Token::RBracket)) {
                    self.next_token()?; // consume ']'
                    return Ok(self.node(ExprKind::ListLit(Vec::new()), start));
                }
                let first = self.parse_expression(Precedence::Lowest)?;
                if matches!(self.peek_token()?, Some(Token::For)) {
                    let clauses = self.parse_comprehension_clauses()?;
                    self.expect(Token::RBracket, "Expected ']' after list comprehension")?;
                    return Ok(self.node(
                        ExprKind::Comprehension {
                            kind: crate::ast::CollectionKind::List,
                            key: None,
                            value: Box::new(first),
                            clauses,
                        },
                        start,
                    ));
                }
                let mut elems = vec![first];
                while matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::RBracket)) {
                        break;
                    }
                    elems.push(self.parse_expression(Precedence::Lowest)?);
                }
                self.expect(Token::RBracket, "Expected ']' after list elements")?;
                Ok(self.node(ExprKind::ListLit(elems), start))
            }
            Token::LBrace => {
                if matches!(self.peek_token()?, Some(Token::RBrace)) {
                    self.next_token()?;
                    return Ok(self.node(ExprKind::BraceLit(Vec::new()), start));
                }
                let first_key = self.parse_expression(Precedence::Lowest)?;
                let first_value = if matches!(self.peek_token()?, Some(Token::Colon)) {
                    self.next_token()?;
                    Some(self.parse_expression(Precedence::Lowest)?)
                } else {
                    None
                };
                if matches!(self.peek_token()?, Some(Token::For)) {
                    let kind = if first_value.is_some() {
                        crate::ast::CollectionKind::Dict
                    } else {
                        crate::ast::CollectionKind::Set
                    };
                    let value = first_value.unwrap_or_else(|| first_key.clone());
                    let clauses = self.parse_comprehension_clauses()?;
                    self.expect(Token::RBrace, "Expected '}' after collection comprehension")?;
                    return Ok(self.node(
                        ExprKind::Comprehension {
                            kind,
                            key: (kind == crate::ast::CollectionKind::Dict)
                                .then(|| Box::new(first_key)),
                            value: Box::new(value),
                            clauses,
                        },
                        start,
                    ));
                }
                let dictionary = first_value.is_some();
                let mut entries = vec![(first_key, first_value)];
                while matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?;
                    if matches!(self.peek_token()?, Some(Token::RBrace)) {
                        break;
                    }
                    let key = self.parse_expression(Precedence::Lowest)?;
                    let value = if matches!(self.peek_token()?, Some(Token::Colon)) {
                        self.next_token()?;
                        Some(self.parse_expression(Precedence::Lowest)?)
                    } else {
                        None
                    };
                    if dictionary != value.is_some() {
                        return Err(ParseError::UnexpectedToken(
                            self.peek_token()?.cloned().unwrap_or(Token::RBrace),
                            "set elements and dictionary key/value pairs cannot be mixed"
                                .to_string(),
                        ));
                    }
                    entries.push((key, value));
                }
                self.expect(Token::RBrace, "Expected '}' after brace literal")?;
                Ok(self.node(ExprKind::BraceLit(entries), start))
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
        if matches!(self.peek_token()?, Some(Token::Caret)) && left.span.1 == self.peek_start() {
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
            return self.parse_bracket_suffix(left, start);
        }

        // Postfix call without explicit parameters: `IDENT '(' args ')'`.
        if matches!(self.peek_token()?, Some(Token::LParen)) {
            self.next_token()?; // consume '('
            let (args, kwargs) = self.parse_call_args()?;
            self.expect(Token::RParen, "Expected ')' after arguments")?;
            let kind = match left.kind {
                ExprKind::Identifier(name) => ExprKind::Call {
                    name,
                    param_args: Vec::new(),
                    args,
                    kwargs,
                },
                _ => ExprKind::Invoke {
                    callee: Box::new(left),
                    param_args: Vec::new(),
                    args,
                    kwargs,
                },
            };
            return Ok(self.node(kind, start));
        }

        // Walrus / named expression: `name := value`. The target must be a bare
        // name. MIR preserves this as an explicit unsupported operation.
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
            Token::At => InfixOp::MatMul,
            Token::Shl => InfixOp::Shl,
            Token::Shr => InfixOp::Shr,
            Token::Amp => InfixOp::BitAnd,
            Token::Pipe => InfixOp::BitOr,
            Token::Caret => InfixOp::BitXor,
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

    fn parse_bracket_suffix(&mut self, object: Expr, start: usize) -> Result<Expr, ParseError> {
        self.expect(Token::LBracket, "Expected '['")?;
        if matches!(self.peek_token()?, Some(Token::RBracket)) {
            self.next_token()?;
            return Ok(self.node(
                ExprKind::Index {
                    object: Box::new(object),
                    index: Box::new(Expr::new(ExprKind::None, self.last_span)),
                },
                start,
            ));
        }

        let mut items = Vec::new();
        loop {
            if matches!(self.peek_token()?, Some(Token::Colon)) {
                let (upper, step, explicit_step) = self.parse_slice_components()?;
                items.push(ParsedBracketItem::Slice {
                    lower: None,
                    upper,
                    step,
                    explicit_step,
                });
            } else {
                let mut argument = self.parse_param_arg()?;
                if matches!(self.peek_token()?, Some(Token::Assign)) {
                    let name = param_argument_name(&argument)?;
                    self.next_token()?;
                    argument = crate::ast::ParamArg::Named {
                        name,
                        value: Box::new(crate::ast::ParamArg::Value(
                            self.parse_expression(Precedence::Lowest)?,
                        )),
                    };
                }
                if matches!(self.peek_token()?, Some(Token::Colon)) {
                    let lower = match argument {
                        crate::ast::ParamArg::Value(value) => Some(Box::new(value)),
                        _ => {
                            return Err(ParseError::UnexpectedToken(
                                Token::Colon,
                                "a slice bound must be an expression".into(),
                            ));
                        }
                    };
                    let (upper, step, explicit_step) = self.parse_slice_components()?;
                    items.push(ParsedBracketItem::Slice {
                        lower,
                        upper,
                        step,
                        explicit_step,
                    });
                } else {
                    items.push(ParsedBracketItem::Param(argument));
                }
            }

            if !matches!(self.peek_token()?, Some(Token::Comma)) {
                break;
            }
            self.next_token()?;
            if matches!(self.peek_token()?, Some(Token::RBracket)) {
                break;
            }
        }
        self.expect(Token::RBracket, "Expected ']' after a subscript")?;

        let contains_slice = items
            .iter()
            .any(|item| matches!(item, ParsedBracketItem::Slice { .. }));
        if matches!(self.peek_token()?, Some(Token::LParen)) {
            if contains_slice {
                return Err(ParseError::UnexpectedToken(
                    Token::LParen,
                    "slice expressions cannot be compile-time call parameters".into(),
                ));
            }
            let param_args = items
                .into_iter()
                .map(|item| match item {
                    ParsedBracketItem::Param(argument) => argument,
                    ParsedBracketItem::Slice { .. } => unreachable!(),
                })
                .collect();
            self.next_token()?;
            let (args, kwargs) = self.parse_call_args()?;
            self.expect(Token::RParen, "Expected ')' after arguments")?;
            let kind = match object.kind {
                ExprKind::Identifier(name) => ExprKind::Call {
                    name,
                    param_args,
                    args,
                    kwargs,
                },
                _ => ExprKind::Invoke {
                    callee: Box::new(object),
                    param_args,
                    args,
                    kwargs,
                },
            };
            return Ok(self.node(kind, start));
        }

        if contains_slice {
            let mut arguments = Vec::with_capacity(items.len());
            for item in items {
                arguments.push(match item {
                    ParsedBracketItem::Param(crate::ast::ParamArg::Value(value)) => {
                        SubscriptArg::Index(value)
                    }
                    ParsedBracketItem::Param(_) => {
                        return Err(ParseError::UnexpectedToken(
                            Token::RBracket,
                            "a mixed subscript argument must be an expression".into(),
                        ));
                    }
                    ParsedBracketItem::Slice {
                        lower,
                        upper,
                        step,
                        explicit_step,
                    } => SubscriptArg::Slice {
                        lower,
                        upper,
                        step,
                        explicit_step,
                    },
                });
            }
            if let [
                SubscriptArg::Slice {
                    lower,
                    upper,
                    step,
                    explicit_step,
                },
            ] = arguments.as_slice()
            {
                return Ok(self.node(
                    ExprKind::Slice {
                        object: Box::new(object),
                        lower: lower.clone(),
                        upper: upper.clone(),
                        step: step.clone(),
                        explicit_step: *explicit_step,
                    },
                    start,
                ));
            }
            return Ok(self.node(
                ExprKind::MultiIndex {
                    object: Box::new(object),
                    args: arguments,
                },
                start,
            ));
        }

        let param_args: Vec<_> = items
            .into_iter()
            .map(|item| match item {
                ParsedBracketItem::Param(argument) => argument,
                ParsedBracketItem::Slice { .. } => unreachable!(),
            })
            .collect();

        // `reflect[T]` is the current Mojo reflection handle.  Unlike an
        // ordinary lower-case expression followed by one subscript, its
        // brackets carry a compile-time type argument and the resulting handle
        // is used directly (`reflect[T].field["name"]`).  Recognize the builtin
        // here so user-defined types such as `Point` do not make the expression
        // look like a runtime `reflect[Point]` index operation.
        if matches!(&object.kind, ExprKind::Identifier(name) if name == "reflect") {
            return Ok(self.node(
                ExprKind::TypeApply {
                    name: "reflect".to_string(),
                    args: param_args,
                },
                start,
            ));
        }

        match <[_; 1]>::try_from(param_args) {
            Ok([crate::ast::ParamArg::Value(index)]) => Ok(self.node(
                ExprKind::Index {
                    object: Box::new(object),
                    index: Box::new(index),
                },
                start,
            )),
            Ok([other]) => Ok(self.node(
                ExprKind::TypeApply {
                    name: call_name(object)?,
                    args: vec![other],
                },
                start,
            )),
            Err(param_args)
                if param_args
                    .iter()
                    .all(|argument| matches!(argument, crate::ast::ParamArg::Value(_)))
                    && expression_name_starts_lowercase(&object) =>
            {
                let args = param_args
                    .into_iter()
                    .map(|argument| match argument {
                        crate::ast::ParamArg::Value(value) => SubscriptArg::Index(value),
                        _ => unreachable!(),
                    })
                    .collect();
                Ok(self.node(
                    ExprKind::MultiIndex {
                        object: Box::new(object),
                        args,
                    },
                    start,
                ))
            }
            Err(param_args) => Ok(self.node(
                ExprKind::TypeApply {
                    name: call_name(object)?,
                    args: param_args,
                },
                start,
            )),
        }
    }

    /// Parse the tail after the first `:` of one slice item. Comma ends the item
    /// for a multi-dimensional subscript; a second colon is retained even when
    /// its step expression is omitted.
    fn parse_slice_components(&mut self) -> Result<ParsedSliceTail, ParseError> {
        self.expect(Token::Colon, "Expected ':' in a slice")?;
        let upper = if matches!(
            self.peek_token()?,
            Some(Token::Colon | Token::Comma | Token::RBracket)
        ) {
            None
        } else {
            Some(Box::new(self.parse_expression(Precedence::Lowest)?))
        };
        let explicit_step = matches!(self.peek_token()?, Some(Token::Colon));
        let step = if explicit_step {
            self.next_token()?;
            if matches!(self.peek_token()?, Some(Token::Comma | Token::RBracket)) {
                None
            } else {
                Some(Box::new(self.parse_expression(Precedence::Lowest)?))
            }
        } else {
            None
        };
        Ok((upper, step, explicit_step))
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
            Some(
                Token::Plus
                | Token::Minus
                | Token::Shl
                | Token::Shr
                | Token::Amp
                | Token::Pipe
                | Token::Caret,
            ) => Precedence::Sum,
            Some(Token::Star | Token::Slash | Token::DoubleSlash | Token::Percent | Token::At) => {
                Precedence::Product
            }
            Some(Token::DoubleStar) => Precedence::Power,
            // `[` begins an explicit compile-time parameter list on a call.
            Some(Token::LParen | Token::LBracket | Token::Dot) => Precedence::Call,
            _ => Precedence::Lowest,
        };
        Ok(prec)
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
            if matches!(self.peek_token()?, Some(Token::DoubleStar)) {
                self.next_token()?;
                let value = self.parse_expression(Precedence::Lowest)?;
                if !matches!(&value.kind, ExprKind::Transfer(_)) {
                    return Err(ParseError::UnexpectedToken(
                        Token::DoubleStar,
                        "keyword forwarding requires a transferred StringDict (`**kwargs^`)".into(),
                    ));
                }
                kwargs.push(KwArg {
                    name: crate::ast::FORWARDED_KWARGS_NAME.to_string(),
                    value,
                });
                if matches!(self.peek_token()?, Some(Token::Comma)) {
                    self.next_token()?;
                    if !matches!(self.peek_token()?, Some(Token::RParen)) {
                        return Err(ParseError::UnexpectedToken(
                            Token::Comma,
                            "`**kwargs^` must be the final call argument".into(),
                        ));
                    }
                }
                break;
            }
            let expr = if matches!(self.peek_token()?, Some(Token::Star)) {
                let start = self.peek_start();
                self.next_token()?;
                let value = self.parse_expression(Precedence::Lowest)?;
                self.node(ExprKind::Spread(Box::new(value)), start)
            } else {
                self.parse_expression(Precedence::Lowest)?
            };
            if let ExprKind::Identifier(name) = &expr.kind
                && matches!(self.peek_token()?, Some(Token::Assign) | Some(Token::Colon))
            {
                self.next_token()?; // consume '=' or ':'
                let value = self.parse_expression(Precedence::Lowest)?;
                kwargs.push(KwArg {
                    name: name.clone(),
                    value,
                });
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

/// Append a literal component while coalescing neighboring text. Keeping the
/// normalized t-string tree compact also gives MIR one constant per run rather
/// than one per source token.
fn push_tstring_literal(parts: &mut Vec<TStringPart>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(TStringPart::Literal(previous)) = parts.last_mut() {
        previous.push_str(&text);
    } else {
        parts.push(TStringPart::Literal(text));
    }
}

/// Maps a contextual convention word (`read`/`mut`/`out`/`ref`) to its
/// `ArgConvention`, or `None` for any other identifier.
fn convention_word(word: &str) -> Option<ArgConvention> {
    match word {
        "imm" | "read" => Some(ArgConvention::Read),
        "mut" => Some(ArgConvention::Mut),
        "out" => Some(ArgConvention::Out),
        "ref" => Some(ArgConvention::Ref),
        "deinit" => Some(ArgConvention::Deinit),
        _ => None,
    }
}

fn param_argument_name(arg: &crate::ast::ParamArg) -> Result<String, ParseError> {
    match arg {
        crate::ast::ParamArg::Value(Expr {
            kind: ExprKind::Identifier(name),
            ..
        }) => Ok(name.clone()),
        _ => Err(ParseError::UnexpectedToken(
            Token::Assign,
            "a compile-time keyword argument requires a name".into(),
        )),
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

fn expression_name_starts_lowercase(expression: &Expr) -> bool {
    matches!(
        &expression.kind,
        ExprKind::Identifier(name)
            if name.chars().next().is_some_and(|character| character.is_lowercase())
    )
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
        InfixOp::Add
        | InfixOp::Sub
        | InfixOp::Shl
        | InfixOp::Shr
        | InfixOp::BitAnd
        | InfixOp::BitOr
        | InfixOp::BitXor => Precedence::Sum,
        InfixOp::Mul | InfixOp::Div | InfixOp::FloorDiv | InfixOp::Mod | InfixOp::MatMul => {
            Precedence::Product
        }
        // Right-associative: parse the right operand one level below `**` so that
        // a following `**` (Power > Unary) is re-absorbed (`a ** b ** c` = `a ** (b ** c)`).
        InfixOp::Pow => Precedence::Unary,
    }
}
