//! Phase 1 (AST → HIR/CFG) tests. They assert the **shape** of the control-flow
//! graph — block count, edges, and terminators — for each control-flow construct,
//! since Phase 1 is about structure, not instruction contents.

use mojo_lite::hir::{Cfg, Terminator};
use mojo_lite::parse;

/// Parse a source snippet and lower it to a CFG.
fn cfg(src: &str) -> Cfg {
    Cfg::build(&parse(src).expect("parse error"))
}

/// Every block must be sealed with exactly one terminator (CFG well-formedness).
fn assert_all_sealed(cfg: &Cfg) {
    for b in cfg.g.node_indices() {
        assert!(cfg.term(b).is_some(), "block {:?} has no terminator", b);
    }
}

#[test]
fn straight_line_is_one_block_returning() {
    // No control flow ⇒ a single block, implicitly `return None`.
    let c = cfg("var x: Int = 1\nvar y: Int = 2\n");
    assert_eq!(c.node_count(), 1);
    assert_eq!(c.edge_count(), 0);
    assert!(matches!(c.term(c.entry), Some(Terminator::Return(None))));
    assert_eq!(c.block(c.entry).instrs.len(), 2); // two Binds
    assert_all_sealed(&c);
}

#[test]
fn if_without_else_has_four_blocks_and_a_diamond() {
    // entry ─Branch→ {then, else}; both ─Jump→ join.  4 blocks, 4 edges.
    let c = cfg("if a:\n    pass\n");
    assert_eq!(c.node_count(), 4);
    assert_eq!(c.edge_count(), 4);
    assert_all_sealed(&c);

    // The entry branches to two successors, which both reach a single join block.
    match c.term(c.entry) {
        Some(Terminator::Branch { then_b, else_b, .. }) => {
            let join_from_then = c.successors(*then_b)[0];
            let join_from_else = c.successors(*else_b)[0];
            assert_eq!(
                join_from_then, join_from_else,
                "then and else must merge at one join"
            );
        }
        other => panic!("entry should end in a Branch, got {other:?}"),
    }
}

#[test]
fn if_elif_else_has_six_blocks_seven_edges() {
    // entry ─Branch→{then1, else1}; else1 ─Branch→{then2, else2(=orelse)};
    // then1, then2, else2 all ─Jump→ join.  6 blocks, 7 edges.
    let c = cfg("if a:\n    pass\nelif b:\n    pass\nelse:\n    pass\n");
    assert_eq!(c.node_count(), 6);
    assert_eq!(c.edge_count(), 7);
    assert_all_sealed(&c);
}

#[test]
fn while_loop_has_header_body_exit_with_back_edge() {
    // entry→header; header ─Branch→{body, exit}; body ─Jump→ header (back-edge).
    // 4 blocks, 4 edges.
    let c = cfg("while a:\n    pass\n");
    assert_eq!(c.node_count(), 4);
    assert_eq!(c.edge_count(), 4);
    assert_all_sealed(&c);

    // The header is the entry's sole successor and branches to two blocks, one of
    // which (the body) jumps back to the header.
    let header = c.successors(c.entry)[0];
    let (body, exit) = match c.term(header) {
        Some(Terminator::Branch { then_b, else_b, .. }) => (*then_b, *else_b),
        other => panic!("header should Branch, got {other:?}"),
    };
    assert!(
        c.has_edge(body, header),
        "body must have a back-edge to the header"
    );
    assert!(
        matches!(c.term(exit), Some(Terminator::Return(None))),
        "exit falls through to return"
    );
}

#[test]
fn break_jumps_to_the_loop_exit_not_the_header() {
    // `while a: break` — the body jumps to the exit and has NO back-edge.
    let c = cfg("while a:\n    break\n");
    let header = c.successors(c.entry)[0];
    let (body, exit) = match c.term(header) {
        Some(Terminator::Branch { then_b, else_b, .. }) => (*then_b, *else_b),
        other => panic!("header should Branch, got {other:?}"),
    };
    assert!(c.has_edge(body, exit), "break must jump to the loop exit");
    assert!(
        !c.has_edge(body, header),
        "a body that always breaks has no back-edge"
    );
    assert_all_sealed(&c);
}

#[test]
fn continue_jumps_back_to_the_header() {
    // `while a: continue` — the body jumps to the header (a back-edge), like a
    // normal loop end, and does NOT reach the exit directly from the body.
    let c = cfg("while a:\n    continue\n");
    let header = c.successors(c.entry)[0];
    let (body, exit) = match c.term(header) {
        Some(Terminator::Branch { then_b, else_b, .. }) => (*then_b, *else_b),
        other => panic!("header should Branch, got {other:?}"),
    };
    assert!(
        c.has_edge(body, header),
        "continue jumps back to the header"
    );
    assert!(
        !c.has_edge(body, exit),
        "the body does not reach the exit directly"
    );
    assert_all_sealed(&c);
}

#[test]
fn nested_loops_break_targets_the_innermost_exit() {
    // The inner `break` must target the INNER loop's exit, never the outer's.
    let c = cfg("while a:\n    while b:\n        break\n");
    // Find the outer header (entry's successor) and the inner header (the outer
    // body's successor). The inner body must edge to the inner exit.
    let outer_header = c.successors(c.entry)[0];
    let outer_body = match c.term(outer_header) {
        Some(Terminator::Branch { then_b, .. }) => *then_b,
        other => panic!("outer header should Branch, got {other:?}"),
    };
    // The outer body jumps to the inner loop's pre-header/header.
    let inner_header = c.successors(outer_body)[0];
    let (inner_body, inner_exit) = match c.term(inner_header) {
        Some(Terminator::Branch { then_b, else_b, .. }) => (*then_b, *else_b),
        other => panic!("inner header should Branch, got {other:?}"),
    };
    assert!(
        c.has_edge(inner_body, inner_exit),
        "inner break targets the inner exit"
    );
    assert_all_sealed(&c);
}

#[test]
fn return_seals_the_block_with_a_value() {
    let c = cfg("return 1\n");
    assert_eq!(c.node_count(), 1);
    assert!(matches!(c.term(c.entry), Some(Terminator::Return(Some(_)))));
    assert_all_sealed(&c);
}

#[test]
fn code_after_a_return_in_a_branch_does_not_flow_to_the_join() {
    // In `if a: return 1  else: pass`, the then-block returns, so it must NOT edge
    // to the join; only the else-block reaches it.
    let c = cfg("if a:\n    return 1\nelse:\n    pass\n");
    let (then_b, else_b) = match c.term(c.entry) {
        Some(Terminator::Branch { then_b, else_b, .. }) => (*then_b, *else_b),
        other => panic!("entry should Branch, got {other:?}"),
    };
    assert!(matches!(c.term(then_b), Some(Terminator::Return(Some(_)))));
    let join = c.successors(else_b)[0];
    assert!(
        !c.has_edge(then_b, join),
        "a returning branch does not reach the join"
    );
    assert!(c.has_edge(else_b, join));
    assert_all_sealed(&c);
}

#[test]
fn for_loop_lowers_to_a_while_shaped_graph() {
    // A `for` gets the same header/body/exit skeleton as a `while` (the iterator
    // protocol is a Phase 2 refinement; the shape is what Phase 1 fixes).
    let c = cfg("for i in range(3):\n    pass\n");
    let header = c.successors(c.entry)[0];
    let (body, _exit) = match c.term(header) {
        Some(Terminator::Branch { then_b, else_b, .. }) => (*then_b, *else_b),
        other => panic!("for-header should Branch, got {other:?}"),
    };
    assert!(
        c.has_edge(body, header),
        "for-body has a back-edge to the header"
    );
    assert_all_sealed(&c);
}

#[test]
fn seeded_region_break_continue_escape_to_external_loops() {
    // A region seeded with an enclosing function loop lowers an outward `break` to
    // `EscapeJump(exit)` and `continue` to `EscapeJump(header)` — carrying the
    // enclosing block ids (not region-local nodes).
    let body = mojo_lite::parse("break\n").expect("parse");
    let header = mojo_lite::hir::BlockId::new(41);
    let exit = mojo_lite::hir::BlockId::new(42);
    let region = Cfg::build_seeded_with_loops(Vec::new(), &body, &[(header, exit)]);
    let esc = region
        .g
        .node_indices()
        .find_map(|b| match region.term(b) {
            Some(Terminator::EscapeJump(t)) => Some(*t),
            _ => None,
        })
        .expect("break should escape as EscapeJump");
    assert_eq!(esc, exit, "break escapes to the enclosing loop exit");

    let cont_body = mojo_lite::parse("continue\n").expect("parse");
    let region2 = Cfg::build_seeded_with_loops(Vec::new(), &cont_body, &[(header, exit)]);
    let esc2 = region2
        .g
        .node_indices()
        .find_map(|b| match region2.term(b) {
            Some(Terminator::EscapeJump(t)) => Some(*t),
            _ => None,
        })
        .expect("continue should escape as EscapeJump");
    assert_eq!(esc2, header, "continue escapes to the enclosing loop header");
}

#[test]
fn nested_loop_in_region_absorbs_its_own_break() {
    // A loop declared *inside* the region absorbs its own `break` (a local `Jump`),
    // so no `EscapeJump` is produced.
    let body = mojo_lite::parse("while True:\n    break\n").expect("parse");
    let header = mojo_lite::hir::BlockId::new(7);
    let exit = mojo_lite::hir::BlockId::new(8);
    let region = Cfg::build_seeded_with_loops(Vec::new(), &body, &[(header, exit)]);
    let has_escape = region
        .g
        .node_indices()
        .any(|b| matches!(region.term(b), Some(Terminator::EscapeJump(_))));
    assert!(!has_escape, "a break of the region's own loop stays a local Jump");
}
