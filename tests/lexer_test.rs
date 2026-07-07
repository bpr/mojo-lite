use mojo_lite::Lexer;
use mojo_lite::token::Token;

/// Collect all tokens, panicking if the lexer reports an error.
fn lex_all(source: &str) -> Vec<Token> {
    Lexer::new(source)
        .map(|r| r.expect("lexer error").0)
        .collect()
}

#[test]
fn lexes_typed_var_with_arithmetic() {
    let tokens = lex_all("var x: Int = 1 + 2 * 3");
    assert_eq!(
        tokens,
        vec![
            Token::Var,
            Token::Identifier("x".into()),
            Token::Colon,
            Token::Identifier("Int".into()),
            Token::Assign,
            Token::IntLiteral(1),
            Token::Plus,
            Token::IntLiteral(2),
            Token::Star,
            Token::IntLiteral(3),
            Token::Newline, // synthesized at EOF since the line had content
            Token::Eof,
        ]
    );
}

#[test]
fn lexes_def_with_indented_body() {
    let tokens = lex_all("def f() -> Int:\n    return 1");
    assert_eq!(
        tokens,
        vec![
            Token::Def,
            Token::Identifier("f".into()),
            Token::LParen,
            Token::RParen,
            Token::Arrow,
            Token::Identifier("Int".into()),
            Token::Colon,
            Token::Newline,
            Token::Indent,
            Token::Return,
            Token::IntLiteral(1),
            Token::Newline,
            Token::Dedent, // flushed at EOF
            Token::Eof,
        ]
    );
}

#[test]
fn disambiguates_two_char_operators() {
    // `->` vs `-`, `==` vs `=`, `<=`/`>=` vs `<`/`>`, and `!=`.
    let tokens = lex_all("a - 1 == b <= c >= d != e");
    assert_eq!(
        tokens,
        vec![
            Token::Identifier("a".into()),
            Token::Minus,
            Token::IntLiteral(1),
            Token::EqEq,
            Token::Identifier("b".into()),
            Token::Le,
            Token::Identifier("c".into()),
            Token::Ge,
            Token::Identifier("d".into()),
            Token::NotEq,
            Token::Identifier("e".into()),
            Token::Newline,
            Token::Eof,
        ]
    );
}

#[test]
fn lexes_keywords_and_string_literal() {
    let tokens = lex_all("var s: String = \"a\\nb\" and not False");
    assert_eq!(
        tokens,
        vec![
            Token::Var,
            Token::Identifier("s".into()),
            Token::Colon,
            Token::Identifier("String".into()),
            Token::Assign,
            Token::StringLiteral("a\nb".into()), // escape decoded
            Token::And,
            Token::Not,
            Token::BoolLiteral(false),
            Token::Newline,
            Token::Eof,
        ]
    );
}

/// Lex a single `"..."` string literal (as a whole program) and return its
/// decoded value.
fn lex_str(source: &str) -> String {
    match &lex_all(source)[0] {
        Token::StringLiteral(s) => s.clone(),
        other => panic!("expected a string literal, got {:?}", other),
    }
}

#[test]
fn decodes_simple_escapes() {
    // The C-style simple escapes, including the ones newly added (\r \a \b \f \v).
    assert_eq!(lex_str(r#""\n\t\r\\\'\"""#), "\n\t\r\\'\"");
    assert_eq!(lex_str(r#""\a\b\f\v""#), "\u{07}\u{08}\u{0C}\u{0B}");
}

#[test]
fn decodes_numeric_escapes() {
    // \xHH, octal \ooo, \uHHHH, and \UHHHHHHHH all name a code point — here all 'A'.
    assert_eq!(lex_str(r#""\x41\101A\U00000041""#), "AAAA");
    // Higher code points encode as UTF-8.
    assert_eq!(lex_str(r#""café""#), "café");
    assert_eq!(lex_str(r#""\U0001F600""#), "\u{1F600}");
    // Octal reads exactly three digits: \000 is NUL, \377 is 0xFF (ÿ, U+00FF).
    assert_eq!(lex_str(r#""\000""#), "\u{00}");
    assert_eq!(lex_str(r#""\377""#), "\u{FF}");
}

#[test]
fn rejects_invalid_escapes() {
    // An unknown letter, a bad hex digit, a surrogate/out-of-range scalar, a
    // truncated hex run, and an out-of-range octal lead are all lexer errors.
    for src in [r#""\q""#, r#""\xZZ""#, r#""\uD800""#, r#""\x4""#, r#""\4""#] {
        assert!(
            Lexer::new(src).any(|r| r.is_err()),
            "expected a lex error for {src}"
        );
    }
}

#[test]
fn lexes_float_literals_and_slash() {
    let tokens = lex_all("12.5 1.0 1e5 2.5e-3 / 10");
    assert_eq!(
        tokens,
        vec![
            Token::FloatLiteral(12.5),
            Token::FloatLiteral(1.0),
            Token::FloatLiteral(1e5),
            Token::FloatLiteral(2.5e-3),
            Token::Slash,
            Token::IntLiteral(10),
            Token::Newline,
            Token::Eof,
        ]
    );
}

#[test]
fn integer_without_point_stays_int() {
    // A bare trailing/leading context must not turn an int into a float.
    assert_eq!(
        lex_all("42"),
        vec![Token::IntLiteral(42), Token::Newline, Token::Eof]
    );
}

#[test]
fn newlines_suppressed_inside_parens() {
    let tokens = lex_all("var x: Int = (\n    1 +\n    2\n)");
    assert_eq!(
        tokens,
        vec![
            Token::Var,
            Token::Identifier("x".into()),
            Token::Colon,
            Token::Identifier("Int".into()),
            Token::Assign,
            Token::LParen,
            Token::IntLiteral(1),
            Token::Plus,
            Token::IntLiteral(2),
            Token::RParen,
            Token::Newline,
            Token::Eof,
        ]
    );
}

#[test]
fn backslash_newline_is_a_line_continuation() {
    // `\` immediately before a newline joins the two physical lines: no Newline
    // token appears at the join, and the continued line's indentation is ignored.
    let tokens = lex_all("var x: Int = 1 + \\\n    2");
    assert_eq!(
        tokens,
        vec![
            Token::Var,
            Token::Identifier("x".into()),
            Token::Colon,
            Token::Identifier("Int".into()),
            Token::Assign,
            Token::IntLiteral(1),
            Token::Plus,
            Token::IntLiteral(2),
            Token::Newline,
            Token::Eof,
        ]
    );
    // CRLF works too.
    assert_eq!(lex_all("1 + \\\r\n2"), lex_all("1 + 2"));
}

#[test]
fn backslash_not_before_newline_is_an_error() {
    // A backslash must be immediately followed by a newline to continue a line.
    assert!(Lexer::new("1 \\ 2").any(|r| r.is_err()));
}

#[test]
fn lexes_type_parameter_list_with_bound() {
    // `[T: Copyable & Movable]` — bracket, colon, `&`, bracket.
    let tokens = lex_all("struct Pair[T: Copyable & Movable]:");
    assert_eq!(
        tokens,
        vec![
            Token::Struct,
            Token::Identifier("Pair".into()),
            Token::LBracket,
            Token::Identifier("T".into()),
            Token::Colon,
            Token::Identifier("Copyable".into()),
            Token::Amp,
            Token::Identifier("Movable".into()),
            Token::RBracket,
            Token::Colon,
            Token::Newline,
            Token::Eof,
        ]
    );
}

#[test]
fn newlines_and_indentation_suppressed_inside_brackets() {
    // Like parentheses, a `[...]` list may span lines without NEWLINE/INDENT.
    let tokens = lex_all("Pair[\n    Int\n]");
    assert_eq!(
        tokens,
        vec![
            Token::Identifier("Pair".into()),
            Token::LBracket,
            Token::Identifier("Int".into()),
            Token::RBracket,
            Token::Newline,
            Token::Eof,
        ]
    );
}

// --- Comments (`#`) ---

#[test]
fn skips_full_line_and_inline_comments() {
    // Comment-only lines are ignored (no tokens); an inline comment ends at EOL.
    let tokens = lex_all("# header\nvar x: Int = 1  # trailing\n# footer\n");
    assert_eq!(
        tokens,
        vec![
            Token::Var,
            Token::Identifier("x".into()),
            Token::Colon,
            Token::Identifier("Int".into()),
            Token::Assign,
            Token::IntLiteral(1),
            Token::Newline,
            Token::Eof,
        ]
    );
}

#[test]
fn comment_only_lines_do_not_affect_indentation() {
    // A comment indented differently from the body must not emit Indent/Dedent.
    let tokens = lex_all("if x:\n    # note\n    pass\n# outer comment\n");
    assert_eq!(
        tokens,
        vec![
            Token::If,
            Token::Identifier("x".into()),
            Token::Colon,
            Token::Newline,
            Token::Indent,
            Token::Pass,
            Token::Newline,
            Token::Dedent,
            Token::Eof,
        ]
    );
}

#[test]
fn comment_with_unicode_is_skipped_cleanly() {
    // Multi-byte characters in a comment must not break byte indexing.
    let tokens = lex_all("var x: Int = 1  # café ☕ \u{2764}\n");
    assert!(tokens.contains(&Token::IntLiteral(1)));
    assert_eq!(tokens.last(), Some(&Token::Eof));
}

#[test]
fn comment_inside_parentheses_is_skipped() {
    let tokens = lex_all("f(\n    1,  # first\n    2,\n)\n");
    assert_eq!(
        tokens,
        vec![
            Token::Identifier("f".into()),
            Token::LParen,
            Token::IntLiteral(1),
            Token::Comma,
            Token::IntLiteral(2),
            Token::Comma,
            Token::RParen,
            Token::Newline,
            Token::Eof,
        ]
    );
}

#[test]
fn keyword_table_and_lex_helper() {
    // `Token::keyword` is the single reserved-word table; `lex` is the helper.
    assert_eq!(Token::keyword("struct"), Some(Token::Struct));
    assert_eq!(Token::keyword("comptime"), Some(Token::Comptime));
    assert_eq!(Token::keyword("with"), Some(Token::With));
    assert_eq!(Token::keyword("True"), Some(Token::BoolLiteral(true)));
    assert_eq!(Token::keyword("mut"), None); // soft word, stays an identifier
    assert_eq!(Token::keyword("point"), None);
    assert_eq!(mojo_lite::lex("var\n").unwrap()[0], Token::Var);
}

// --- Augmented-assignment operators ---

#[test]
fn lexes_augmented_assignment_operators() {
    let tokens = lex_all("+= -= *= /= //= %= **=");
    assert_eq!(
        &tokens[..7],
        &[
            Token::PlusEq,
            Token::MinusEq,
            Token::StarEq,
            Token::SlashEq,
            Token::DoubleSlashEq,
            Token::PercentEq,
            Token::DoubleStarEq,
        ]
    );
}

#[test]
fn disambiguates_aug_ops_from_longer_operators() {
    // `**=` vs `**`, `//=` vs `//`, `-=` vs `->`.
    assert_eq!(lex_all("a ** b")[1], Token::DoubleStar);
    assert_eq!(lex_all("a **= b")[1], Token::DoubleStarEq);
    assert_eq!(lex_all("a // b")[1], Token::DoubleSlash);
    assert_eq!(lex_all("a //= b")[1], Token::DoubleSlashEq);
    assert_eq!(lex_all("f() -> Int")[3], Token::Arrow);
    assert_eq!(lex_all("a -= b")[1], Token::MinusEq);
}

// --- Walrus operator ---

#[test]
fn lexes_walrus_vs_colon() {
    assert_eq!(lex_all("x := 5")[1], Token::ColonEq);
    // A plain `:` (annotation) is still a Colon.
    assert_eq!(lex_all("x: Int")[1], Token::Colon);
}

// --- Numeric literals: bases + digit separators ---

/// The single literal token in `var x = <lit>`.
fn lex_literal(lit: &str) -> Token {
    let toks = lex_all(&format!("var x = {lit}"));
    toks[3].clone()
}

#[test]
fn lexes_integer_bases() {
    assert_eq!(lex_literal("0xFF"), Token::IntLiteral(255));
    assert_eq!(lex_literal("0o77"), Token::IntLiteral(63));
    assert_eq!(lex_literal("0b0111"), Token::IntLiteral(7));
    assert_eq!(lex_literal("0x0"), Token::IntLiteral(0));
}

#[test]
fn lexes_digit_separators() {
    assert_eq!(lex_literal("1_000_000"), Token::IntLiteral(1_000_000));
    assert_eq!(lex_literal("0xFF_00"), Token::IntLiteral(0xFF00));
    assert_eq!(lex_literal("1_000.5"), Token::FloatLiteral(1000.5));
}

// --- String literals: single-quoted, triple-quoted ---

#[test]
fn lexes_single_and_triple_quoted_strings() {
    assert_eq!(lex_literal("'hello'"), Token::StringLiteral("hello".into()));
    assert_eq!(
        lex_literal("\"plain\""),
        Token::StringLiteral("plain".into())
    );
    assert_eq!(
        lex_all("var x = \"\"\"multi\nline\"\"\"")[3],
        Token::StringLiteral("multi\nline".into())
    );
    assert_eq!(
        lex_all("var x = '''a\nb'''")[3],
        Token::StringLiteral("a\nb".into())
    );
}

// --- t-strings: lexed into text/interpolation chunks ---

#[test]
fn lexes_tstring_chunks() {
    use mojo_lite::token::TStringChunk;
    assert_eq!(
        lex_literal("t\"n={n}, sum={a+b}!\""),
        Token::TString {
            chunks: vec![
                TStringChunk::Text("n=".into()),
                TStringChunk::Interp("n".into()),
                TStringChunk::Text(", sum=".into()),
                TStringChunk::Interp("a+b".into()),
                TStringChunk::Text("!".into()),
            ],
            raw: false,
        }
    );
    // Raw t-string (`rt`) sets `raw`, and `{{`/`}}` are literal braces.
    assert_eq!(
        lex_literal("rt\"a {{lit}} {x}\""),
        Token::TString {
            chunks: vec![
                TStringChunk::Text("a {lit} ".into()),
                TStringChunk::Interp("x".into()),
            ],
            raw: true,
        }
    );
    // `t` on its own is still an identifier (no adjacent quote).
    assert_eq!(lex_all("var t = 1")[1], Token::Identifier("t".into()));
}
