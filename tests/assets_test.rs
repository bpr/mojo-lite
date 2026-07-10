//! File-based fixtures: each `.mojo` file under `assets/<category>/` is run through
//! the whole pipeline and asserted to land at the outcome its folder names. Drop a
//! real Mojo file into the matching folder to add coverage — no code changes.
//!
//! Categories: `ok` (lex+parse+check+**ownership**+eval all succeed), `parse_error`
//! (rejected at lex/parse), `type_error` (checker rejects), `ownership_error` (the
//! Phase-4 move analysis rejects — use-after-move / conditional move), and
//! `runtime_error` (fails at eval, including the `Unsupported` gaps).
//!
//! A file may optionally pin the exact message with a top comment
//! `# expect: <substring>` (valid Mojo, skipped by the lexer): the reported error
//! must then contain `<substring>`.

use mojito::{BackendKind, check, check_ownership};
use std::fs;
use std::path::{Path, PathBuf};

/// The pipeline stage at which a program is first rejected (or `Ok`).
#[derive(Debug, PartialEq, Clone, Copy)]
enum Outcome {
    Ok,
    ParseError,
    TypeError,
    OwnershipError,
    RuntimeError,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::ParseError => "parse_error",
            Outcome::TypeError => "type_error",
            Outcome::OwnershipError => "ownership_error",
            Outcome::RuntimeError => "runtime_error",
        }
    }
}

/// Run the full pipeline, returning where it first fails and the message.
fn classify(path: &Path) -> (Outcome, String) {
    let program = match mojito::link(path) {
        Ok(p) => p,
        Err(mojito::ModuleError::Parse { err, .. }) => {
            return (Outcome::ParseError, err.to_string());
        }
        Err(e) => return (Outcome::TypeError, e.to_string()),
    };
    // Compile-time elaboration (resolve `comptime if`/`comptime for`) — its errors
    // are compile-time rejections, classified with the type-error stage.
    let program = match mojito::elaborate(program) {
        Ok(p) => p,
        Err(e) => return (Outcome::TypeError, e.to_string()),
    };
    if let Err(e) = check(&program) {
        return (Outcome::TypeError, e.to_string());
    }
    if let Err(e) = check_ownership(&program) {
        return (Outcome::OwnershipError, e.to_string());
    }
    let mut backend = BackendKind::Vm.make();
    match backend.run(&program) {
        Ok(()) => (Outcome::Ok, String::new()),
        Err(e) => (Outcome::RuntimeError, e.to_string()),
    }
}

/// The `# expect: <substring>` directive (if any), pinning the error message.
fn expected_substring(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix('#')?
            .trim_start()
            .strip_prefix("expect:")
            .map(|s| s.trim().to_string())
    })
}

/// The `.mojo` files in `assets/<category>/`, sorted (empty if the dir is absent).
fn fixtures(category: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(category);
    let mut files: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "mojo"))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

/// Assert every fixture in a category lands at `expected`, reporting all
/// mismatches (and any unmet `# expect:` pin) at once.
fn check_category(category: &str, expected: Outcome) {
    let mut failures = Vec::new();
    for path in fixtures(category) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = fs::read_to_string(&path).expect("read fixture");
        let (got, message) = classify(&path);
        let shown = if message.is_empty() {
            "no error"
        } else {
            message.as_str()
        };
        if got != expected {
            failures.push(format!(
                "  {name}: expected {}, got {} ({shown})",
                expected.label(),
                got.label(),
            ));
        } else if let Some(sub) = expected_substring(&source)
            && !message.contains(&sub)
        {
            failures.push(format!(
                "  {name}: error did not contain '{sub}' (got: {shown})"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "\n{} fixture(s) in assets/{category}/ did not match:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

#[test]
fn assets_ok() {
    check_category("ok", Outcome::Ok);
}

#[test]
fn assets_parse_error() {
    check_category("parse_error", Outcome::ParseError);
}

#[test]
fn assets_type_error() {
    check_category("type_error", Outcome::TypeError);
}

#[test]
fn assets_ownership_error() {
    check_category("ownership_error", Outcome::OwnershipError);
}

#[test]
fn assets_runtime_error() {
    check_category("runtime_error", Outcome::RuntimeError);
}
