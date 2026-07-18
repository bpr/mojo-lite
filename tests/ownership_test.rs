//! Phase 4 — ownership (move) analysis tests.
//!
//! `check_ownership` runs after type-checking and models Mojo's move semantics: a
//! value transferred with `^` may not be used again. These tests cover the
//! positive cases (a move is fine if the value isn't used afterward, or is
//! reinitialized) and the violations (use-after-move, conditional move), including
//! the file fixtures under `assets/ownership_error/` (each pinned with `# expect:`)
//! and `assets/ownership_ok/`.

use mojito::{OwnershipError, check, check_ownership, parse};

/// Type-check `src`, then run the ownership analysis.
fn own(src: &str) -> Result<(), OwnershipError> {
    let program = parse(src).expect("parse error");
    check(&program).expect("type error");
    check_ownership(&program)
}

#[test]
fn ownership_reports_invalid_unchecked_input_instead_of_panicking() {
    let program = parse("def main():\n    print(missing)\n").expect("parse error");
    assert!(matches!(
        check_ownership(&program),
        Err(OwnershipError::InvalidInput(message)) if message.contains("missing")
    ));
}

#[test]
fn reference_aggregate_extends_the_owner_loan() {
    let src = "@fieldwise_init\nstruct RefBox[origin: Origin[mut=True]]:\n    var value: ref[origin] Int\n\ndef main():\n    var value = 40\n    ref alias = value\n    var box = RefBox(alias)\n    value += 1\n    print(box.value)\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
}

#[test]
fn reference_aggregate_preserves_every_field_loan() {
    let src = "@fieldwise_init\nstruct RefPair[a: Origin[mut=True], b: Origin[mut=True]]:\n    var first: ref[a] Int\n    var second: ref[b] Int\n\ndef main():\n    var x = 10\n    var y = 20\n    ref rx = x\n    ref ry = y\n    var pair = RefPair(rx, ry)\n    y += 1\n    print(pair.second)\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
}

#[test]
fn moving_reference_aggregate_transfers_its_owner_loan() {
    let src = "@fieldwise_init\nstruct RefBox[origin: Origin[mut=True]]:\n    var value: ref[origin] Int\n\ndef main():\n    var value = 40\n    ref alias = value\n    var box = RefBox(alias)\n    var moved = box^\n    value += 1\n    print(moved.value)\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
}

#[test]
fn nested_reference_aggregate_preserves_every_element_loan() {
    // Executable `ref` fields are a Mojito extension used to prove the checked
    // aggregate/loan representation; current Mojo spells stored provenance with
    // origin-bearing pointer types instead.
    let src = "@fieldwise_init\nstruct RefTuple[origin: Origin[mut=True]]:\n    var values: Tuple[ref[origin] Int, ref[origin] Int]\n\ndef main():\n    var x = 10\n    var y = 20\n    ref rx = x\n    ref ry = y\n    var pair = RefTuple((rx, ry))\n    y += 1\n    print(pair.values[1])\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));

    let src = "@fieldwise_init\nstruct RefList[origin: Origin[mut=True]]:\n    var values: List[ref[origin] Int]\n\ndef main():\n    var x = 10\n    var y = 20\n    ref rx = x\n    ref ry = y\n    var pair = RefList([rx, ry])\n    y += 1\n    print(pair.values[1])\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
}

#[test]
fn variant_payload_reference_loans_the_variant() {
    // The ownership unit runs the unlinked checker, so a local declaration makes
    // the compiler-provided Variant name visible without involving module I/O.
    let src = "struct Variant:\n    pass\n\ndef main():\n    var value = Variant[Int, String](7)\n    ref payload = value[Int]\n    value.set[String](\"changed\")\n    print(payload)\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
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
fn owned_iteration_consumes_the_source_collection() {
    let ok_source = format!(
        "{THING}def main():\n    var values = [Thing(1), Thing(2)]\n    for var item in values^:\n        print(item.x)\n"
    );
    assert!(own(&ok_source).is_ok());

    let used_again = format!(
        "{THING}def main():\n    var values = [Thing(1), Thing(2)]\n    for var item in values^:\n        print(item.x)\n    print(len(values))\n"
    );
    match own(&used_again) {
        Err(OwnershipError::UseAfterMove { .. }) => {}
        other => panic!("expected owned iteration to consume its source, got {other:?}"),
    }
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
fn transferred_tuple_reverse_consumes_the_receiver() {
    let src = "def main():\n    var pair = Tuple(3, \"seven\")\n    var reversed = pair^.reverse()\n    print(reversed)\n    print(pair)\n";
    assert!(matches!(own(src), Err(OwnershipError::UseAfterMove { .. })));
}

#[test]
fn transferred_tuple_concat_consumes_both_operands() {
    let src = "def main():\n    var left = Tuple(3, \"seven\")\n    var right = Tuple(True)\n    var joined = left^.concat(right^)\n    print(joined)\n    print(right)\n";
    assert!(matches!(own(src), Err(OwnershipError::UseAfterMove { .. })));
}

#[test]
fn local_reference_loan_ends_at_last_use() {
    let src = "def main():\n    var value: Int = 1\n    ref alias = value\n    print(alias)\n    value = 2\n    print(value)\n";
    assert!(own(src).is_ok());
}

#[test]
fn local_reference_blocks_owner_access_while_live() {
    let src = "def main():\n    var value: Int = 1\n    ref alias = value\n    value = 2\n    print(alias)\n";
    assert!(matches!(
        own(src),
        Err(OwnershipError::LoanConflict { place, loan, .. })
            if place == "value" && loan == "alias"
    ));
}

#[test]
fn local_reference_loans_are_field_sensitive() {
    let src = "@fieldwise_init\nstruct Pair:\n    var left: Int\n    var right: Int\n\ndef main():\n    var pair = Pair(1, 2)\n    ref alias = pair.left\n    pair.right = 3\n    print(alias)\n";
    assert!(own(src).is_ok());
}

#[test]
fn local_reference_loan_flows_through_cfg_join() {
    let src = "def main():\n    var value: Int = 1\n    var flag: Bool = True\n    ref alias = value\n    if flag:\n        print(0)\n    value = 2\n    print(alias)\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
}

#[test]
fn local_reference_blocks_mutating_calls_between_uses() {
    let src = "def replace(mut value: Int):\n    value = 2\n\ndef main():\n    var value: Int = 1\n    ref alias = value\n    replace(value)\n    print(alias)\n";
    assert!(matches!(own(src), Err(OwnershipError::LoanConflict { .. })));
}

#[test]
fn returned_reference_establishes_a_persistent_caller_loan() {
    let source = "def borrow(ref value: Int) -> ref[value] Int:\n    return value\n\ndef main():\n    var value = 1\n    ref alias = borrow(value)\n    value = 2\n    print(alias)\n";
    assert!(matches!(
        own(source),
        Err(OwnershipError::LoanConflict { .. })
    ));

    let after_last_use = "def borrow(ref value: Int) -> ref[value] Int:\n    return value\n\ndef main():\n    var value = 1\n    ref alias = borrow(value)\n    print(alias)\n    value = 2\n    print(value)\n";
    assert!(own(after_last_use).is_ok());
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
            assert_eq!(&src[span.span.0..span.span.1], "a.x");
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
