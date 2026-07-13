use mojito::{
    BackendKind, Compiler, CompilerError, LinkOptions, ModuleError, ParseError, Stmt, check, lex,
    parse, parse_diagnostics,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Obtain the program to check/run: when a real file path is given, **link** it
/// with its imported modules (`from module import …`); for stdin (or `-`), parse
/// the source alone (imports left unresolved — there is no base directory). Either
/// way, **compile-time elaboration** resolves `comptime if`/`comptime for` before
/// the program is handed to the checker.
fn load_program(file: Option<&str>, link_options: &LinkOptions) -> Result<Vec<Stmt>, String> {
    let program = match file {
        Some(path) if path != "-" => {
            let source = read_source(file).map_err(|e| format!("cannot read input: {e}"))?;
            mojito::link_source_with_options(&source, Path::new(path), link_options.clone())
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
    let cli = match parse_cli_args(raw) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let (command, file) = (
        cli.args.first().map(String::as_str),
        cli.args.get(1).map(String::as_str),
    );
    match command {
        None => ExitCode::SUCCESS,
        Some("lex") => stage("lex", file, run_lex),
        Some("parse") => stage_parse(file),
        Some("check") => program_stage("check", file, &cli.link_options, run_check),
        Some("own") => program_stage("own", file, &cli.link_options, run_own),
        Some("run") => stage_run(file, cli.backend, &cli.link_options),
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

struct CliArgs {
    backend: BackendKind,
    args: Vec<String>,
    link_options: LinkOptions,
}

/// Extract global options from anywhere on the command line. Local imports win,
/// then CLI roots in occurrence order, then the bundled stdlib fallback.
fn parse_cli_args(raw: Vec<String>) -> Result<CliArgs, String> {
    let mut backend = BackendKind::Vm;
    let mut args = Vec::new();
    let mut roots = Vec::<PathBuf>::new();
    let mut iter = raw.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--backend=") {
            backend = BackendKind::parse(name)?;
        } else if arg == "--backend" {
            let name = iter.next().ok_or("--backend requires a name")?;
            backend = BackendKind::parse(&name)?;
        } else if let Some(path) = arg.strip_prefix("--module-path=") {
            require_path("--module-path", path, &mut roots)?;
        } else if arg == "--module-path" || arg == "-I" {
            let path = iter
                .next()
                .ok_or_else(|| format!("{arg} requires a path"))?;
            require_path(&arg, &path, &mut roots)?;
        } else if let Some(path) = arg.strip_prefix("--stdlib=") {
            require_path("--stdlib", path, &mut roots)?;
        } else if arg == "--stdlib" {
            let path = iter.next().ok_or("--stdlib requires a path")?;
            require_path("--stdlib", &path, &mut roots)?;
        } else if arg.starts_with('-') && arg != "-" && !matches!(arg.as_str(), "-h" | "--help") {
            return Err(format!("unknown option '{arg}'"));
        } else {
            args.push(arg);
        }
    }
    roots.extend(LinkOptions::default().search_roots);
    Ok(CliArgs {
        backend,
        args,
        link_options: LinkOptions {
            search_roots: roots,
        },
    })
}

fn require_path(option: &str, path: &str, roots: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.is_empty() {
        Err(format!("{option} requires a non-empty path"))
    } else {
        roots.push(PathBuf::from(path));
        Ok(())
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
fn program_stage(
    name: &str,
    file: Option<&str>,
    link_options: &LinkOptions,
    f: fn(&[Stmt]) -> Result<(), String>,
) -> ExitCode {
    let program = match load_program(file, link_options) {
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
fn stage_run(file: Option<&str>, backend: BackendKind, link_options: &LinkOptions) -> ExitCode {
    match run_program(file, backend, link_options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_program(
    file: Option<&str>,
    backend: BackendKind,
    link_options: &LinkOptions,
) -> Result<(), String> {
    let source = read_source(file).map_err(|e| format!("cannot read input: {e}"))?;
    let compiler = Compiler::new(link_options.clone(), backend);
    let compiled = match file {
        Some(path) if path != "-" => compiler.compile_source(&source, Path::new(path)),
        _ => compiler.compile_unlinked(&source),
    }
    .map_err(|error| match &error {
        CompilerError::Module(module) => format_module_error(module, file.unwrap_or("-"), &source),
        _ => error.to_string(),
    })?;
    let execution = compiler
        .execute(&compiled)
        .map_err(|error| error.to_string())?;
    if !execution.output.is_empty() {
        print!("{}", execution.output);
    }
    for (n, v) in execution.bindings {
        println!("{n} = {v}");
    }
    Ok(())
}

fn print_usage() {
    eprint!(
        "mojito — a compiler and register VM for a subset of Mojo\n\n\
         usage: mojito [COMMAND] [FILE]\n\n\
         global options:\n\
         \x20 -I, --module-path PATH  add a module search root (repeatable)\n\
         \x20 --stdlib PATH          add a stdlib search root (repeatable)\n\
         \x20 --backend NAME         select the run backend\n\n\
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
    let report = parse_diagnostics(&source, 20);
    if report.errors.is_empty() {
        println!("{:#?}", report.program);
        ExitCode::SUCCESS
    } else {
        for e in &report.errors {
            eprintln!(
                "parse error: {}",
                format_parse_error(file.unwrap_or("-"), &source, e)
            );
        }
        if report.truncated {
            eprintln!("parse error: stopped after 20 diagnostics");
        }
        ExitCode::FAILURE
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

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn module_roots_preserve_cli_order_before_bundled_stdlib() {
        let cli = parse_cli_args(vec![
            "check".into(),
            "main.mojo".into(),
            "--module-path".into(),
            "first".into(),
            "-I".into(),
            "second".into(),
            "--stdlib=third".into(),
        ])
        .unwrap();
        assert_eq!(
            &cli.link_options.search_roots[..3],
            &[
                PathBuf::from("first"),
                PathBuf::from("second"),
                PathBuf::from("third")
            ]
        );
    }

    #[test]
    fn module_root_options_require_paths() {
        assert!(parse_cli_args(vec!["check".into(), "--module-path".into()]).is_err());
        assert!(parse_cli_args(vec!["check".into(), "--stdlib=".into()]).is_err());
    }
}
