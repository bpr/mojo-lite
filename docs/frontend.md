# Frontend: Lexer And Parser

This document describes mojito's lexer and parser. It stops at the parsed AST.
The compiler stages after parsing are described in `architecture.md`.

The frontend is intentionally straightforward:

```text
source text
  -> Lexer
  -> tokens with spans
  -> Parser
  -> Vec<ast::Stmt>
```

The lexer handles indentation-sensitive layout and source spans. The parser is a
recursive-descent statement parser with a Pratt parser for expressions.

## Source Spans

The lexer and parser thread source positions through the entire frontend.

The canonical span type is:

```rust
pub type Span = (usize, usize);
```

A span is a half-open byte range:

```text
[start, end)
```

This is the single span representation used by:

- the lexer, which stamps every token
- the parser, which stamps AST expressions and statements
- later MIR diagnostics, which map generated temporaries back to source

The key idea is simple: the lexer owns byte offsets, and the parser preserves
them when building AST nodes.

### Span Lesson Learned

One practical lesson from this codebase: spans should have been added earlier.

It is tempting to follow YAGNI and keep the first parser AST span-free. For this
project, that was a false economy. Once later stages needed good diagnostics,
especially MIR ownership errors, adding spans after the AST and parser were
already established caused a fair amount of mechanical churn. Every token,
expression builder, statement wrapper, and test expectation had to be revisited.

For a compiler, source locations are not ornamental. Even if early diagnostics
are crude, threading spans from the beginning is usually cheaper than retrofitting
them after lowering and analysis depend on the tree.

## Lexer Overview

Module:

```text
src/lexer.rs
```

Token definitions:

```text
src/token.rs
```

The lexer implements:

- keywords and identifiers
- integer and float literals
- string literals and t-strings
- operators and punctuation
- indentation-sensitive layout tokens
- comments and blank lines
- explicit line continuation with backslash-newline
- token spans

It yields:

```rust
Iterator<Item = Result<(Token, Span), LexError>>
```

The parser therefore sees a stream of tokens where every token already knows its
source byte range.

## Token Set

The main token enum is `token::Token`.

It includes:

- Mojo-specific keywords such as `var`, `struct`, `trait`, `comptime`, `raises`
- Python-like keywords such as `def`, `return`, `if`, `for`, `try`, `except`
- identifiers
- integer, float, bool, string, and t-string literals
- arithmetic and comparison operators
- assignment and augmented assignment operators
- punctuation such as parentheses, brackets, commas, dots, arrows, colons
- layout tokens: `Newline`, `Indent`, `Dedent`, `Eof`

Keyword recognition is centralized in:

```rust
Token::keyword(text: &str) -> Option<Token>
```

`True` and `False` lex as boolean literals, not as separate keyword tokens.

## Indentation And The Offside Rule

Mojo uses indentation-sensitive blocks. mojito handles that in the lexer, not
the parser.

The lexer keeps:

```rust
indent_stack: Vec<usize>
at_line_start: bool
paren_count: usize
```

`indent_stack` starts as:

```rust
vec![0]
```

At the beginning of each logical line, the lexer counts leading spaces and
compares the count to the current top of the indentation stack:

- more spaces -> push the new level and emit `Indent`
- fewer spaces -> pop levels and emit one `Dedent` per popped level
- same spaces -> emit no layout token
- spaces not matching any previous indentation level -> `IndentationError`

Blank lines and comment-only lines are skipped and do not affect indentation.

The parser then consumes blocks using explicit layout tokens:

```text
INDENT statement* DEDENT
```

So a block parser can be small:

```rust
fn parse_block(&mut self) -> Result<Vec<Stmt>, ParseError> {
    self.expect(Token::Indent, "Expected an indented block")?;
    self.parse_block_body()
}
```

This keeps indentation logic out of recursive-descent parsing.

## Parentheses Suppress Layout

The lexer tracks nesting with `paren_count`.

The name is historical: it counts both parentheses and brackets:

- `(`
- `)`
- `[`
- `]`

When `paren_count > 0`:

- newlines are suppressed
- indentation is ignored
- bracketed type-argument lists, call arguments, list literals, and grouped
  expressions can span physical lines

This is the usual Python/Mojo offside-rule behavior: layout matters only at
top-level logical lines.

## Pending Tokens

Some source positions produce more than one token. For example:

- EOF may need trailing `Newline`, several `Dedent`s, then `Eof`
- a decrease in indentation may emit multiple `Dedent`s

The lexer handles this with:

```rust
pending_tokens: VecDeque<(Token, Span)>
```

`next()` first drains pending tokens. If the queue is empty, it scans more source.
This lets one scan step enqueue several layout tokens without complicating the
iterator interface.

## Token Spans

The lexer has:

```rust
token_start: usize
pos: usize
```

At the start of a scan iteration, `token_start` is set to `pos`. When a token is
emitted, it receives:

```rust
(token_start, pos)
```

Leading whitespace is skipped before `token_start` is refreshed for the next real
token, so ordinary token spans do not include indentation.

Synthetic layout tokens such as `Indent`, `Dedent`, `Newline`, and `Eof` also get
spans. Those spans are not semantically important, but keeping every token
spanned simplifies the parser interface.

## Comments And Blank Lines

Comments start with `#` and run to the end of the physical line.

At line start, a blank or comment-only line is skipped completely. It does not
emit `Newline`, and it does not affect indentation.

Inline comments skip only comment text. The newline remains to terminate the
logical statement.

## Explicit Line Continuation

A backslash immediately followed by `\n` or `\r\n` suppresses the newline.

The continued line's indentation is not significant because `at_line_start`
remains false. A backslash not followed by a newline is a lexer error.

## String Literals

The lexer supports:

- single-quoted strings
- double-quoted strings
- triple-quoted strings
- escapes such as `\n`, `\t`, `\\`, `\"`
- numeric escapes: octal, `\xHH`, `\uHHHH`, `\UHHHHHHHH`

Single-line strings cannot contain raw newlines. Triple-quoted strings can.

String escapes decode to Unicode scalar values.

## T-Strings

t-strings are lexed into chunks:

```rust
pub enum TStringChunk {
    Text(String),
    Interp(String),
}
```

The lexer does not parse interpolation expressions directly. It extracts the raw
source text inside `{...}` and stores it as `TStringChunk::Interp`.

The parser later reparses each interpolation with a fresh lexer/parser:

```rust
fn parse_interpolation(src: &str) -> Result<Expr, ParseError>
```

This keeps the main lexer simple. The brace matching is intentionally modest: it
tracks nested braces, but does not fully lex nested string literals inside an
interpolation.

Escaped `{{` and `}}` become literal braces.

## Parser Overview

Module:

```text
src/parser.rs
```

Input:

```rust
Iterator<Item = Result<(Token, Span), LexError>>
```

Output:

```rust
Vec<ast::Stmt>
```

The parser has two main layers:

1. recursive descent for statements, declarations, blocks, types, and parameter
   lists
2. Pratt parsing for expressions

The parser stores:

```rust
tokens: Peekable<I>
last_span: Span
last_significant_end: usize
```

`last_span` tracks the span of the most recently consumed token.

`last_significant_end` tracks the end of the most recent non-layout token. This
is used for statement spans so a statement does not accidentally include its
trailing newline.

## Statement Parsing

Top-level parsing is:

```rust
parse_program() -> Vec<Stmt>
```

It skips top-level blank lines and repeatedly calls:

```rust
parse_statement()
```

`parse_statement()` peeks at the first token and dispatches:

- `var` -> variable declaration
- `def` -> function declaration
- `struct` -> struct declaration
- decorators -> decorated `def` or `struct`
- `trait` -> trait declaration
- `comptime` -> comptime declaration/if/for
- `if`, `while`, `for`, `with`, `try`
- `return`, `raise`, `import`, `from`
- `pass`, `break`, `continue`
- otherwise, expression-or-assignment

Each statement parser returns a `StmtKind`. `parse_statement()` wraps it in a
`Stmt` with a source span.

The statement span starts at the first token and ends at
`last_significant_end`, not at the newline token consumed by
`expect_stmt_end()`.

## Blocks

The parser relies on lexer-produced layout tokens.

Block grammar is:

```text
Indent statement* Dedent
```

`parse_block()` consumes the opening `Indent`. `parse_block_body()` reads
statements until it consumes the matching `Dedent`.

This means statement parsers can be written in a direct style:

```text
if condition:
    body
```

becomes:

1. parse `if`
2. parse condition expression
3. expect `:`
4. expect statement end
5. parse indented block

## Expression-Or-Assignment

For statements that do not start with a keyword, the parser first parses an
expression.

Then it decides whether the result is:

- a bare expression statement
- assignment: `target = value`
- augmented assignment: `target += value`
- tuple unpacking: `a, b = value`
- place assignment: `p.x = value`, `xs[i] = value`

This works because all of those forms share a leading expression-like target.

The parser does only syntactic validation here. Later stages decide whether the
target is semantically assignable.

## Types And Parameter Arguments

Type parsing is recursive descent.

The parser recognizes:

- scalar type names such as `Int`, `Bool`, `String`, `Float64`
- nominal types such as `Point`
- parameterized types such as `Pair[Int]`
- `Self`
- `Self.T`
- function types such as `def(Int) -> Int`
- reference type syntax, which is parsed but later rejected as unsupported

Mojo-style parameter lists use square brackets for both type and value
arguments:

```mojo
Pair[Int]
FixedBuffer[8]
SIMD[DType.int32, 4]
```

The parser classifies clear type arguments as `ParamArg::Type` and value-like
arguments as `ParamArg::Value`. Ambiguous bare identifiers are left for the
checker to reinterpret based on the declaration's parameter kinds.

This is an intentional parser/checker split: the parser handles syntax, while
the checker knows which parameter positions expect types versus comptime values.

## Pratt Parser

Expressions are parsed with a Pratt parser, also known as precedence climbing.

The central functions are:

```rust
parse_expression(min_precedence)
parse_prefix()
parse_expression_from(left, min_precedence)
parse_infix(left)
peek_precedence()
```

The algorithm is:

1. parse a prefix expression, producing a left-hand expression
2. while the next token binds more tightly than `min_precedence`, parse it as an
   infix or postfix continuation of the current left-hand expression
3. return the final expression

In code shape:

```rust
fn parse_expression(&mut self, min_precedence: Precedence) -> Result<Expr, ParseError> {
    let left = self.parse_prefix()?;
    self.parse_expression_from(left, min_precedence)
}

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
```

This gives a small parser for a large expression surface.

## Binding Powers

The parser's precedence levels are:

```rust
enum Precedence {
    Lowest,
    Walrus,
    Conditional,
    Or,
    And,
    Not,
    Comparison,
    Sum,
    Product,
    Unary,
    Power,
    Call,
}
```

Lowest to highest:

- walrus: `name := value`
- conditional expression: `a if cond else b`
- `or`
- `and`
- prefix `not`
- comparisons and membership
- `+`, `-`
- `*`, `/`, `//`, `%`
- prefix unary `-`
- `**`
- calls, member access, indexing, and postfix transfer

Postfix forms bind at `Call`, the highest level.

## Prefix Parsing

`parse_prefix()` handles atoms and prefix operators:

- integer, float, bool, string, t-string, and `None` literals
- identifiers
- unary `-`
- unary `not`
- grouped expressions
- tuple literals
- list literals

Parenthesized expressions are disambiguated syntactically:

- `()` is the empty tuple
- `(x)` is grouping
- `(x,)` is a tuple
- `(x, y)` is a tuple

List literals parse as bracketed expression lists. Empty list literals are
rejected because mojito cannot infer their element type; use `List[T]()` for
an empty list.

## Infix And Postfix Parsing

`parse_infix(left)` handles everything that continues an already-parsed
left-hand expression.

Postfix forms:

- transfer: `expr^`
- member access: `expr.field`
- method call: `expr.method(args)`
- function call: `name(args)`
- explicit parameter call: `Name[Int](args)`
- index: `expr[index]`
- slice: `expr[lower:upper:step]`

Infix forms:

- arithmetic: `+`, `-`, `*`, `/`, `//`, `%`, `**`
- boolean: `and`, `or`
- comparison: `==`, `!=`, `<`, `>`, `<=`, `>=`
- membership: `in`, `not in`
- conditional expression
- walrus expression

Most binary operators are left-associative. The parser enforces this by parsing
the right operand at the operator's own precedence, so another equal-precedence
operator is not swallowed into the right-hand side.

Power is handled specially through its precedence mapping so it binds tighter
than unary in the intended way.

## Chained Comparisons

Comparisons get special treatment.

The parser recognizes:

```mojo
a < b <= c
0 <= i < n
x not in y
```

A single comparison remains an ordinary `ExprKind::Infix`, preserving simple
cases for later stages.

A chain of two or more comparison operators becomes:

```rust
ExprKind::Compare {
    first,
    rest,
}
```

The checker currently rejects some parsed comparison forms until their semantics
are implemented, but the AST can represent them.

## Calls, Keywords, And Explicit Parameters

Call parsing supports:

- positional arguments
- keyword arguments: `f(x=1)`
- trailing commas
- explicit type/value parameters: `f[Int, 4](x)`

Keyword arguments are parsed even if a later stage rejects them in some contexts.
The parser enforces the basic rule that a positional argument cannot follow a
keyword argument.

Explicit parameter arguments are parsed with the same `ParamArg` machinery used
for types.

## Slices

Slice syntax is parsed:

```mojo
xs[i:j:k]
xs[:j]
xs[i:]
xs[::k]
```

The parser represents it as `ExprKind::Slice`. Later semantic support is
separate; parsing it does not imply the compiler can execute it yet.

## T-String Interpolations

The lexer produces t-string chunks. The parser builds:

```rust
ExprKind::TString { parts, raw }
```

Each interpolation string is reparsed as a standalone expression. This means
t-string interpolation shares the same expression grammar as the rest of the
language.

## Error Boundary

The parser reports syntax errors only.

It does not try to decide:

- whether a type name exists
- whether a variable is defined
- whether a method exists
- whether a parsed feature is semantically supported
- whether a value parameter is a valid comptime expression

Those belong to the checker or later compiler stages.

This separation is visible throughout the codebase: many constructs parse and
then become `Unsupported` in the checker or VM until their semantics are
implemented.

## Why This Design Works

The lexer/parser split is intentionally plain:

- the lexer handles byte-level details, indentation, comments, strings, and spans
- the parser handles grammatical structure
- Pratt parsing keeps expression precedence compact
- recursive descent keeps statements and declarations readable
- spans are preserved without turning every parser function into a diagnostics
  framework

That is enough frontend machinery for mojito's goals. The deeper language
semantics begin after parsing, in the checker and compiler pipeline.
