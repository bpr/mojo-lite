//! Stage 6: ownership and persistent-loan analysis over MIR.
//!
//! Mojo's move semantics: transferring a value with `^` (`y = x^`, `take(x^)`)
//! leaves the source **uninitialized**, so using it again is an error. This pass
//! is a forward dataflow over each `MirFunction`'s basic blocks that tracks, per
//! variable, whether it is `Owned`, `Moved`, or — where control-flow paths
//! disagree — `MaybeMoved`. A use of a `Moved` variable is a **use-after-move**; a
//! use of a `MaybeMoved` one is a **conditional move** (transferred on some paths
//! but not others). Diagnostics carry the source [`Span`](crate::mir::Span) of the
//! offending use, recovered from the MIR `SpanTable`.
//!
//! This is a distinct compiler stage after MIR lowering. The production
//! [`Compiler`](crate::compiler::Compiler) always runs it before drop elaboration
//! and VM execution. Backward liveness then builds on this move/init foundation
//! to insert ASAP drops and control-flow-edge cleanup.

use crate::ast::Stmt;
use crate::error::OwnershipError;
use crate::hir::VarId;
use crate::mir::{
    MirBlock, MirFunction, MirInstr, MirPlace, MirProgram, MirTerm, Proj, Reg, SpanTable, UseMode,
    lower_program,
};
use std::collections::{BTreeMap, HashSet};

/// Run the ownership analysis over a whole program. Returns the first ownership
/// violation found (in function, then block, then instruction order), or `Ok` if
/// every value is used consistently with having been moved.
pub fn check_ownership(program: &[Stmt]) -> Result<(), OwnershipError> {
    let prog =
        lower_program(program).map_err(|error| OwnershipError::InvalidInput(error.to_string()))?;
    check_ownership_mir(&prog)
}

pub fn check_ownership_checked(
    program: &crate::checked::CheckedProgram,
) -> Result<(), OwnershipError> {
    let prog = crate::mir::lower_checked_program(program);
    check_ownership_mir(&prog)
}

fn check_ownership_mir(prog: &MirProgram) -> Result<(), OwnershipError> {
    for (_name, f) in &prog.functions {
        analyze_moves(f)?;
        analyze_loans(f)?;
    }
    Ok(())
}

// --- Liveness + ASAP drop elaboration ---------------------------------------

/// Elaborate ASAP destruction across a whole program: after each variable's last
/// use, splice a `DropVar`. Applied by the VM before execution so a struct's
/// `__del__` fires at the value's last use (not at scope end).
pub fn elaborate_drops_program(prog: MirProgram) -> MirProgram {
    MirProgram {
        functions: prog
            .functions
            .into_iter()
            .map(|(name, f)| {
                // Module-scope (`__toplevel__`) variables live until program end, so
                // they are not ASAP-dropped — that also keeps their final values
                // intact for the CLI/`bindings()` global dump (a `DropVar` would
                // clear the slot).
                let elaborated = if name == "__toplevel__" {
                    f
                } else {
                    elaborate_drops(&f)
                };
                (name, elaborated)
            })
            .collect(),
        declarations: prog.declarations,
        invariant_errors: prog.invariant_errors,
    }
}

/// The variables a MIR instruction reads, each paired with a nearby result
/// register (for a diagnostic span). Covers direct reads (`UseVar`), place roots
/// (`Store`/`LoadPlace`/a `mut self` receiver — so a write *through* a moved value
/// is caught too), and the `for` iterator variable.
fn var_uses(i: &MirInstr) -> Vec<(VarId, Reg)> {
    match i {
        MirInstr::BeginLoan { place, marker, .. } => vec![(place.root, *marker)],
        MirInstr::MakeRef { dest, place } => place_loan_uses(place, *dest),
        MirInstr::UseVar { dest, var, .. } => vec![(*var, *dest)],
        MirInstr::MovePlace { dest, place } => place_loan_uses(place, *dest),
        MirInstr::Store { place, src } => place_loan_uses(place, *src),
        MirInstr::LoadPlace { dest, place } => place_loan_uses(place, *dest),
        MirInstr::MethodCall {
            dest,
            recv_place: Some(p),
            ..
        } => vec![(p.root, *dest)],
        MirInstr::HasNext { dest, iter } | MirInstr::Next { dest, iter } => vec![(*iter, *dest)],
        // A `try` reads every variable its sub-regions read: the outer liveness must
        // treat it as one big use, so a value used only inside the `try` is not
        // dropped *before* it.
        MirInstr::Try {
            body,
            handler,
            orelse,
            finalbody,
            ..
        } => {
            let mut uses = Vec::new();
            let mut add = |bs: &[MirBlock]| {
                for b in bs {
                    for instr in &b.instrs {
                        uses.extend(var_uses(instr));
                    }
                }
            };
            add(body);
            if let Some((_, h)) = handler {
                add(h);
            }
            if let Some(e) = orelse {
                add(e);
            }
            if let Some(fb) = finalbody {
                add(fb);
            }
            uses
        }
        _ => Vec::new(),
    }
}

fn place_loan_uses(place: &MirPlace, reg: Reg) -> Vec<(VarId, Reg)> {
    let mut uses = vec![(place.root, reg)];
    if let Some(reference) = place.through {
        uses.push((reference, reg));
    }
    uses
}

/// The variables **defined** within a `try` region's blocks (a `DefVar` at any
/// nesting), excluding those moved out with `^` — the body-local values to destroy
/// when the body is left (the exceptional-edge / scope-exit cleanup).
fn region_cleanup_vars(blocks: &[MirBlock]) -> Vec<VarId> {
    let mut defined: Vec<VarId> = Vec::new();
    let mut moved: HashSet<VarId> = HashSet::new();
    for b in blocks {
        for instr in &b.instrs {
            if let Some(v) = var_def(instr)
                && !defined.contains(&v)
            {
                defined.push(v);
            }
            if let Some(v) = var_moved(instr) {
                moved.insert(v);
            }
        }
    }
    defined.retain(|v| !moved.contains(v));
    defined
}

/// The variable a MIR instruction writes (a `DefVar`), if any.
fn var_def(i: &MirInstr) -> Option<VarId> {
    match i {
        MirInstr::DefVar { var, .. } => Some(*var),
        _ => None,
    }
}

/// `BeginLoan` starts the reference's analytical live range, but it does not
/// overwrite the runtime handle already stored by `DefVar`.
fn loan_liveness_def(i: &MirInstr) -> Option<VarId> {
    match i {
        MirInstr::BeginLoan { reference, .. } => Some(*reference),
        _ => var_def(i),
    }
}

/// The variable transferred out by this instruction (a `^` move), if any — such a
/// variable is *not* dropped here (its value has moved to a new owner).
fn var_moved(i: &MirInstr) -> Option<VarId> {
    match i {
        MirInstr::UseVar {
            var,
            mode: UseMode::Move,
            ..
        } => Some(*var),
        _ => None,
    }
}

/// Insert `DropVar`s at each variable's last use. A backward liveness dataflow
/// finds where each variable dies (touched, then dead), and a forward rebuild
/// splices a drop right after — skipping variables moved out with `^` (their new
/// owner drops them) so nothing is double-dropped. At a shared death point,
/// variables drop in reverse declaration order (descending `VarId`).
///
/// Conservative by design: it drops at a variable's last use *within a block* and
/// leaks (rather than risk a double-free) on the branch edges where a value dies
/// without being used — full drop elaboration across branches is future work.
/// Whether the value in variable `v` is dropped by *this* function: locals always;
/// an `owned` parameter (the caller transferred it) yes; a borrowed parameter or
/// `self` never (the caller owns a borrow; `self` is written back / would recurse).
fn is_droppable_root(f: &MirFunction, v: VarId) -> bool {
    let vi = v as usize;
    if vi < f.n_params {
        f.owned_params.get(vi).copied().unwrap_or(false) && f.var_names[vi] != "self"
    } else {
        true
    }
}

fn elaborate_drops(f: &MirFunction) -> MirFunction {
    let nb = f.blocks.len();
    let loan_roots = loan_root_dependencies(f);

    // Backward liveness: live_in[b] = uses(b) ∪ (live_out[b] − defs(b)); live_out[b]
    // = ⋃ live_in[succ]. Fixpoint.
    let mut live_in: Vec<HashSet<VarId>> = vec![HashSet::new(); nb];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..nb).rev() {
            let live_out = block_live_out(f, b, &live_in);
            let new_in = transfer_drop_liveness(&f.blocks[b].instrs, live_out, &loan_roots);
            if new_in != live_in[b] {
                live_in[b] = new_in;
                changed = true;
            }
        }
    }

    // (1) Block-internal drops: replay each block, tracking the live set after each
    // instruction, and drop the variables that die at their last use in-block.
    let mut blocks = Vec::with_capacity(nb);
    for b in 0..nb {
        let live_out = block_live_out(f, b, &live_in);
        let instrs = &f.blocks[b].instrs;
        let mut live = live_out.clone();
        extend_with_loan_roots(&mut live, &loan_roots);
        let mut live_after = vec![HashSet::new(); instrs.len()];
        for i in (0..instrs.len()).rev() {
            live_after[i] = live.clone();
            if let Some(d) = var_def(&instrs[i]) {
                live.remove(&d);
            }
            for (u, _) in var_uses(&instrs[i]) {
                live.insert(u);
            }
            extend_with_loan_roots(&mut live, &loan_roots);
        }

        let mut new_instrs = Vec::with_capacity(instrs.len());
        for (i, instr) in instrs.iter().enumerate() {
            // For a `try`, fill its regions' escape-edge cleanups: the *outer*
            // variables live entering the `try` that are dead at the escape target
            // (so they die on the hidden `break`/`continue` edge), minus the `try`'s
            // own body-locals and moved-out values.
            let mut cloned = instr.clone();
            if let MirInstr::Try { .. } = &cloned {
                let mut live_before = live_after[i].clone();
                for (u, _) in var_uses(instr) {
                    live_before.insert(u);
                }
                let (mut rdef, mut rmov) = (HashSet::new(), HashSet::new());
                try_region_defs(instr, &mut rdef, &mut rmov);
                // `finally` runs *after* every escape, so a variable it uses must
                // survive the escape edge — exclude it (and the loop var, which the
                // `finally` typically reads) from the escape cleanup.
                let fin_used = match instr {
                    MirInstr::Try {
                        finalbody: Some(fb),
                        ..
                    } => region_uses(fb),
                    _ => HashSet::new(),
                };
                let base: HashSet<VarId> = live_before
                    .into_iter()
                    .filter(|v| {
                        is_droppable_root(f, *v)
                            && !rdef.contains(v)
                            && !rmov.contains(v)
                            && !fin_used.contains(v)
                    })
                    .collect();
                fill_escape_cleanups(&mut cloned, &base, &live_in);
            }
            new_instrs.push(cloned);
            let moved = var_moved(instr);
            let mut dying: Vec<VarId> = Vec::new();
            let touched = var_uses(instr)
                .into_iter()
                .map(|(v, _)| v)
                .chain(var_def(instr));
            for v in touched {
                if is_droppable_root(f, v)
                    && Some(v) != moved
                    && !live_after[i].contains(&v)
                    && !dying.contains(&v)
                {
                    dying.push(v);
                }
            }
            append_drops(&mut new_instrs, dying);
        }
        blocks.push(MirBlock {
            instrs: new_instrs,
            term: f.blocks[b].term.clone(),
        });
    }

    // (2) Edge drops: a variable live out of `p` but dead entering successor `s`
    // dies on the edge `p → s` (e.g. a value used on one `if` arm but not the
    // other). Drop it on that edge — at the end of `p` (if `p` has one successor),
    // the start of `s` (if `s` has one predecessor), or, for a critical edge, in a
    // fresh block spliced between them.
    let pred_count = predecessor_counts(f);
    for p in 0..nb {
        let live_out_p = block_live_out(f, p, &live_in);
        // Unique successors (a `Branch`'s arms are distinct in practice; dedup for
        // safety so an edge isn't processed — and dropped — twice).
        let mut succs: Vec<usize> = successors(&f.blocks[p].term);
        succs.sort_unstable();
        succs.dedup();
        let n_succ = succs.len();
        for &s in &succs {
            // Variables live out of `p` but dead entering `s` die on this edge.
            let dying: Vec<VarId> = live_out_p
                .iter()
                .copied()
                .filter(|&v| !live_in[s].contains(&v) && is_droppable_root(f, v))
                .collect();
            if dying.is_empty() {
                continue;
            }
            if n_succ == 1 {
                append_drops(&mut blocks[p].instrs, dying); // drop before the jump
            } else if pred_count[s] == 1 {
                prepend_drops(&mut blocks[s].instrs, dying); // drop on entry to `s`
            } else {
                // Critical edge: splice a drop block `p → new → s`.
                let new_idx = blocks.len();
                let mut instrs = Vec::new();
                append_drops(&mut instrs, dying);
                blocks.push(MirBlock {
                    instrs,
                    term: MirTerm::Jump(s),
                });
                rewire_target(&mut blocks[p].term, s, new_idx);
            }
        }
    }

    // Fill each `try`'s exceptional-edge cleanup (the body-local values to destroy
    // when the body is left), recursing into nested regions.
    set_try_cleanups(&mut blocks);

    MirFunction {
        blocks,
        n_regs: f.n_regs,
        n_vars: f.n_vars,
        var_names: f.var_names.clone(),
        n_params: f.n_params,
        param_annotations: f.param_annotations.clone(),
        owned_params: f.owned_params.clone(),
        ref_params: f.ref_params.clone(),
        returns_reference: f.returns_reference,
        spans: SpanTable(f.spans.0.clone()),
    }
}

/// A live reference keeps every possible owner root behind it alive. Dynamic
/// reference returns may conservatively name several roots, so retain all of
/// them; the runtime handle selects the actual one.
fn loan_root_dependencies(f: &MirFunction) -> BTreeMap<VarId, Vec<VarId>> {
    let mut dependencies: BTreeMap<VarId, Vec<VarId>> = BTreeMap::new();
    for instr in f.blocks.iter().flat_map(|block| &block.instrs) {
        if let MirInstr::BeginLoan {
            reference, place, ..
        } = instr
        {
            let roots = dependencies.entry(*reference).or_default();
            if !roots.contains(&place.root) {
                roots.push(place.root);
            }
        }
    }
    dependencies
}

fn extend_with_loan_roots(live: &mut HashSet<VarId>, dependencies: &BTreeMap<VarId, Vec<VarId>>) {
    loop {
        let roots: Vec<VarId> = live
            .iter()
            .filter_map(|reference| dependencies.get(reference))
            .flatten()
            .copied()
            .filter(|root| !live.contains(root))
            .collect();
        if roots.is_empty() {
            break;
        }
        live.extend(roots);
    }
}

fn transfer_drop_liveness(
    instrs: &[MirInstr],
    mut live: HashSet<VarId>,
    dependencies: &BTreeMap<VarId, Vec<VarId>>,
) -> HashSet<VarId> {
    extend_with_loan_roots(&mut live, dependencies);
    for instr in instrs.iter().rev() {
        if let Some(d) = var_def(instr) {
            live.remove(&d);
        }
        for (u, _) in var_uses(instr) {
            live.insert(u);
        }
        extend_with_loan_roots(&mut live, dependencies);
    }
    live
}

/// The variables *used* anywhere in a region's blocks (recursively, through nested
/// `try`s — `var_uses` already descends into a `Try` instruction).
fn region_uses(blocks: &[MirBlock]) -> HashSet<VarId> {
    let mut s = HashSet::new();
    for b in blocks {
        for instr in &b.instrs {
            for (v, _) in var_uses(instr) {
                s.insert(v);
            }
        }
    }
    s
}

/// Collect the variables *defined* or `^`-moved anywhere in a `try`'s regions
/// (recursively, through nested `try`s). Used to exclude a `try`'s own body-locals
/// (handled by `Try.cleanup`) and moved-out values from the escape-edge cleanup.
fn try_region_defs(try_instr: &MirInstr, defs: &mut HashSet<VarId>, moved: &mut HashSet<VarId>) {
    if let MirInstr::Try {
        body,
        handler,
        orelse,
        finalbody,
        ..
    } = try_instr
    {
        let mut regions: Vec<&Vec<MirBlock>> = vec![body];
        if let Some((_, h)) = handler {
            regions.push(h);
        }
        if let Some(e) = orelse {
            regions.push(e);
        }
        if let Some(fb) = finalbody {
            regions.push(fb);
        }
        for blocks in regions {
            for b in blocks {
                for instr in &b.instrs {
                    if let Some(v) = var_def(instr) {
                        defs.insert(v);
                    }
                    if let Some(v) = var_moved(instr) {
                        moved.insert(v);
                    }
                    try_region_defs(instr, defs, moved);
                }
            }
        }
    }
}

/// Fill each `EscapeJump.cleanup` inside a `try` (recursively) with the outer
/// variables from `base` that are dead at the escape's target block — those that
/// die on the hidden `break`/`continue` edge and must be destroyed there. `base`
/// already excludes the `try`'s body-locals (dropped by `Try.cleanup`), moved
/// values, and non-droppable roots.
fn fill_escape_cleanups(
    try_instr: &mut MirInstr,
    base: &HashSet<VarId>,
    live_in: &[HashSet<VarId>],
) {
    if let MirInstr::Try {
        body,
        handler,
        orelse,
        finalbody,
        ..
    } = try_instr
    {
        let mut regions: Vec<&mut Vec<MirBlock>> = vec![body];
        if let Some((_, h)) = handler {
            regions.push(h);
        }
        if let Some(e) = orelse {
            regions.push(e);
        }
        if let Some(fb) = finalbody {
            regions.push(fb);
        }
        for blocks in regions {
            for b in blocks.iter_mut() {
                for instr in b.instrs.iter_mut() {
                    fill_escape_cleanups(instr, base, live_in); // nested `try`s
                }
                if let MirTerm::EscapeJump { target, cleanup } = &mut b.term {
                    let dead_at_target = live_in.get(*target).cloned().unwrap_or_default();
                    let mut vars: Vec<VarId> = base
                        .iter()
                        .copied()
                        .filter(|v| !dead_at_target.contains(v))
                        .collect();
                    vars.sort_unstable_by(|a, b| b.cmp(a)); // reverse declaration order
                    *cleanup = vars;
                }
            }
        }
    }
}

/// Recursively fill every `MirInstr::Try`'s `cleanup` with the body's local
/// variables (dropped when the body is left, normally or via a raise).
fn set_try_cleanups(blocks: &mut [MirBlock]) {
    for b in blocks.iter_mut() {
        for instr in b.instrs.iter_mut() {
            if let MirInstr::Try {
                body,
                handler,
                orelse,
                finalbody,
                cleanup,
            } = instr
            {
                *cleanup = region_cleanup_vars(body);
                set_try_cleanups(body);
                if let Some((_, h)) = handler {
                    set_try_cleanups(h);
                }
                if let Some(e) = orelse {
                    set_try_cleanups(e);
                }
                if let Some(fb) = finalbody {
                    set_try_cleanups(fb);
                }
            }
        }
    }
}

/// The live-out set of block `b`: the union of its successors' live-in sets.
fn block_live_out(f: &MirFunction, b: usize, live_in: &[HashSet<VarId>]) -> HashSet<VarId> {
    let mut out = HashSet::new();
    for s in successors(&f.blocks[b].term) {
        out.extend(&live_in[s]);
    }
    out
}

/// Number of predecessors of each block (from terminator successors).
fn predecessor_counts(f: &MirFunction) -> Vec<usize> {
    let mut counts = vec![0usize; f.blocks.len()];
    for b in 0..f.blocks.len() {
        for s in successors(&f.blocks[b].term) {
            counts[s] += 1;
        }
    }
    counts
}

/// Append `DropVar`s for the given variables in reverse declaration order.
fn append_drops(instrs: &mut Vec<MirInstr>, mut vars: Vec<VarId>) {
    vars.sort_unstable_by(|a, b| b.cmp(a));
    for v in vars {
        instrs.push(MirInstr::DropVar { var: v });
    }
}

/// Prepend `DropVar`s (reverse declaration order) to the front of a block.
fn prepend_drops(instrs: &mut Vec<MirInstr>, mut vars: Vec<VarId>) {
    vars.sort_unstable_by(|a, b| b.cmp(a));
    for (i, v) in vars.into_iter().enumerate() {
        instrs.insert(i, MirInstr::DropVar { var: v });
    }
}

/// Redirect a terminator's `old` target to `new` (for critical-edge splitting).
fn rewire_target(term: &mut MirTerm, old: usize, new: usize) {
    match term {
        MirTerm::Jump(t) => {
            if *t == old {
                *t = new;
            }
        }
        MirTerm::Branch { then_b, else_b, .. } => {
            if *then_b == old {
                *then_b = new;
            }
            if *else_b == old {
                *else_b = new;
            }
        }
        // `EscapeJump` targets a block in the enclosing function; it never appears
        // as a *function-body* terminator (only inside a `try` region), and its
        // target isn't a critical-edge successor here, so leave it untouched.
        MirTerm::Return(_) | MirTerm::FallOff | MirTerm::EscapeJump { .. } => {}
    }
}

/// Backward transfer over a block for liveness.
fn transfer_liveness(instrs: &[MirInstr], mut live: HashSet<VarId>) -> HashSet<VarId> {
    for instr in instrs.iter().rev() {
        if let Some(d) = loan_liveness_def(instr) {
            live.remove(&d);
        }
        for (u, _) in var_uses(instr) {
            live.insert(u);
        }
    }
    live
}

#[derive(Clone)]
struct Loan {
    place: MirPlace,
    mutable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoanAccess {
    Read,
    Write,
}

/// Persistent local-loan checking. Reference variables participate in ordinary
/// backward liveness through `MirPlace::through`, so a loan is active precisely
/// from `BeginLoan` through the reference's last use, including CFG joins/loops.
fn analyze_loans(f: &MirFunction) -> Result<(), OwnershipError> {
    let mut loans: BTreeMap<VarId, Vec<Loan>> = BTreeMap::new();
    for instr in f.blocks.iter().flat_map(|block| &block.instrs) {
        if let MirInstr::BeginLoan {
            reference,
            place,
            mutable,
            ..
        } = instr
        {
            loans.entry(*reference).or_default().push(Loan {
                place: place.clone(),
                mutable: *mutable,
            });
        }
    }
    if loans.is_empty() {
        return Ok(());
    }

    let nb = f.blocks.len();
    let mut live_in = vec![HashSet::new(); nb];
    let mut changed = true;
    while changed {
        changed = false;
        for block in (0..nb).rev() {
            let live_out = block_live_out(f, block, &live_in);
            let incoming = transfer_liveness(&f.blocks[block].instrs, live_out);
            if incoming != live_in[block] {
                live_in[block] = incoming;
                changed = true;
            }
        }
    }

    for block in 0..nb {
        let instrs = &f.blocks[block].instrs;
        let mut live = block_live_out(f, block, &live_in);
        let mut live_before = vec![HashSet::new(); instrs.len()];
        for index in (0..instrs.len()).rev() {
            if let Some(def) = loan_liveness_def(&instrs[index]) {
                live.remove(&def);
            }
            for (used, _) in var_uses(&instrs[index]) {
                live.insert(used);
            }
            live_before[index] = live.clone();
        }

        for (index, instr) in instrs.iter().enumerate() {
            let active = &live_before[index];
            if let MirInstr::BeginLoan {
                reference,
                place,
                mutable,
                marker,
            } = instr
            {
                for other in active.iter().filter(|id| **id != *reference) {
                    if loans
                        .get(other)
                        .and_then(|loans| {
                            loans.iter().find(|existing| {
                                (*mutable || existing.mutable)
                                    && mir_places_overlap(place, &existing.place)
                            })
                        })
                        .is_some()
                    {
                        let span = f
                            .spans
                            .0
                            .get(&marker.0)
                            .map(|(span, _)| span.clone())
                            .unwrap_or_else(|| {
                                crate::token::SourceSpan::new(None, crate::token::DUMMY_SPAN)
                            });
                        return Err(loan_error(f, place, *other, span));
                    }
                }
                continue;
            }
            for (place, access, span) in loan_accesses(f, instr) {
                for reference in active {
                    let Some(reference_loans) = loans.get(reference) else {
                        continue;
                    };
                    if place.through == Some(*reference) {
                        if access == LoanAccess::Write
                            && reference_loans.iter().any(|loan| !loan.mutable)
                        {
                            return Err(loan_error(f, &place, *reference, span));
                        }
                        continue;
                    }
                    if reference_loans.iter().any(|loan| {
                        mir_places_overlap(&place, &loan.place)
                            && (access == LoanAccess::Write || loan.mutable)
                    }) {
                        return Err(loan_error(f, &place, *reference, span));
                    }
                }
            }
        }
    }
    Ok(())
}

fn mir_places_overlap(left: &MirPlace, right: &MirPlace) -> bool {
    left.root == right.root
        && left.proj.iter().zip(&right.proj).all(|(a, b)| {
            matches!((a, b), (Proj::Field(x), Proj::Field(y)) if x == y)
                || matches!((a, b), (Proj::Index(_), Proj::Index(_)))
        })
}

fn loan_accesses(
    f: &MirFunction,
    instr: &MirInstr,
) -> Vec<(MirPlace, LoanAccess, crate::token::SourceSpan)> {
    let fallback = crate::token::SourceSpan::new(None, crate::token::DUMMY_SPAN);
    let span_for = |reg: Reg| {
        f.spans
            .0
            .get(&reg.0)
            .map(|(span, _)| span.clone())
            .unwrap_or_else(|| fallback.clone())
    };
    match instr {
        MirInstr::UseVar { var, dest, mode } => vec![(
            MirPlace {
                root: *var,
                proj: Vec::new(),
                through: None,
            },
            if matches!(mode, UseMode::Move) {
                LoanAccess::Write
            } else {
                LoanAccess::Read
            },
            span_for(*dest),
        )],
        MirInstr::DefVar { var, src, .. } => vec![(
            MirPlace {
                root: *var,
                proj: Vec::new(),
                through: None,
            },
            LoanAccess::Write,
            span_for(*src),
        )],
        MirInstr::LoadPlace { dest, place } => {
            vec![(place.clone(), LoanAccess::Read, span_for(*dest))]
        }
        MirInstr::Store { place, src } => {
            vec![(place.clone(), LoanAccess::Write, span_for(*src))]
        }
        MirInstr::MovePlace { dest, place } => {
            vec![(place.clone(), LoanAccess::Write, span_for(*dest))]
        }
        MirInstr::Call {
            dest, arg_places, ..
        } => arg_places
            .iter()
            .flatten()
            .cloned()
            .map(|place| (place, LoanAccess::Write, span_for(*dest)))
            .collect(),
        MirInstr::MethodCall {
            dest,
            recv_place,
            arg_places,
            ..
        } => recv_place
            .iter()
            .chain(arg_places.iter().flatten())
            .cloned()
            .map(|place| (place, LoanAccess::Write, span_for(*dest)))
            .collect(),
        MirInstr::DropVar { var } => vec![(
            MirPlace {
                root: *var,
                proj: Vec::new(),
                through: None,
            },
            LoanAccess::Write,
            fallback,
        )],
        _ => Vec::new(),
    }
}

fn loan_error(
    f: &MirFunction,
    place: &MirPlace,
    reference: VarId,
    span: crate::token::SourceSpan,
) -> OwnershipError {
    OwnershipError::LoanConflict {
        place: place_display(&f.var_names[place.root as usize], &place_path(place)),
        loan: f.var_names[reference as usize].clone(),
        span,
    }
}

/// A place's move/init state. A three-point lattice ordered by how "moved" a
/// place might be; the merge of disagreeing paths is `MaybeMoved`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Own {
    /// Initialized and not transferred — safe to use.
    Owned,
    /// Transferred (`^`) on every path to here — using it is a use-after-move.
    Moved,
    /// Transferred on some paths but not others — using it is a conditional move.
    MaybeMoved,
}

/// The dataflow join (least upper bound): equal states are preserved; any
/// disagreement between `Owned` and `Moved`, or anything involving `MaybeMoved`,
/// becomes `MaybeMoved`.
fn join(a: Own, b: Own) -> Own {
    match (a, b) {
        (Own::Owned, Own::Owned) => Own::Owned,
        (Own::Moved, Own::Moved) => Own::Moved,
        _ => Own::MaybeMoved,
    }
}

/// A total order on the lattice: `Moved(2) > MaybeMoved(1) > Owned(0)`.
fn severity(o: Own) -> u8 {
    match o {
        Own::Owned => 0,
        Own::MaybeMoved => 1,
        Own::Moved => 2,
    }
}

// --- Place-tree ownership lattice (field-sensitive partial moves) -----------

/// One projection step in a place path. Dynamic indices collapse to a single
/// wildcard (`Index`) — the analysis is index-insensitive, a conservative choice
/// so any `xs[i]` aliases any `xs[j]`. (Indexed *moves* aren't lowered as partial
/// moves anyway; this only classifies field-vs-index in a place chain.)
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Key {
    Field(String),
    Index,
}

/// Map a MIR place's projection chain to a path of lattice keys.
fn place_path(place: &MirPlace) -> Vec<Key> {
    place
        .proj
        .iter()
        .map(|p| match p {
            Proj::Field(f) => Key::Field(f.clone()),
            Proj::Index(_) => Key::Index,
        })
        .collect()
}

/// A human-readable place name (`p`, `p.a`, `p.items[…]`) for diagnostics.
fn place_display(root: &str, path: &[Key]) -> String {
    let mut s = root.to_string();
    for k in path {
        match k {
            Key::Field(f) => {
                s.push('.');
                s.push_str(f);
            }
            Key::Index => s.push_str("[…]"),
        }
    }
    s
}

/// The move/init state of a place *and everything under it*, as a tree. `base`
/// is the state of this node's own value and of any child not present in
/// `children`; `children` refine specific sub-places (fields / the wildcard
/// index). A partial move is `base = Owned` with a `Moved` child. Invariant: a
/// `base == Moved` node has no children (moving the whole clears sub-state); a
/// control-flow join may produce `base == MaybeMoved` with children.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Node {
    base: Own,
    children: BTreeMap<Key, Node>,
}

impl Node {
    fn owned() -> Node {
        Node {
            base: Own::Owned,
            children: BTreeMap::new(),
        }
    }

    /// Severity of *reading the whole subtree* at this node, paired with the
    /// relative path of the worst offender (for a precise diagnostic): the worst
    /// of its own base (path `[]`) and every descendant's whole severity — a
    /// moved child taints a whole read of the parent, and is named as the blame.
    fn whole(&self) -> (Own, Vec<Key>) {
        let mut worst = (self.base, Vec::new());
        for (k, c) in &self.children {
            let (sev, sub) = c.whole();
            if severity(sev) > severity(worst.0) {
                let mut full = vec![k.clone()];
                full.extend(sub);
                worst = (sev, full);
            }
        }
        worst
    }

    /// The state of *reading* the place reached by `path` (its whole subtree),
    /// combined with any moved ancestor passed through along the way. Returns the
    /// severity and the blamed sub-path: a moved *ancestor* blames the ancestor,
    /// a moved *descendant* of a whole read blames the descendant.
    fn read(&self, path: &[Key]) -> (Own, Vec<Key>) {
        match path.split_first() {
            None => self.whole(),
            Some((k, rest)) => match self.children.get(k) {
                // A moved ancestor on the way down blames the ancestor itself.
                Some(_) if self.base != Own::Owned => (self.base, Vec::new()),
                Some(child) => {
                    let (sev, sub) = child.read(rest);
                    let mut full = vec![k.clone()];
                    full.extend(sub);
                    (sev, full)
                }
                None => (self.base, Vec::new()), // uniform subtree: state is `base`
            },
        }
    }

    /// The base state of the *node itself* reached by `path` — only ancestor
    /// bases matter, not sibling/descendant moves. Used to check a field write,
    /// whose parent must merely be initialized (not wholly moved), so writing
    /// `p.a` is legal even when the sibling `p.b` has been moved out. Blames the
    /// nearest moved ancestor.
    fn base_at(&self, path: &[Key]) -> (Own, Vec<Key>) {
        match path.split_first() {
            None => (self.base, Vec::new()),
            Some((_, _)) if self.base != Own::Owned => (self.base, Vec::new()),
            Some((k, rest)) => match self.children.get(k) {
                Some(child) => {
                    let (sev, sub) = child.base_at(rest);
                    let mut full = vec![k.clone()];
                    full.extend(sub);
                    (sev, full)
                }
                None => (Own::Owned, Vec::new()),
            },
        }
    }

    /// Mark the place at `path` as wholly moved (clearing its sub-state).
    fn do_move(&mut self, path: &[Key]) {
        match path.split_first() {
            None => {
                *self = Node {
                    base: Own::Moved,
                    children: BTreeMap::new(),
                }
            }
            Some((k, rest)) => {
                let base = self.base;
                self.children
                    .entry(k.clone())
                    .or_insert_with(|| Node {
                        base,
                        children: BTreeMap::new(),
                    })
                    .do_move(rest);
            }
        }
    }

    /// Re-initialize the place at `path` to `Owned` (a def / field store).
    fn do_def(&mut self, path: &[Key]) {
        match path.split_first() {
            None => *self = Node::owned(),
            Some((k, rest)) => {
                // Reinitializing a field of a wholly-moved value is itself invalid
                // (caught as a write through a moved parent); don't corrupt state.
                if self.base == Own::Moved {
                    return;
                }
                let base = self.base;
                self.children
                    .entry(k.clone())
                    .or_insert_with(|| Node {
                        base,
                        children: BTreeMap::new(),
                    })
                    .do_def(rest);
            }
        }
    }
}

/// Join two place-trees at a control-flow merge (a per-node dataflow lub). A key
/// present on only one side inherits that side's `base` for the missing child.
fn join_node(a: &Node, b: &Node) -> Node {
    let base = join(a.base, b.base);
    let mut children = BTreeMap::new();
    let mut keys: Vec<&Key> = a.children.keys().chain(b.children.keys()).collect();
    keys.sort_unstable();
    keys.dedup();
    for k in keys {
        let ca = a.children.get(k).cloned().unwrap_or(Node {
            base: a.base,
            children: BTreeMap::new(),
        });
        let cb = b.children.get(k).cloned().unwrap_or(Node {
            base: b.base,
            children: BTreeMap::new(),
        });
        children.insert(k.clone(), join_node(&ca, &cb));
    }
    Node { base, children }
}

/// A basic block's successors (by terminator).
fn successors(term: &MirTerm) -> Vec<usize> {
    match term {
        MirTerm::Jump(t) => vec![*t],
        MirTerm::Branch { then_b, else_b, .. } => vec![*then_b, *else_b],
        // `EscapeJump` only appears inside a `try` region (never a function body),
        // so this — which walks function-body successors — never sees it; it leaves
        // this CFG like a `Return`.
        MirTerm::Return(_) | MirTerm::FallOff | MirTerm::EscapeJump { .. } => vec![],
    }
}

/// How an instruction touches a place: a whole-value *read* (using the subtree),
/// or the *structural* parent-check of a field write (the parent must merely be
/// initialized, not wholly moved — so writing `p.a` is fine when `p.b` is moved).
enum Touch {
    Read,
    WriteParent,
}

/// The places an instruction *reads* or structurally touches (for reporting),
/// each with the register whose span points at the offending source. Moves and
/// definitions are applied separately by [`apply_effects`].
fn place_uses(i: &MirInstr) -> Vec<(VarId, Vec<Key>, Touch, Reg)> {
    match i {
        MirInstr::BeginLoan { place, marker, .. } => {
            vec![(place.root, place_path(place), Touch::Read, *marker)]
        }
        // A whole-variable read/borrow (a bare `x`) or move (`x^`): reads the
        // whole variable first.
        MirInstr::UseVar { dest, var, .. } => vec![(*var, Vec::new(), Touch::Read, *dest)],
        // A place read (`p.a`, a read-modify-write load) or a partial move
        // (`p.a^`): reads that specific sub-place.
        MirInstr::LoadPlace { dest, place } | MirInstr::MovePlace { dest, place } => {
            vec![(place.root, place_path(place), Touch::Read, *dest)]
        }
        // A place write `p…​.f = e`: the *parent* place must be initialized (the
        // field itself is being overwritten, so it need not be). A dynamic-index
        // write keeps the whole chain as the parent (conservative).
        MirInstr::Store { place, src } => {
            let mut path = place_path(place);
            if matches!(place.proj.last(), Some(Proj::Field(_))) {
                path.pop(); // drop the final field — check its parent
            }
            vec![(place.root, path, Touch::WriteParent, *src)]
        }
        // The `for` iterator variable is read (and advanced) — treat as a whole read.
        MirInstr::HasNext { dest, iter } | MirInstr::Next { dest, iter } => {
            vec![(*iter, Vec::new(), Touch::Read, *dest)]
        }
        _ => Vec::new(),
    }
}

/// Apply an instruction's move/def effects to a place-tree state (no reporting):
/// a `DefVar` (re)initializes a whole variable, a `^` transfer moves one, a
/// partial move `p.a^` moves that sub-place, and a field store reinitializes the
/// written field.
fn apply_effects(state: &mut [Node], i: &MirInstr) {
    match i {
        MirInstr::DefVar { var, .. } => state[*var as usize].do_def(&[]),
        MirInstr::UseVar {
            var,
            mode: UseMode::Move,
            ..
        } => state[*var as usize].do_move(&[]),
        MirInstr::MovePlace { place, .. } => {
            state[place.root as usize].do_move(&place_path(place));
        }
        // A field store reinitializes exactly that field; a dynamic-index store
        // can't precisely reinitialize one element (index-insensitive), so it is
        // left conservative (no reinit).
        MirInstr::Store { place, .. } if matches!(place.proj.last(), Some(Proj::Field(_))) => {
            state[place.root as usize].do_def(&place_path(place));
        }
        _ => {}
    }
}

/// Apply a block's instructions to a place-tree state, *without* reporting (used
/// to reach the dataflow fixpoint).
fn transfer(state: &mut [Node], instrs: &[MirInstr]) {
    for i in instrs {
        apply_effects(state, i);
    }
}

/// Join two per-variable place-tree states (control-flow merge).
fn join_states(a: &[Node], b: &[Node]) -> Vec<Node> {
    a.iter().zip(b).map(|(x, y)| join_node(x, y)).collect()
}

/// Analyze one function body for move violations, field-sensitively (partial
/// moves): a value transferred with `^` — whole (`x^`) or a field (`p.a^`) — may
/// not be read again on that path, but a disjoint sibling (`p.b`) stays usable.
fn analyze_moves(f: &MirFunction) -> Result<(), OwnershipError> {
    let nb = f.blocks.len();
    let nv = f.n_vars;

    // Predecessor lists, from each block's successors.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nb];
    for (b, blk) in f.blocks.iter().enumerate() {
        for s in successors(&blk.term) {
            preds[s].push(b);
        }
    }

    // The entry starts every variable `Owned` — the checker guarantees definite
    // assignment before use, so this never causes a false negative for our
    // purpose (tracking transfers) and avoids a spurious "uninitialized" lattice.
    let entry: Vec<Node> = vec![Node::owned(); nv];
    let mut in_states: Vec<Vec<Node>> = vec![entry.clone(); nb];
    let mut out_states: Vec<Vec<Node>> = vec![entry.clone(); nb];

    // Iterate to a fixpoint: in[b] = ⨆ out[pred], out[b] = transfer(in[b]).
    let mut changed = true;
    while changed {
        changed = false;
        #[allow(clippy::needless_range_loop)]
        for b in 0..nb {
            let new_in = if b == 0 || preds[b].is_empty() {
                entry.clone() // entry block, or an unreachable one
            } else {
                let mut acc = out_states[preds[b][0]].clone();
                for &p in &preds[b][1..] {
                    acc = join_states(&acc, &out_states[p]);
                }
                acc
            };
            let mut new_out = new_in.clone();
            transfer(&mut new_out, &f.blocks[b].instrs);
            if new_in != in_states[b] || new_out != out_states[b] {
                in_states[b] = new_in;
                out_states[b] = new_out;
                changed = true;
            }
        }
    }

    // Reporting pass: replay each block from its fixed-point in-state, checking
    // every place use against the current move-state. Returns the first violation.
    #[allow(clippy::needless_range_loop)]
    for b in 0..nb {
        let mut state = in_states[b].clone();
        for instr in &f.blocks[b].instrs {
            for (root, path, touch, reg) in place_uses(instr) {
                let node = &state[root as usize];
                let (sev, blame) = match touch {
                    Touch::Read => node.read(&path),
                    Touch::WriteParent => node.base_at(&path),
                };
                if sev != Own::Owned {
                    let span = f
                        .spans
                        .0
                        .get(&reg.0)
                        .map(|(s, _)| s.clone())
                        .unwrap_or_else(|| crate::token::SourceSpan::new(None, (0, 0)));
                    let var = place_display(&f.var_names[root as usize], &blame);
                    return Err(match sev {
                        Own::Moved => OwnershipError::UseAfterMove { var, span },
                        _ => OwnershipError::ConditionallyMoved { var, span },
                    });
                }
            }
            apply_effects(&mut state, instr);
        }
    }
    Ok(())
}
