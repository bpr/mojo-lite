use mojito::{BackendKind, ModuleError, ParseError, Stmt, check, lex, parse};
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

/// Obtain the program to check/run: when a real file path is given, **link** it
/// with its imported modules (`from module import …`); for stdin (or `-`), parse
/// the source alone (imports left unresolved — there is no base directory). Either
/// way, **compile-time elaboration** resolves `comptime if`/`comptime for` before
/// the program is handed to the checker.
fn load_program(file: Option<&str>) -> Result<Vec<Stmt>, String> {
    let program = match file {
        Some(path) if path != "-" => {
            let source = read_source(file).map_err(|e| format!("cannot read input: {e}"))?;
            mojito::link_source(&source, Path::new(path))
                .map_err(|e| format_module_error(&e, path, &source))?
        }
        _ => {
            let source = read_source(file).map_err(|e| format!("cannot read input: {e}"))?;
            parse(&source).map_err(|e| format_parse_error(file.unwrap_or("-"), &source, &e))?
        }
    };
    mojito::elaborate(program).map_err(|e| e.to_string())
}

/// mojito doubles as a small **syntax-analysis tool**. With no arguments it
/// runs the built-in demo; otherwise the first argument selects a pipeline stage
/// to run over a file (or stdin), so you can inspect the tokens or the AST:
///
/// ```text
/// mojito lex   [FILE]   # the token stream, one per line
/// mojito parse [FILE]   # the parsed AST (pretty-printed)
/// mojito check [FILE]   # lex + parse + type-check; report ok / the error
/// mojito run   [FILE]   # the full pipeline; print output + final bindings
/// mojito demo           # the built-in showcase (also the no-arg default)
/// ```
///
/// A `FILE` of `-`, or its absence, reads from standard input.
fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // Extract an optional `--backend=NAME` from anywhere; the register VM is the
    // sole/default executor.
    let mut backend = BackendKind::Vm;
    let mut args: Vec<String> = Vec::new();
    for a in raw {
        if let Some(name) = a.strip_prefix("--backend=") {
            match BackendKind::parse(name) {
                Ok(b) => backend = b,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            args.push(a);
        }
    }

    let (command, file) = (
        args.first().map(String::as_str),
        args.get(1).map(String::as_str),
    );
    match command {
        None => ExitCode::SUCCESS,
        Some("lex") => stage("lex", file, run_lex),
        Some("parse") => stage_parse(file),
        Some("check") => program_stage("check", file, run_check),
        Some("own") => program_stage("own", file, run_own),
        Some("run") => stage_run(file, backend), // ← now backend-aware
        Some("-h" | "--help" | "help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn format_module_error(err: &ModuleError, entry_path: &str, entry_source: &str) -> String {
    match err {
        ModuleError::Parse { module, err } => {
            if module == entry_path {
                format_parse_error(module, entry_source, err)
            } else {
                match std::fs::read_to_string(module) {
                    Ok(source) => format_parse_error(module, &source, err),
                    Err(_) => format!("in module '{module}': {err}"),
                }
            }
        }
        _ => err.to_string(),
    }
}

/// Run a stage that operates on the **linked program** (so `from module import …`
/// is resolved when a file path is given). Used by `check`/`own`.
fn program_stage(name: &str, file: Option<&str>, f: fn(&[Stmt]) -> Result<(), String>) -> ExitCode {
    let program = match load_program(file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{name} error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match f(&program) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{name} error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `run`, routed through the selected backend (over the linked program).
fn stage_run(file: Option<&str>, backend: BackendKind) -> ExitCode {
    match run_program(file, backend) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_program(file: Option<&str>, backend: BackendKind) -> Result<(), String> {
    let program = load_program(file)?;
    check(&program).map_err(|e| e.to_string())?;
    // Ownership (move) analysis is a real compile stage: reject use-after-move.
    mojito::check_ownership(&program).map_err(|e| e.to_string())?;
    let mut backend = backend.make(); // Box<dyn Backend>
    backend.run(&program).map_err(|e| e.to_string())?;
    let output = backend.output();
    if !output.is_empty() {
        print!("{output}");
    }
    for (n, v) in backend.bindings() {
        println!("{n} = {v}");
    }
    Ok(())
}

fn print_usage() {
    eprint!(
        "mojito — a lexer/parser/checker/evaluator for a subset of Mojo\n\n\
         usage: mojito [COMMAND] [FILE]\n\n\
         commands:\n\
         \x20 lex   [FILE]   print the token stream (one per line)\n\
         \x20 parse [FILE]   print the parsed AST\n\
         \x20 check [FILE]   type-check and report ok or the first error\n\
         \x20 run   [FILE]   evaluate and print output + final bindings\n\
         \x20 demo           run the built-in showcase (default)\n\n\
         FILE defaults to '-' (standard input).\n"
    );
}

/// Read the named file, or standard input when it is absent or `-`.
fn read_source(file: Option<&str>) -> std::io::Result<String> {
    match file {
        None | Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
        Some(path) => std::fs::read_to_string(path),
    }
}

/// Run one stage over the source, turning any I/O or stage error into a non-zero
/// exit code with a message on stderr.
fn stage(name: &str, file: Option<&str>, f: fn(&str) -> Result<(), String>) -> ExitCode {
    let source = match read_source(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: cannot read input: {}", name, e);
            return ExitCode::FAILURE;
        }
    };
    match f(&source) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} error: {}", name, e);
            ExitCode::FAILURE
        }
    }
}

fn stage_parse(file: Option<&str>) -> ExitCode {
    let source = match read_source(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("parse: cannot read input: {}", e);
            return ExitCode::FAILURE;
        }
    };
    match parse(&source) {
        Ok(program) => {
            println!("{:#?}", program);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "parse error: {}",
                format_parse_error(file.unwrap_or("-"), &source, &e)
            );
            ExitCode::FAILURE
        }
    }
}

fn format_parse_error(label: &str, source: &str, err: &ParseError) -> String {
    let Some(byte) = err.byte_pos() else {
        return err.to_string();
    };
    format_source_error(label, source, byte, &err.to_string())
}

fn format_source_error(label: &str, source: &str, byte: usize, message: &str) -> String {
    let byte = byte.min(source.len());
    let mut line_no = 1usize;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= byte {
            break;
        }
        if ch == '\n' {
            line_no += 1;
            line_start = idx + 1;
        }
    }
    let line_end = source[line_start..]
        .find('\n')
        .map(|n| line_start + n)
        .unwrap_or(source.len());
    let line = source[line_start..line_end].trim_end_matches('\r');
    let col = source[line_start..byte].chars().count() + 1;
    let caret = format!("{}^", " ".repeat(col.saturating_sub(1)));
    format!("{label}:{line_no}:{col}: {message}\n{line}\n{caret}")
}

/// `lex`: print every token, one per line.
fn run_lex(source: &str) -> Result<(), String> {
    let tokens = lex(source).map_err(|e| e.to_string())?;
    for tok in tokens {
        println!("{:?}", tok);
    }
    Ok(())
}

/// `check`: type-check the linked program; report success or the first error.
fn run_check(program: &[Stmt]) -> Result<(), String> {
    check(program).map_err(|e| e.to_string())?;
    // The ownership analysis is part of a full check.
    mojito::check_ownership(program).map_err(|e| e.to_string())?;
    println!("ok");
    Ok(())
}

/// `own` — type-check, then run the ownership (move) analysis. Reports `ok`, or the
/// first move violation with its source byte range.
fn run_own(program: &[Stmt]) -> Result<(), String> {
    check(program).map_err(|e| e.to_string())?;
    match mojito::check_ownership(program) {
        Ok(()) => {
            println!("ok");
            Ok(())
        }
        Err(e) => {
            let (start, end) = e.span();
            Err(format!("{e} (bytes {start}..{end})"))
        }
    }
}
