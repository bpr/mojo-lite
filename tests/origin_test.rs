//! End-to-end conformance fixtures for checked origins and executable references.

use mojito::{BackendKind, check_ownership, check_program, parse};
use std::fs;
use std::path::{Path, PathBuf};

fn fixtures(category: &str) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join(category),
    )
    .expect("origin fixture directory exists")
    .map(|entry| entry.expect("fixture entry").path())
    .filter(|path| {
        path.extension()
            .is_some_and(|extension| extension == "mojo")
    })
    .collect();
    paths.sort();
    paths
}

fn expected(source: &str) -> Option<&str> {
    source
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("# expect:").map(str::trim))
}

#[test]
fn origin_ok_fixtures_execute() {
    for path in fixtures("origin_ok") {
        let source = fs::read_to_string(&path).expect("read origin fixture");
        let program = parse(&source).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let checked =
            check_program(&program).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        check_ownership(&program).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let mut backend = BackendKind::Vm.make();
        backend
            .run(&checked)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

#[test]
fn origin_error_fixtures_are_rejected() {
    for path in fixtures("origin_error") {
        let source = fs::read_to_string(&path).expect("read origin fixture");
        let program = parse(&source).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let message = match check_program(&program) {
            Err(error) => error.to_string(),
            Ok(_) => check_ownership(&program)
                .expect_err("origin error fixture must fail checking or ownership")
                .to_string(),
        };
        if let Some(expected) = expected(&source) {
            assert!(
                message.contains(expected),
                "{}: expected '{expected}' in '{message}'",
                path.display()
            );
        }
    }
}
