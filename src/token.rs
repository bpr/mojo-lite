/// A source byte range `(start, end)` — a half-open `[start, end)` slice of the
/// original source. This is the single, canonical span type shared by the lexer
/// (which stamps each token), the parser (which propagates spans onto AST nodes),
/// and the MIR (whose `SpanTable` maps each temporary back to its origin span).
pub type Span = (usize, usize);

/// The zero-width, position-`0` span used for synthetic nodes that have no source
/// text (e.g. the `main()` call the evaluator synthesizes as a program entry).
pub const DUMMY_SPAN: Span = (0, 0);

/// The token set for the implemented subset of Mojo.
///
/// Keywords are split the way the reference tree-sitter Mojo grammar splits them
/// — into the ones Mojo **shares with Python** and the **Mojo-only** ones — since
/// mojito is a strict subset of Mojo, which is itself (largely) a superset of
/// Python's surface syntax. `Token::keyword` is the single lookup table.
/// One piece of a lexed t-string: either literal text or the raw source text of
/// an interpolation `{…}` (which the parser later parses into an `Expr`).
#[derive(Debug, Clone, PartialEq)]
pub enum TStringChunk {
    Text(String),
    Interp(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // --- Mojo-only keywords ---
    // Not present in Python: `var`, `struct`, `trait`, `comptime` (Mojo's
    // replacement for `alias`), and the `raises` function-effect keyword.
    // (Further Mojo keywords — `ref`, `out`, `where`, `capturing`, … — are not
    // reserved here; the features that need them aren't implemented, and `mut`
    // stays a soft/contextual word so `mut self` doesn't reserve the name.)
    Var,
    Struct,
    Trait,
    Comptime,
    Raises,

    // --- Keywords shared with Python ---
    Def,
    Return,
    Pass,
    None,
    And,
    Or,
    Not,
    If,
    Elif,
    Else,
    While,
    For,
    In,
    With,
    Break,
    Continue,
    Raise,
    Try,
    Except,
    Finally,
    Import,
    From,
    As,

    // Identifiers (includes type names like `Int`, `String`, `Bool`)
    Identifier(String),

    // Literals
    IntLiteral(i64),
    FloatLiteral(f64),
    BoolLiteral(bool),
    StringLiteral(String),
    /// A t-string (`t"…{expr}…"`) or raw t-string (`rt"…"`), lexed into
    /// alternating literal text and raw interpolation-expression text; the parser
    /// re-parses each interpolation into a real `Expr`. `raw` is true for `rt`.
    TString {
        chunks: Vec<TStringChunk>,
        raw: bool,
    },

    // Operators & Punctuation
    Assign,
    // Augmented assignment: `+= -= *= /= //= %= **=`
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    DoubleSlashEq,
    PercentEq,
    DoubleStarEq,
    Colon,
    ColonEq, // `:=` (the walrus / named-expression operator)
    Comma,
    Semicolon,
    Dot,      // `.`
    Ellipsis, // `...` (an unimplemented trait-method requirement)
    At,       // `@`
    Amp,      // `&` (trait-bound conjunction)
    Caret,    // `^` (transfer sigil)
    Arrow,    // `->`
    LParen,
    RParen,
    LBracket, // `[`  (type-parameter / type-argument list)
    RBracket, // `]`

    // Arithmetic operators
    Plus,        // `+`
    Minus,       // `-`
    Star,        // `*`
    DoubleStar,  // `**`
    Slash,       // `/`
    DoubleSlash, // `//`
    Percent,     // `%`

    // Comparison operators
    EqEq,  // `==`
    NotEq, // `!=`
    Lt,    // `<`
    Gt,    // `>`
    Le,    // `<=`
    Ge,    // `>=`

    // Structural (Offside Rule) Tokens
    Newline,
    Indent,
    Dedent,
    Eof,
}

impl Token {
    /// The keyword token for `text`, or `None` if it is an ordinary identifier.
    /// The single source of truth for the reserved-word set (the lexer calls this
    /// after scanning a word). `True`/`False` map to `BoolLiteral`s, not keywords.
    pub fn keyword(text: &str) -> Option<Token> {
        Some(match text {
            // Mojo-only
            "var" => Token::Var,
            "struct" => Token::Struct,
            "trait" => Token::Trait,
            "comptime" => Token::Comptime,
            "raises" => Token::Raises,
            // Shared with Python
            "def" => Token::Def,
            "return" => Token::Return,
            "pass" => Token::Pass,
            "None" => Token::None,
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            "if" => Token::If,
            "elif" => Token::Elif,
            "else" => Token::Else,
            "while" => Token::While,
            "for" => Token::For,
            "in" => Token::In,
            "with" => Token::With,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "raise" => Token::Raise,
            "try" => Token::Try,
            "except" => Token::Except,
            "finally" => Token::Finally,
            "import" => Token::Import,
            "from" => Token::From,
            "as" => Token::As,
            // Boolean literals lex as values, not keywords.
            "True" => Token::BoolLiteral(true),
            "False" => Token::BoolLiteral(false),
            _ => return None,
        })
    }
}
