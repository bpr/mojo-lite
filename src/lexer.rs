use crate::error::LexError;
use crate::token::{Span, TStringChunk, Token};
use std::collections::VecDeque;

pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,

    // Byte offset at which the token currently being scanned began. Refreshed to
    // `self.pos` once per scan iteration (after the pending queue is drained), so
    // any token produced during that iteration spans `(token_start, self.pos)`.
    token_start: usize,

    // Offside rule and scoping state
    indent_stack: Vec<usize>,
    paren_count: usize,
    at_line_start: bool,
    eof_emitted: bool,

    // Queue for when a single character (or EOF) produces multiple tokens. Each
    // entry carries the token's source span (see `emit`).
    pending_tokens: VecDeque<(Token, Span)>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            token_start: 0,
            indent_stack: vec![0],
            paren_count: 0,
            at_line_start: true,
            eof_emitted: false,
            pending_tokens: VecDeque::new(),
        }
    }

    /// Helper to peek at the remaining string
    fn remainder(&self) -> &'a str {
        &self.input[self.pos..]
    }

    /// Advance to just before the next newline (or EOF), by whole characters so
    /// we never split a multi-byte UTF-8 sequence. Used to skip comment text.
    fn skip_to_line_end(&mut self) {
        while let Some(c) = self.remainder().chars().next() {
            if c == '\n' {
                break;
            }
            self.pos += c.len_utf8();
        }
    }

    /// Advance past a run of ASCII digits.
    fn consume_digits(&mut self) {
        while self
            .remainder()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            self.pos += 1;
        }
    }

    /// Advance past a run of base-`radix` digits with optional `_` digit
    /// separators (each `_` must sit between two digits, e.g. `1_000`, `0xFF_00`).
    fn consume_digit_run(&mut self, radix: u32) {
        loop {
            let mut chars = self.remainder().chars();
            match chars.next() {
                Some(c) if c.is_digit(radix) => self.pos += c.len_utf8(),
                Some('_') if chars.next().is_some_and(|d| d.is_digit(radix)) => self.pos += 1,
                _ => break,
            }
        }
    }

    /// If the remainder begins with a well-formed exponent (`e`/`E`, an optional
    /// sign, then at least one digit), consume it and return `true`. A lone `e`
    /// not followed by digits is left untouched (it is not part of the number).
    fn consume_exponent(&mut self) -> bool {
        let rem = self.remainder();
        if !(rem.starts_with('e') || rem.starts_with('E')) {
            return false;
        }
        let after_e = &rem[1..];
        let sign_len = match after_e.chars().next() {
            Some('+') | Some('-') => 1,
            _ => 0,
        };
        if after_e[sign_len..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            self.pos += 1 + sign_len; // consume 'e' and the optional sign
            self.consume_digits();
            true
        } else {
            false
        }
    }

    /// Lex a string literal delimited by `quote` (`"` or `'`), either single- or
    /// `triple`-quoted. `self.pos` must point at the opening delimiter. Escapes are
    /// the full Mojo set (see `decode_escape`); a triple-quoted string may span
    /// newlines.
    fn lex_string(&mut self, quote: char, triple: bool) -> Result<Token, LexError> {
        let start = self.pos;
        let delim: String = quote.to_string().repeat(if triple { 3 } else { 1 });
        self.pos += delim.len(); // consume the opening delimiter (quotes are ASCII)
        let mut value = String::new();

        loop {
            let rem = self.remainder();
            if rem.starts_with(&delim) {
                self.pos += delim.len(); // consume the closing delimiter
                return Ok(Token::StringLiteral(value));
            }
            let c = match rem.chars().next() {
                Some(c) => c,
                None => return Err(LexError::UnterminatedString(start)),
            };
            match c {
                // A single-line string cannot contain a raw newline.
                '\n' if !triple => return Err(LexError::UnterminatedString(start)),
                '\\' => {
                    self.pos += 1; // consume the backslash
                    value.push(self.decode_escape(start)?);
                }
                _ => {
                    value.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    /// Decode a backslash escape sequence. `self.pos` must point at the character
    /// immediately after the `\`; this advances `self.pos` past the whole escape
    /// body and returns the decoded character. `start` is the string's start offset
    /// (for the unterminated-string error). Supports the simple escapes
    /// `\a \b \f \n \r \t \v \\ \' \"` and the numeric escapes — octal `\ooo`
    /// (leading digit 0–3, then two octal digits), `\xHH`, `\uHHHH`, `\UHHHHHHHH`
    /// — each a Unicode scalar value, encoded UTF-8 into the string.
    fn decode_escape(&mut self, start: usize) -> Result<char, LexError> {
        let esc = match self.remainder().chars().next() {
            Some(e) => e,
            None => return Err(LexError::UnterminatedString(start)),
        };
        // Simple single-character escapes (the escape letter is one byte here).
        let simple = match esc {
            '"' => Some('"'),
            '\'' => Some('\''),
            '\\' => Some('\\'),
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            'a' => Some('\u{07}'), // bell
            'b' => Some('\u{08}'), // backspace
            'f' => Some('\u{0C}'), // form feed
            'v' => Some('\u{0B}'), // vertical tab
            _ => None,
        };
        if let Some(ch) = simple {
            self.pos += esc.len_utf8();
            return Ok(ch);
        }
        // Numeric escapes decode to a code point, which must be a Unicode scalar.
        let code = match esc {
            'x' => {
                self.pos += 1; // consume 'x'
                self.read_hex_digits(2, start)?
            }
            'u' => {
                self.pos += 1; // consume 'u'
                self.read_hex_digits(4, start)?
            }
            'U' => {
                self.pos += 1; // consume 'U'
                self.read_hex_digits(8, start)?
            }
            // Octal: the leading digit (0–3) is part of the value; read 3 in all.
            '0'..='3' => self.read_octal_digits(start)?,
            other => return Err(LexError::InvalidEscape(other, self.pos)),
        };
        char::from_u32(code).ok_or(LexError::InvalidEscape(esc, self.pos))
    }

    /// Read exactly `n` hex digits (for `\x`/`\u`/`\U`) into a code point. A
    /// missing or non-hex digit is an error. `self.pos` is at the first digit.
    fn read_hex_digits(&mut self, n: usize, start: usize) -> Result<u32, LexError> {
        let mut code = 0u32;
        for _ in 0..n {
            let ch = match self.remainder().chars().next() {
                Some(c) if c.is_ascii_hexdigit() => c,
                Some(c) => return Err(LexError::InvalidEscape(c, self.pos)),
                None => return Err(LexError::UnterminatedString(start)),
            };
            code = code * 16 + ch.to_digit(16).unwrap();
            self.pos += 1;
        }
        Ok(code)
    }

    /// Read exactly 3 octal digits into a byte value (the leading digit, already
    /// known to be 0–3, is the first). `self.pos` is at that leading digit.
    fn read_octal_digits(&mut self, start: usize) -> Result<u32, LexError> {
        let mut code = 0u32;
        for _ in 0..3 {
            let ch = match self.remainder().chars().next() {
                Some(c) if c.is_digit(8) => c,
                Some(c) => return Err(LexError::InvalidEscape(c, self.pos)),
                None => return Err(LexError::UnterminatedString(start)),
            };
            code = code * 8 + ch.to_digit(8).unwrap();
            self.pos += 1;
        }
        Ok(code)
    }

    /// Lex a t-string body (the prefix `t`/`rt` has been scanned; `self.pos` is at
    /// the opening `quote`). Splits into literal `Text` chunks and `Interp` chunks
    /// holding each `{…}` interpolation's raw source (the parser parses those into
    /// expressions). `{{`/`}}` are literal braces; a raw (`rt`) t-string does not
    /// expand escapes. Brace matching for an interpolation is naive (it does not
    /// look inside nested string literals).
    fn lex_tstring(&mut self, quote: char, triple: bool, raw: bool) -> Result<Token, LexError> {
        let start = self.pos;
        let delim: String = quote.to_string().repeat(if triple { 3 } else { 1 });
        self.pos += delim.len(); // consume the opening delimiter
        let mut chunks: Vec<TStringChunk> = Vec::new();
        let mut text = String::new();

        loop {
            let rem = self.remainder();
            if rem.starts_with(&delim) {
                self.pos += delim.len();
                if !text.is_empty() {
                    chunks.push(TStringChunk::Text(text));
                }
                return Ok(Token::TString { chunks, raw });
            }
            let c = match rem.chars().next() {
                Some(c) => c,
                None => return Err(LexError::UnterminatedString(start)),
            };
            match c {
                '{' if rem.starts_with("{{") => {
                    text.push('{');
                    self.pos += 2;
                }
                '{' => {
                    if !text.is_empty() {
                        chunks.push(TStringChunk::Text(std::mem::take(&mut text)));
                    }
                    self.pos += 1; // consume '{'
                    let expr_start = self.pos;
                    let mut depth = 1usize;
                    loop {
                        let ch = match self.remainder().chars().next() {
                            Some(c) => c,
                            None => return Err(LexError::UnterminatedString(start)),
                        };
                        match ch {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        self.pos += ch.len_utf8();
                    }
                    let expr_text = self.input[expr_start..self.pos].to_string();
                    self.pos += 1; // consume '}'
                    chunks.push(TStringChunk::Interp(expr_text));
                }
                '}' if rem.starts_with("}}") => {
                    text.push('}');
                    self.pos += 2;
                }
                '\n' if !triple => return Err(LexError::UnterminatedString(start)),
                '\\' if !raw => {
                    self.pos += 1; // consume the backslash
                    text.push(self.decode_escape(start)?);
                }
                _ => {
                    text.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    /// Enqueue a scanned token, stamping it with the span `(token_start, pos)` —
    /// the source range consumed since this scan iteration began. Synthetic layout
    /// tokens (Indent/Dedent/Newline/Eof) get whatever narrow range is current,
    /// which is fine since they carry no source text.
    fn emit(&mut self, token: Token) {
        self.pending_tokens
            .push_back((token, (self.token_start, self.pos)));
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<(Token, Span), LexError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // 1. Drain any pending tokens first (e.g., multiple Dedents)
            if let Some(spanned) = self.pending_tokens.pop_front() {
                return Some(Ok(spanned));
            }

            // Refresh the token-start mark: any token scanned this iteration spans
            // from here to wherever `self.pos` lands. Re-running per iteration means
            // leading whitespace (which advances `pos` and loops) is excluded.
            self.token_start = self.pos;

            // 2. Stop completely if we've already emitted the EOF token
            if self.eof_emitted {
                return None;
            }

            // 3. Handle End of File
            if self.pos >= self.input.len() {
                // If the file didn't end with a newline but had tokens, emit one
                if !self.at_line_start {
                    self.emit(Token::Newline);
                }

                // Unwind the indentation stack
                while self.indent_stack.len() > 1 {
                    self.indent_stack.pop();
                    self.emit(Token::Dedent);
                }

                self.emit(Token::Eof);
                self.eof_emitted = true;
                continue; // Loop around to pop the tokens we just enqueued
            }

            // 4. Handle indentation at the start of a logical line
            if self.at_line_start {
                let mut spaces = 0;
                let mut temp_pos = self.pos;
                let mut is_blank_line = false;

                // Count leading spaces. A line that is empty or holds only a
                // comment (`#...` after optional spaces) is "blank": it must not
                // affect indentation (no Indent/Dedent/Newline).
                for c in self.remainder().chars() {
                    if c == ' ' {
                        spaces += 1;
                        temp_pos += c.len_utf8();
                    } else if c == '\n' || c == '\r' {
                        is_blank_line = true;
                        break;
                    } else {
                        is_blank_line = c == '#';
                        break;
                    }
                }

                if is_blank_line {
                    // Skip the rest of the line (any comment text) and its newline.
                    self.pos = temp_pos;
                    self.skip_to_line_end();
                    if self.remainder().starts_with('\n') {
                        self.pos += 1;
                    }
                    self.at_line_start = true;
                    continue;
                }

                self.pos = temp_pos;
                self.at_line_start = false;

                // Only evaluate indentation if we are NOT inside parentheses
                if self.paren_count == 0 {
                    let current_indent = *self.indent_stack.last().unwrap();

                    if spaces > current_indent {
                        self.indent_stack.push(spaces);
                        self.emit(Token::Indent);
                        continue;
                    } else if spaces < current_indent {
                        while let Some(&top) = self.indent_stack.last() {
                            if top > spaces {
                                self.indent_stack.pop();
                                self.emit(Token::Dedent);
                            } else if top == spaces {
                                break;
                            } else {
                                return Some(Err(LexError::IndentationError(self.pos)));
                            }
                        }
                        if !self.pending_tokens.is_empty() {
                            continue;
                        }
                    }
                }
            }

            // 5. Consume characters
            let c = self.remainder().chars().next().unwrap();

            match c {
                ' ' | '\t' | '\r' => {
                    // Inline whitespace is ignored
                    self.pos += c.len_utf8();
                }
                '#' => {
                    // A comment runs to the end of the line; the newline (if any)
                    // is handled on the next iteration, so `x = 1  # note` still
                    // ends the logical line, and a `#` inside `( … )` is skipped.
                    self.skip_to_line_end();
                }
                '\n' => {
                    self.pos += 1;
                    if self.paren_count == 0 {
                        self.at_line_start = true;
                        self.emit(Token::Newline);
                        continue;
                    }
                }
                '\\' => {
                    // Explicit line continuation: a backslash *immediately* followed
                    // by a newline (LF or CRLF) joins the two physical lines into one
                    // logical line — the newline is suppressed and the continued
                    // line's indentation is not significant (`at_line_start` stays
                    // false). A backslash not followed by a newline is an error.
                    let after = self.pos + 1; // byte offset just past the '\'
                    if self.input[after..].starts_with("\r\n") {
                        self.pos = after + 2;
                    } else if self.input[after..].starts_with('\n') {
                        self.pos = after + 1;
                    } else {
                        return Some(Err(LexError::UnexpectedCharacter('\\', self.pos)));
                    }
                }
                '(' => {
                    self.pos += 1;
                    self.paren_count += 1;
                    self.emit(Token::LParen);
                    continue;
                }
                ')' => {
                    self.pos += 1;
                    if self.paren_count > 0 {
                        self.paren_count -= 1;
                    } else {
                        return Some(Err(LexError::UnmatchedParenthesis(self.pos)));
                    }
                    self.emit(Token::RParen);
                    continue;
                }
                '[' => {
                    // Brackets nest like parentheses for the offside rule, so a
                    // type-parameter / type-argument list may span lines.
                    self.pos += 1;
                    self.paren_count += 1;
                    self.emit(Token::LBracket);
                    continue;
                }
                ']' => {
                    self.pos += 1;
                    if self.paren_count > 0 {
                        self.paren_count -= 1;
                    } else {
                        return Some(Err(LexError::UnmatchedParenthesis(self.pos)));
                    }
                    self.emit(Token::RBracket);
                    continue;
                }
                '&' => {
                    self.pos += 1;
                    self.emit(Token::Amp);
                    continue;
                }
                '^' => {
                    self.pos += 1;
                    self.emit(Token::Caret);
                    continue;
                }
                ':' => {
                    // `:=` (walrus) vs `:` (annotation / block header)
                    if self.remainder().starts_with(":=") {
                        self.pos += 2;
                        self.emit(Token::ColonEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Colon);
                    }
                    continue;
                }
                ',' => {
                    self.pos += 1;
                    self.emit(Token::Comma);
                    continue;
                }
                ';' => {
                    self.pos += 1;
                    self.emit(Token::Semicolon);
                    continue;
                }
                '.' => {
                    // `...` is the ellipsis (a trait-method requirement); a `.`
                    // adjacent to digits is consumed by number scanning; an
                    // otherwise standalone `.` is member access.
                    if self.remainder().starts_with("...") {
                        self.pos += 3;
                        self.emit(Token::Ellipsis);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Dot);
                    }
                    continue;
                }
                '@' => {
                    self.pos += 1;
                    self.emit(Token::At);
                    continue;
                }
                '=' => {
                    // `==` (equality) vs `=` (assignment)
                    if self.remainder().starts_with("==") {
                        self.pos += 2;
                        self.emit(Token::EqEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Assign);
                    }
                    continue;
                }
                '-' => {
                    // `->` (return arrow), `-=` (augmented) vs `-` (sub / negation)
                    if self.remainder().starts_with("->") {
                        self.pos += 2;
                        self.emit(Token::Arrow);
                    } else if self.remainder().starts_with("-=") {
                        self.pos += 2;
                        self.emit(Token::MinusEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Minus);
                    }
                    continue;
                }
                '+' => {
                    if self.remainder().starts_with("+=") {
                        self.pos += 2;
                        self.emit(Token::PlusEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Plus);
                    }
                    continue;
                }
                '*' => {
                    // Longest match first: `**=`, then `**`, then `*=`, then `*`.
                    if self.remainder().starts_with("**=") {
                        self.pos += 3;
                        self.emit(Token::DoubleStarEq);
                    } else if self.remainder().starts_with("**") {
                        self.pos += 2;
                        self.emit(Token::DoubleStar);
                    } else if self.remainder().starts_with("*=") {
                        self.pos += 2;
                        self.emit(Token::StarEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Star);
                    }
                    continue;
                }
                '/' => {
                    // Longest match first: `//=`, then `//`, then `/=`, then `/`.
                    if self.remainder().starts_with("//=") {
                        self.pos += 3;
                        self.emit(Token::DoubleSlashEq);
                    } else if self.remainder().starts_with("//") {
                        self.pos += 2;
                        self.emit(Token::DoubleSlash);
                    } else if self.remainder().starts_with("/=") {
                        self.pos += 2;
                        self.emit(Token::SlashEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Slash);
                    }
                    continue;
                }
                '%' => {
                    if self.remainder().starts_with("%=") {
                        self.pos += 2;
                        self.emit(Token::PercentEq);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Percent);
                    }
                    continue;
                }
                '<' => {
                    if self.remainder().starts_with("<=") {
                        self.pos += 2;
                        self.emit(Token::Le);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Lt);
                    }
                    continue;
                }
                '>' => {
                    if self.remainder().starts_with(">=") {
                        self.pos += 2;
                        self.emit(Token::Ge);
                    } else {
                        self.pos += 1;
                        self.emit(Token::Gt);
                    }
                    continue;
                }
                '!' => {
                    // `!` only appears as part of `!=` in this subset.
                    if self.remainder().starts_with("!=") {
                        self.pos += 2;
                        self.emit(Token::NotEq);
                        continue;
                    }
                    return Some(Err(LexError::UnexpectedCharacter(c, self.pos)));
                }
                '"' | '\'' => {
                    // Triple-quoted (`"""` / `'''`) if the next three chars match.
                    let triple = self.remainder().starts_with(&c.to_string().repeat(3));
                    match self.lex_string(c, triple) {
                        Ok(token) => {
                            self.emit(token);
                            continue;
                        }
                        Err(err) => return Some(Err(err)),
                    }
                }
                _ if c.is_ascii_alphabetic() || c == '_' => {
                    let start = self.pos;
                    while self.pos < self.input.len() {
                        let next_c = self.remainder().chars().next().unwrap();
                        if next_c.is_ascii_alphanumeric() || next_c == '_' {
                            self.pos += next_c.len_utf8();
                        } else {
                            break;
                        }
                    }

                    let text = self.input[start..self.pos].to_string();
                    // A `t"…"` / `rt"…"` string prefix: the word is exactly `t`/`rt`
                    // and is immediately followed by a quote (no space).
                    let tprefix = match text.as_str() {
                        "t" => Some(false),
                        "rt" => Some(true),
                        _ => None,
                    };
                    if let Some(raw) = tprefix
                        && let Some(quote) = self
                            .remainder()
                            .chars()
                            .next()
                            .filter(|&q| q == '"' || q == '\'')
                    {
                        let triple = self.remainder().starts_with(&quote.to_string().repeat(3));
                        match self.lex_tstring(quote, triple, raw) {
                            Ok(token) => {
                                self.emit(token);
                                continue;
                            }
                            Err(err) => return Some(Err(err)),
                        }
                    }
                    let token = Token::keyword(&text).unwrap_or(Token::Identifier(text));
                    self.emit(token);
                    continue;
                }
                _ if c.is_ascii_digit() => {
                    let start = self.pos;

                    // A based integer literal: `0x…` (hex), `0o…` (octal), `0b…`
                    // (binary), with optional `_` digit separators.
                    let radix = if c == '0' {
                        match self.remainder()[1..].chars().next() {
                            Some('x') | Some('X') => Some(16),
                            Some('o') | Some('O') => Some(8),
                            Some('b') | Some('B') => Some(2),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some(radix) = radix {
                        self.pos += 2; // consume the `0x` / `0o` / `0b` prefix
                        let digits_start = self.pos;
                        self.consume_digit_run(radix);
                        let cleaned: String = self.input[digits_start..self.pos]
                            .chars()
                            .filter(|&c| c != '_')
                            .collect();
                        match i64::from_str_radix(&cleaned, radix) {
                            Ok(num) => self.emit(Token::IntLiteral(num)),
                            Err(_) => return Some(Err(LexError::InvalidInteger(start))),
                        }
                        continue;
                    }

                    self.consume_digit_run(10);

                    // A `.` followed by a digit, or an `e`/`E` exponent, makes this
                    // a float; otherwise the same text is an integer. (A bare
                    // trailing `.` is left for a future member-access `.`.)
                    let mut is_float = false;
                    let rem = self.remainder();
                    if rem.starts_with('.')
                        && rem[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
                    {
                        is_float = true;
                        self.pos += 1; // consume '.'
                        self.consume_digit_run(10);
                    }
                    if self.consume_exponent() {
                        is_float = true;
                    }

                    // Strip `_` separators before parsing (Rust's parsers reject them).
                    let cleaned: String = self.input[start..self.pos]
                        .chars()
                        .filter(|&c| c != '_')
                        .collect();
                    if is_float {
                        match cleaned.parse::<f64>() {
                            Ok(num) => self.emit(Token::FloatLiteral(num)),
                            Err(_) => return Some(Err(LexError::InvalidFloat(start))),
                        }
                    } else if let Ok(num) = cleaned.parse::<i64>() {
                        self.emit(Token::IntLiteral(num));
                    } else {
                        return Some(Err(LexError::InvalidInteger(start)));
                    }
                    continue;
                }
                _ => {
                    return Some(Err(LexError::UnexpectedCharacter(c, self.pos)));
                }
            }
        }
    }
}
