//! Phase 4 — ownership (move) analysis tests.
//!
//! `check_ownership` runs after type-checking and models Mojo's move semantics: a
//! value transferred with `^` may not be used again. These tests cover the
//! positive cases (a move is fine if the value isn't used afterward, or is
//! reinitialized) and the violations (use-after-move, conditional move), including
//! the file fixtures under `assets/ownership_error/` (each pinned with `# expect:`)
//! and `assets/ownership_ok/`.

use mojo_lite::{OwnershipError, check, check_ownership, parse};

/// Type-check `src`, then run the ownership analysis.
fn own(src: &str) -> Result<(), OwnershipError> {
    let program = parse(src).expect("parse error");
    check(&program).expect("type error");
    check_ownership(&program)
}

const THING: &str = "@fieldwise_init\nstruct Thing:\n    var x: Int\n\n";

#[test]
fn move_without_later_use_is_ok() {
    // Transferring a value is fine as long as the source isn't used afterward.
    let src = format!(
        "{THING}def main():\n    var a: Thing = Thing(1)\n    var b: Thing = a^\n    print(b.x)\n"
    );
    assert!(own(&src).is_ok());
}

#[test]
fn reassign_after_move_is_ok() {
    // A moved variable reinitialized before its next use is fine ("reinit").
    let src = format!(
        "{THING}def main():\n    var a: Thing = Thing(1)\n    var b: Thing = a^\n    a = Thing(2)\n    print(a.x)\n"
    );
    assert!(own(&src).is_ok());
}

#[test]
fn no_transfer_never_errors() {
    // Without `^`, nothing moves; ordinary value semantics are untouched.
    let src = "def main():\n    var a: Int = 1\n    var b: Int = a\n    print(a)\n    print(b)\n";
    assert!(own(src).is_ok());
}

#[test]
fn use_after_move_is_rejected() {
    let src = format!(
        "{THING}def main():\n    var a: Thing = Thing(1)\n    var b: Thing = a^\n    print(a.x)\n"
    );
    match own(&src) {
        Err(OwnershipError::UseAfterMove { var, span }) => {
            assert_eq!(var, "a");
            // The message names the moved variable `a`; the span points at the
            // offending use expression `a.x` in `print(a.x)`.
            assert_eq!(&src[span.0..span.1], "a.x");
        }
        other => panic!("expected UseAfterMove, got {other:?}"),
    }
}

#[test]
fn double_transfer_is_use_after_move() {
    let src = format!(
        "{THING}def main():\n    var a: Thing = Thing(1)\n    var b: Thing = a^\n    var c: Thing = a^\n    print(b.x)\n"
    );
    assert!(matches!(
        own(&src),
        Err(OwnershipError::UseAfterMove { .. })
    ));
}

#[test]
fn conditional_move_is_rejected() {
    // Moved on one branch of an `if`, then used after the merge.
    let src = format!(
        "{THING}def main():\n    var flag: Bool = True\n    var a: Thing = Thing(1)\n    if flag:\n        var b: Thing = a^\n    print(a.x)\n"
    );
    match own(&src) {
        Err(OwnershipError::ConditionallyMoved { var, .. }) => assert_eq!(var, "a"),
        other => panic!("expected ConditionallyMoved, got {other:?}"),
    }
}

#[test]
fn move_in_loop_is_rejected() {
    // The first iteration moves `a`; the back-edge makes it (maybe-)moved on entry
    // to the second, so the transfer is flagged.
    let src = format!(
        "{THING}def main():\n    var a: Thing = Thing(1)\n    for i in range(3):\n        var b: Thing = a^\n        print(b.x)\n"
    );
    assert!(own(&src).is_err());
}

#[test]
fn ownership_error_fixtures() {
    // Every `assets/ownership_error/*.mojo` must be a move violation whose message
    // contains its `# expect:` substring.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ownership_error");
    let mut n = 0;
    for entry in std::fs::read_dir(dir).expect("assets/ownership_error exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mojo") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let expect = expect_substring(&src);
        match own(&src) {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains(expect),
                    "{}: message {msg:?} lacks expected {expect:?}",
                    path.display()
                );
                n += 1;
            }
            Ok(()) => panic!("{}: expected an ownership error", path.display()),
        }
    }
    assert!(n >= 4, "expected several ownership-error fixtures, ran {n}");
}

#[test]
fn ownership_ok_fixtures() {
    // Every `assets/ownership_ok/*.mojo` must pass the ownership analysis.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ownership_ok");
    for entry in std::fs::read_dir(dir).expect("assets/ownership_ok exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mojo") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        assert!(
            own(&src).is_ok(),
            "{}: expected no ownership error, got {:?}",
            path.display(),
            own(&src)
        );
    }
}

/// The `# expect: <substring>` pinned at the top of a fixture.
fn expect_substring(src: &str) -> &str {
    src.lines()
        .find_map(|l| l.trim_start().strip_prefix("# expect:"))
        .map(str::trim)
        .expect("fixture must pin a `# expect:` substring")
}

#[test]
fn use_after_move_through_a_place_write() {
    // Writing a field of a moved value is a use-after-move (caught via the place
    // root, not just a plain read).
    let src = format!(
        "{THING}def main():\n    var a: Thing = Thing(1)\n    var b: Thing = a^\n    a.x = 5\n"
    );
    assert!(matches!(
        own(&src),
        Err(OwnershipError::UseAfterMove { .. })
    ));
}

// Two non-copyable struct fields, so a partial move of one is unambiguous.
const PAIR: &str = "@fieldwise_init\nstruct Inner:\n    var id: Int\n\n@fieldwise_init\nstruct Pair:\n    var a: Inner\n    var b: Inner\n\n";

#[test]
fn partial_move_leaves_sibling_usable() {
    // Moving `p.a` out leaves `p.b` initialized and usable — field-sensitivity.
    let src = format!(
        "{PAIR}def main():\n    var p: Pair = Pair(Inner(1), Inner(2))\n    var x: Inner = p.a^\n    print(x.id)\n    print(p.b.id)\n"
    );
    assert!(
        own(&src).is_ok(),
        "sibling use after partial move: {:?}",
        own(&src)
    );
}

#[test]
fn use_of_moved_field_is_rejected() {
    // Reading the moved-out field itself is a use-after-move, named at the field.
    let src = format!(
        "{PAIR}def main():\n    var p: Pair = Pair(Inner(1), Inner(2))\n    var x: Inner = p.a^\n    print(p.a.id)\n"
    );
    match own(&src) {
        Err(OwnershipError::UseAfterMove { var, .. }) => assert_eq!(var, "p.a"),
        other => panic!("expected UseAfterMove of p.a, got {other:?}"),
    }
}

#[test]
fn whole_use_after_partial_move_is_rejected() {
    // Using the whole `p` (here transferring it) after a field was moved out is a
    // use-after-move, blamed on the moved field.
    let src = format!(
        "{PAIR}def main():\n    var p: Pair = Pair(Inner(1), Inner(2))\n    var x: Inner = p.a^\n    var q: Pair = p^\n    print(q.b.id)\n"
    );
    match own(&src) {
        Err(OwnershipError::UseAfterMove { var, .. }) => assert_eq!(var, "p.a"),
        other => panic!("expected UseAfterMove blamed on p.a, got {other:?}"),
    }
}

#[test]
fn reinitializing_a_moved_field_is_ok() {
    // Assigning the moved field re-initializes it; the whole value is usable again.
    let src = format!(
        "{PAIR}def main():\n    var p: Pair = Pair(Inner(1), Inner(2))\n    var x: Inner = p.a^\n    p.a = Inner(9)\n    print(p.a.id)\n    var q: Pair = p^\n    print(q.a.id)\n"
    );
    assert!(
        own(&src).is_ok(),
        "reinit after partial move: {:?}",
        own(&src)
    );
}

#[test]
fn conditional_partial_move_is_rejected() {
    // Moving `p.a` on one `if` arm makes it maybe-moved after the merge.
    let src = format!(
        "{PAIR}def main():\n    var flag: Bool = True\n    var p: Pair = Pair(Inner(1), Inner(2))\n    if flag:\n        var x: Inner = p.a^\n    print(p.a.id)\n"
    );
    match own(&src) {
        Err(OwnershipError::ConditionallyMoved { var, .. }) => assert_eq!(var, "p.a"),
        other => panic!("expected ConditionallyMoved of p.a, got {other:?}"),
    }
}

#[test]
fn moving_a_field_twice_is_use_after_move() {
    let src = format!(
        "{PAIR}def main():\n    var p: Pair = Pair(Inner(1), Inner(2))\n    var x: Inner = p.a^\n    var y: Inner = p.a^\n    print(x.id)\n    print(y.id)\n"
    );
    assert!(matches!(
        own(&src),
        Err(OwnershipError::UseAfterMove { .. })
    ));
}

#[test]
fn use_after_move_through_a_method_call() {
    let src = "@fieldwise_init\nstruct Thing:\n    var x: Int\n    def get(self) -> Int:\n        return self.x\n\ndef main():\n    var a: Thing = Thing(1)\n    var b: Thing = a^\n    print(a.get())\n";
    assert!(matches!(own(src), Err(OwnershipError::UseAfterMove { .. })));
}
