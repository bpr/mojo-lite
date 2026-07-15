//! Declaration collection, signature construction, and body-checking support.

use super::*;

pub(super) fn definitely_returns(body: &[Stmt]) -> bool {
    body.iter().any(stmt_returns)
}

pub(super) fn stmt_returns(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Return(_) => true,
        // A `raise` diverges (it never falls through to the end), so for
        // reachability it behaves like a `return`.
        StmtKind::Raise(_) => true,
        StmtKind::If { branches, orelse } => {
            orelse.as_ref().is_some_and(|e| definitely_returns(e))
                && branches.iter().all(|(_, b)| definitely_returns(b))
        }
        // A `try` definitely diverges when: a `finally` does (it overrides every
        // path); or the **normal-completion** path diverges (the body — or, if the
        // body may complete, the `else`) *and* the **exceptional** path does (every
        // `except` handler diverges; with no handler, an uncaught raise itself
        // exits, so only the normal path can fall through).
        StmtKind::Try {
            body,
            except,
            orelse,
            finalbody,
        } => {
            if finalbody.as_ref().is_some_and(|fb| definitely_returns(fb)) {
                return true;
            }
            let normal = match orelse {
                Some(else_) => definitely_returns(body) || definitely_returns(else_),
                None => definitely_returns(body),
            };
            let exceptional = match except {
                Some((_, handler)) => definitely_returns(handler),
                None => true,
            };
            normal && exceptional
        }
        _ => false,
    }
}
