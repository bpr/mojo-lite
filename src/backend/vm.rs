//! Phase 3 — register-VM backend (parity first).
//!
//! Executes **verified MIR** directly: a program is lowered by `lower_program`
//! into one `MirFunction` per `def`/method plus a synthetic `__toplevel__`, and
//! this interpreter walks each function's basic blocks over a per-call frame of
//! register + variable slots. No ownership analysis yet (that is Phase 4) — the
//! goal here is to reproduce the tree-walker's observable behaviour (its captured
//! `print` output). Value semantics come for free from `Value: Clone`.
//!
//! **Reuse for parity:** the VM and the tree-walker share one value-level layer,
//! [`crate::runtime`] — `Value`, `apply_infix`/`apply_prefix`, `coerce`/
//! `coerce_like`, the `List` method logic (`apply_list_method`/`list_query`), the
//! `SIMD`/place operations, and `Value`'s `Display` — so the two backends can't
//! drift on the covered subset, and the VM does not depend on the evaluator.
//!
//! **Covered:** scalars & `String`; all operators (short-circuit `and`/`or`
//! lowered to CFG); `if`/`while`; `for`/`range` and `for` over a `List`; variable
//! read/write and literal coercion; user `def` calls (parameter ABI) + recursion;
//! `return`; **structs** (fieldwise construction, field read, method calls incl.
//! `mut self` write-back, `mut`/`ref` ordinary-param write-back); **`List`/`Tuple`/
//! `SIMD`** (construction, indexing, lane reads *and writes* through places); member/
//! index writes through places; **value-parameterized generics** (structs reify
//! `value_params`, functions bind them as frame locals); default/keyword/`*args`
//! argument matching; **exceptions** (`try`/`except`/`else`/`finally`, incl. a
//! `return` crossing the boundary); and the scalar/utility built-ins. Not yet (clean
//! `Unsupported`, never a wrong answer): `break`/`continue` crossing a `try`, methods
//! with keyword/default/variadic args, and nested `def`/`struct`.

use super::Backend;
use crate::ast::{ArgConvention, Expr, ExprKind, ParamKind, PrefixOp, Stmt, StmtKind, Type};
use crate::checker::{ArgSlot, match_call_slots};
use crate::error::RuntimeError;
use crate::runtime::{
    Value, apply_infix, apply_list_method, apply_prefix, builtin_abs, builtin_convert,
    builtin_error, builtin_min_max, builtin_round, coerce, is_list_mutator, list_query,
    promote_numeric_elems, read_simd_lane, simd_from_values, value_as_index,
};
use crate::hir::VarId;
use crate::mir::{Const, MirBlock, MirInstr, MirPlace, MirProgram, MirTerm, Proj};
use std::collections::HashMap;

/// The control-flow outcome of executing an instruction or a `try` sub-region.
/// Most execution is `Normal`; a `return` that crosses a `try` boundary surfaces
/// as `Return`, so `finally` can run before control leaves the function. (A
/// `raise` propagates separately as `RuntimeError::Raised`; `break`/`continue`
/// crossing a `try` are refused at lowering — the mini-CFG region can't name the
/// outer loop's target block.)
enum Flow {
    Normal,
    Return(Value),
    /// A `break`/`continue` that crossed a `try` boundary, already resolved to the
    /// target loop block in the enclosing **function** CFG. Propagates out of the
    /// `try` (running each `finally`) until the function driver jumps there.
    Jump(usize),
}

/// A struct type's runtime shape, gathered from the program AST (the MIR doesn't
/// keep field layout): field names + types (for constructor coercion), and which
/// methods take `mut self` (so their receiver is written back).
struct StructDef {
    fields: Vec<(String, Type)>,
    mut_self_methods: std::collections::HashSet<String>,
    fieldwise_init: bool,
    /// The struct's compile-time parameters (`[...]`), each `(name, is_value)` —
    /// `true` for a value parameter (reified onto the instance at construction),
    /// `false` for an erased type parameter. Aligns positionally with a
    /// construction's supplied parameter arguments.
    param_decls: Vec<(String, bool)>,
}

/// A free function's calling signature (the MIR doesn't keep it), for matching
/// positional + keyword arguments to parameter slots — filling defaults and
/// collecting a trailing `*args`. Covers only the *regular* parameters; `variadic`
/// is the trailing `*args` element type, if any.
struct FnSig {
    param_names: Vec<String>,
    param_types: Vec<Type>,
    /// Const-evaluated default per regular parameter (`None` = no default, or a
    /// non-constant default the VM can't fold — using such a slot errors).
    defaults: Vec<Option<Value>>,
    required: usize,
    variadic: Option<Type>,
    /// The function's compile-time parameters (`def f[...]`), each `(name,
    /// is_value)` — a value parameter is reified as a frame-local `Int` at the call.
    param_decls: Vec<(String, bool)>,
}

/// The whole program the VM executes: the lowered MIR plus the struct and
/// function-signature registries. Immutable during execution, so it threads as
/// `&Prog` beside the mutable output.
struct Prog {
    mir: MirProgram,
    structs: HashMap<String, StructDef>,
    sigs: HashMap<String, FnSig>,
}

impl Prog {
    fn index_of(&self, name: &str) -> Option<usize> {
        self.mir.functions.iter().position(|(n, _)| n == name)
    }
}

#[derive(Default)]
pub struct VmBackend {
    output: String,
    /// The final top-level (`__toplevel__`) variable values, by name — the global
    /// bindings, captured after execution for the CLI `run` dump and tests.
    bindings: Vec<(String, Value)>,
}

impl VmBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute one function, returning its return value plus the final variable
    /// slots (so a `mut self` method call can recover the mutated receiver).
    fn call_frame(
        &mut self,
        prog: &Prog,
        fidx: usize,
        args: Vec<Value>,
        value_params: &[(String, Value)],
    ) -> Result<(Value, Vec<Value>), RuntimeError> {
        let f = &prog.mir.functions[fidx].1;
        // The MIR flattens only positional arguments; a mismatched count means a
        // default/keyword/`*args` form (not lowered yet) — refuse rather than
        // silently bind `None`.
        if args.len() != f.n_params {
            return Err(RuntimeError::Unsupported(format!(
                "vm backend does not support default/keyword/variadic arguments yet \
                 (call passed {} args to a {}-parameter function)",
                args.len(),
                f.n_params
            )));
        }
        let mut regs = vec![Value::None; f.n_regs as usize];
        let mut vars = vec![Value::None; f.n_vars];
        for (i, arg) in args.into_iter().enumerate() {
            vars[i] = match f.param_types.get(i) {
                Some(t) => coerce(arg, t),
                None => arg,
            };
        }
        // Bind reified value parameters (a value-parameterized generic function's
        // comptime `Int` params) into their body var slots, resolved by name — the
        // body reads them as ordinary `Int` locals (`return n * 2`).
        for (pname, val) in value_params {
            if let Some(slot) = f.var_names.iter().position(|n| n == pname) {
                vars[slot] = val.clone();
            }
        }

        let mut block = 0usize;
        let ret = 'run: loop {
            let b = &f.blocks[block];
            for instr in &b.instrs {
                // A `return`/`break`/`continue` that crossed a `try` boundary
                // surfaces here from the `try` instruction: a `Return` ends the
                // function; a `Jump` targets an enclosing loop block, so continue
                // there (skipping the rest of this block).
                match self.exec_instr(prog, instr, &mut regs, &mut vars)? {
                    Flow::Normal => {}
                    Flow::Return(v) => break 'run v,
                    Flow::Jump(target) => {
                        block = target;
                        continue 'run;
                    }
                }
            }
            match &b.term {
                MirTerm::Jump(t) => block = *t,
                MirTerm::Branch { cond, then_b, else_b } => {
                    block = if is_true(&regs[cond.0 as usize]) {
                        *then_b
                    } else {
                        *else_b
                    };
                }
                MirTerm::Return(r) => {
                    break 'run r
                        .as_ref()
                        .map(|r| regs[r.0 as usize].clone())
                        .unwrap_or(Value::None);
                }
                // A function body never ends in `FallOff`/`EscapeJump` (only `try`
                // regions do); treat them defensively as `return None`.
                MirTerm::FallOff | MirTerm::EscapeJump { .. } => break 'run Value::None,
            }
        };
        Ok((ret, vars))
    }

    /// Execute a function for its return value only. `value_params` reifies a
    /// value-parameterized generic function's comptime arguments (empty otherwise).
    fn call_function(
        &mut self,
        prog: &Prog,
        fidx: usize,
        args: Vec<Value>,
        value_params: &[(String, Value)],
    ) -> Result<Value, RuntimeError> {
        Ok(self.call_frame(prog, fidx, args, value_params)?.0)
    }

    /// Call a free function that has `mut`/`ref` parameters, writing each one's
    /// final value back to the caller's argument place (`arg_places`). This is the
    /// runtime half of reference parameters — the tree-walker's `eval_call`
    /// write-back, done over the caller's frame (`regs`/`vars`).
    #[allow(clippy::too_many_arguments)]
    fn call_with_writeback(
        &mut self,
        prog: &Prog,
        name: &str,
        idx: usize,
        argv: Vec<Value>,
        kwargs: Vec<(String, Value)>,
        arg_places: &[Option<MirPlace>],
        regs: &[Value],
        vars: &mut [Value],
    ) -> Result<Value, RuntimeError> {
        // Order the arguments into parameter slots (filling defaults/keywords),
        // keeping the slot map so each parameter's source argument is known.
        let (bound, slots) = match prog.sigs.get(name) {
            Some(sig) => bind_args(name, sig, argv, kwargs)?,
            None => {
                let slots = (0..argv.len()).map(ArgSlot::Positional).collect();
                (argv, slots)
            }
        };
        let (ret, frame_vars) = self.call_frame(prog, idx, bound, &[])?;
        let ref_params = prog.mir.functions[idx].1.ref_params.clone();
        for (i, is_ref) in ref_params.iter().enumerate() {
            if !is_ref {
                continue;
            }
            // The reference parameter's caller place: it must have been supplied by
            // a positional argument that is a simple place.
            let place = match slots.get(i) {
                Some(ArgSlot::Positional(p)) => arg_places.get(*p).and_then(|o| o.as_ref()),
                _ => None,
            };
            match place {
                Some(place) => *nav_mut(vars, regs, place)? = frame_vars[i].clone(),
                None => {
                    return Err(RuntimeError::Unsupported(format!(
                        "vm: a mut/ref argument to '{name}' must be a plain variable or field \
                         (not a temporary, an indexed place, or a keyword/default argument)"
                    )));
                }
            }
        }
        Ok(ret)
    }

    /// Execute one straight-line MIR instruction against the current frame.
    /// Returns the control-flow outcome — `Normal`, or `Return` when a `return`
    /// inside a nested `try` region crosses out (all other instructions are
    /// `Normal`).
    fn exec_instr(
        &mut self,
        prog: &Prog,
        i: &MirInstr,
        regs: &mut [Value],
        vars: &mut [Value],
    ) -> Result<Flow, RuntimeError> {
        match i {
            MirInstr::Const { dest, k } => regs[dest.0 as usize] = const_value(k),
            MirInstr::UseVar { dest, var, mode } => {
                let slot = *var as usize;
                // A `^` move **transfers** the value out of the source slot, leaving
                // a `Moved` tombstone; any other use copies. Either way, touching an
                // already-moved slot is a use-after-move — a loud runtime error (the
                // ownership analysis rejects this statically, so this only fires on a
                // compiler bug).
                let value = match mode {
                    crate::mir::UseMode::Move => std::mem::replace(&mut vars[slot], Value::Moved),
                    _ => vars[slot].clone(),
                };
                if matches!(value, Value::Moved) {
                    return Err(RuntimeError::TypeError(format!(
                        "vm: use of variable slot {slot} after it was moved"
                    )));
                }
                regs[dest.0 as usize] = value;
            }
            MirInstr::DefVar { var, src, ty } => {
                let v = regs[src.0 as usize].clone();
                let slot = *var as usize;
                vars[slot] = match ty {
                    Some(t) => coerce(v, t),
                    None => crate::runtime::coerce_like(v, &vars[slot]),
                };
            }
            MirInstr::UnOp { op, dest, a } => {
                regs[dest.0 as usize] = apply_prefix(*op, regs[a.0 as usize].clone())?;
            }
            MirInstr::BinOp { op, dest, a, b } => {
                let l = regs[a.0 as usize].clone();
                let r = regs[b.0 as usize].clone();
                regs[dest.0 as usize] = apply_infix(*op, l, r)?;
            }
            MirInstr::Call { dest, func, args, kwargs, arg_places, param_arg_regs } => {
                let argv: Vec<Value> = args.iter().map(|r| regs[r.0 as usize].clone()).collect();
                let kw: Vec<(String, Value)> = kwargs
                    .iter()
                    .map(|(n, r)| (n.clone(), regs[r.0 as usize].clone()))
                    .collect();
                // The supplied compile-time value-parameter arguments (a type
                // parameter is `None`), used to reify a constructed struct's
                // `value_params`.
                let pvals: Vec<Option<Value>> =
                    param_arg_regs.iter().map(|o| o.map(|r| regs[r.0 as usize].clone())).collect();
                // A free function with `mut`/`ref` parameters writes each one's final
                // value back to the caller's argument place after the call.
                let writeback = prog
                    .index_of(&func.0)
                    .filter(|&idx| prog.mir.functions[idx].1.ref_params.iter().any(|&r| r));
                let result = match writeback {
                    Some(idx) => {
                        self.call_with_writeback(prog, &func.0, idx, argv, kw, arg_places, regs, vars)?
                    }
                    None => self.call_named(prog, &func.0, argv, kw, &pvals)?,
                };
                regs[dest.0 as usize] = result;
            }
            MirInstr::MethodCall { dest, recv, method, args, recv_place, arg_places } => {
                let recv_val = regs[recv.0 as usize].clone();
                let argv: Vec<Value> = args.iter().map(|r| regs[r.0 as usize].clone()).collect();
                regs[dest.0 as usize] =
                    self.method_call(prog, recv_val, method, argv, recv_place, arg_places, regs, vars)?;
            }
            MirInstr::GetField { dest, base, field } => {
                regs[dest.0 as usize] = get_field(&regs[base.0 as usize], field)?;
            }
            MirInstr::Index { dest, base, index } => {
                let idx = value_as_index(&regs[index.0 as usize])?;
                regs[dest.0 as usize] = index_value(&regs[base.0 as usize], idx)?;
            }
            MirInstr::MakeList { dest, elems } => {
                let mut items: Vec<Value> = elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                promote_numeric_elems(&mut items); // unify literal element kinds
                regs[dest.0 as usize] = Value::List(items);
            }
            MirInstr::MakeTuple { dest, elems } => {
                let items = elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                regs[dest.0 as usize] = Value::Tuple(items);
            }
            MirInstr::MakeSimd { dest, dtype, width, elems } => {
                let vals: Vec<Value> = elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                regs[dest.0 as usize] = simd_from_values(*dtype, *width, &vals)?;
            }
            MirInstr::Store { place, src } => {
                let v = regs[src.0 as usize].clone();
                store_place(vars, regs, place, v)?;
            }
            MirInstr::LoadPlace { dest, place } => {
                regs[dest.0 as usize] = load_place(vars, regs, place)?;
            }
            MirInstr::MovePlace { dest, place } => {
                // A partial move `p.a^`: transfer the field's value out, leaving a
                // `Moved` tombstone so a later drop of the whole struct skips it (no
                // double-drop) and any stray use fails loudly. The ownership analysis
                // has already proven the moved field is not read again.
                let slot = nav_mut(vars, regs, place)?;
                let value = std::mem::replace(slot, Value::Moved);
                if matches!(value, Value::Moved) {
                    return Err(RuntimeError::TypeError(
                        "vm: partial use of an already-moved place".into(),
                    ));
                }
                regs[dest.0 as usize] = value;
            }
            // Iterator protocol (`for`). Range: counter with step direction. List:
            // consume the (copied) list from the front, preserving order.
            MirInstr::HasNext { dest, iter } => {
                let has = match &vars[*iter as usize] {
                    Value::Range { start, stop, step } => {
                        (*step > 0 && start < stop) || (*step < 0 && start > stop)
                    }
                    Value::List(items) => !items.is_empty(),
                    other => {
                        return Err(RuntimeError::Unsupported(format!(
                            "vm: `for` iterator must be a range or List, got {}",
                            crate::runtime::type_name(other)
                        )));
                    }
                };
                regs[dest.0 as usize] = Value::Bool(has);
            }
            MirInstr::Next { dest, iter } => {
                let slot = *iter as usize;
                match &mut vars[slot] {
                    Value::Range { start, stop, step } => {
                        let (cur, st, sp) = (*start, *stop, *step);
                        regs[dest.0 as usize] = Value::Int(cur);
                        vars[slot] = Value::Range { start: cur + sp, stop: st, step: sp };
                    }
                    Value::List(items) => {
                        regs[dest.0 as usize] = items.remove(0);
                    }
                    ref other => {
                        return Err(RuntimeError::Unsupported(format!(
                            "vm: `for` iterator must be a range or List, got {}",
                            crate::runtime::type_name(other)
                        )));
                    }
                }
            }
            // ASAP destruction (Phase 4): drop the value at the variable's last
            // use, running its `__del__` if it has one.
            MirInstr::DropVar { var } => {
                let v = std::mem::replace(&mut vars[*var as usize], Value::None);
                self.drop_value(prog, v)?;
            }
            MirInstr::Unsupported(what) => {
                return Err(RuntimeError::Unsupported(format!(
                    "vm backend does not support {what} yet"
                )));
            }
            MirInstr::Raise { src } => {
                // Raise an error, propagating as `Raised` — the nearest enclosing
                // `Try` (if any) intercepts it; otherwise it unwinds the frame.
                let msg = match &regs[src.0 as usize] {
                    Value::Error(s) | Value::Str(s) => s.clone(),
                    other => crate::runtime::type_name(other).to_string(),
                };
                return Err(RuntimeError::Raised(msg));
            }
            MirInstr::Try { body, handler, orelse, finalbody, cleanup } => {
                // A `try` may complete with a `return` that crossed its boundary;
                // propagate that outcome to the block driver.
                return self.exec_try(prog, body, handler, orelse, finalbody, cleanup, regs, vars);
            }
            MirInstr::Drop { .. } => {
                return Err(RuntimeError::Unsupported(format!(
                    "vm backend does not support this operation yet: {i:?}"
                )));
            }
        }
        Ok(Flow::Normal)
    }

    /// Execute a `try`/`except`/`else`/`finally` region (mirroring the tree-walker's
    /// `exec_try`). Each sub-part runs as a mini-CFG in the current frame; a raise in
    /// the body unwinds to `handler` (after running the `cleanup` drops), `else` runs
    /// on normal completion, and `finally` always runs (its raise wins).
    #[allow(clippy::too_many_arguments)]
    fn exec_try(
        &mut self,
        prog: &Prog,
        body: &[MirBlock],
        handler: &Option<(Option<VarId>, Vec<MirBlock>)>,
        orelse: &Option<Vec<MirBlock>>,
        finalbody: &Option<Vec<MirBlock>>,
        cleanup: &[VarId],
        regs: &mut [Value],
        vars: &mut [Value],
    ) -> Result<Flow, RuntimeError> {
        let outcome = match self.run_region(prog, body, regs, vars) {
            // The body raised: run the exceptional-edge cleanup (destroy the body's
            // locals as they go out of scope), then dispatch to the handler or
            // re-propagate.
            Err(RuntimeError::Raised(msg)) => {
                self.run_cleanup(prog, cleanup, vars)?;
                match handler {
                    Some((err_slot, hblocks)) => {
                        if let Some(slot) = err_slot {
                            vars[*slot as usize] = Value::Error(msg);
                        }
                        self.run_region(prog, hblocks, regs, vars)
                    }
                    None => Err(RuntimeError::Raised(msg)),
                }
            }
            // A non-raised runtime error propagates untouched.
            Err(other) => Err(other),
            // The body completed (normally, or via a `return` that crossed out): its
            // locals go out of scope here too. `else` runs only on *normal*
            // completion; a `return` from the body skips `else` and carries out.
            Ok(flow) => {
                self.run_cleanup(prog, cleanup, vars)?;
                match flow {
                    Flow::Normal => match orelse {
                        Some(eblocks) => self.run_region(prog, eblocks, regs, vars),
                        None => Ok(Flow::Normal),
                    },
                    ret => Ok(ret),
                }
            }
        };
        // `finally` always runs; if it raises or itself transfers control
        // (`return`/`break`/`continue`), that outcome wins over the pending one
        // (Python/Mojo semantics).
        if let Some(fblocks) = finalbody {
            match self.run_region(prog, fblocks, regs, vars)? {
                Flow::Normal => {}
                non_normal => return Ok(non_normal),
            }
        }
        outcome
    }

    /// Destroy a `try` body's local variables as they leave scope — a `DropVar` on
    /// each cleanup slot (a no-op on an already-emptied/`None` slot, so it is safe
    /// whether or not the value was already dropped before a raise).
    fn run_cleanup(
        &mut self,
        prog: &Prog,
        cleanup: &[VarId],
        vars: &mut [Value],
    ) -> Result<(), RuntimeError> {
        for &v in cleanup {
            let old = std::mem::replace(&mut vars[v as usize], Value::None);
            self.drop_value(prog, old)?;
        }
        Ok(())
    }

    /// Run a `try` sub-region's mini-CFG (block ids local, entry = 0) in the current
    /// frame. Returns the control-flow outcome — `Flow::Normal` on normal completion
    /// (`FallOff`), or `Flow::Return` when a `return` inside the region crosses out —
    /// and propagates a raise as `RuntimeError::Raised`. (`break`/`continue` crossing
    /// the region are refused at lowering, so no such terminator reaches here.)
    fn run_region(
        &mut self,
        prog: &Prog,
        blocks: &[MirBlock],
        regs: &mut [Value],
        vars: &mut [Value],
    ) -> Result<Flow, RuntimeError> {
        let mut block = 0usize;
        loop {
            let b = &blocks[block];
            for instr in &b.instrs {
                // A non-`Normal` outcome from a nested `try` (a `return`, or a
                // `break`/`continue` escaping to an outer loop) leaves this region
                // carrying that outcome.
                match self.exec_instr(prog, instr, regs, vars)? {
                    Flow::Normal => {}
                    non_normal => return Ok(non_normal),
                }
            }
            match &b.term {
                MirTerm::Jump(t) => block = *t,
                MirTerm::Branch { cond, then_b, else_b } => {
                    block = if is_true(&regs[cond.0 as usize]) { *then_b } else { *else_b };
                }
                MirTerm::Return(r) => {
                    let v = r.as_ref().map(|r| regs[r.0 as usize].clone()).unwrap_or(Value::None);
                    return Ok(Flow::Return(v));
                }
                MirTerm::FallOff => return Ok(Flow::Normal),
                // A `break`/`continue` targeting an outer function loop: run this
                // region's escape-edge cleanup (values that die leaving the region),
                // then carry the resolved target out as a `Flow::Jump`.
                MirTerm::EscapeJump { target, cleanup } => {
                    self.run_cleanup(prog, cleanup, vars)?;
                    return Ok(Flow::Jump(*target));
                }
            }
        }
    }

    /// Dispatch a method call. `List` methods run in place (mutators via the
    /// receiver place, queries on the value); struct methods resolve to the
    /// mangled `Type.method` function, with a `mut self` receiver written back.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn method_call(
        &mut self,
        prog: &Prog,
        recv: Value,
        method: &str,
        args: Vec<Value>,
        recv_place: &Option<MirPlace>,
        arg_places: &[Option<MirPlace>],
        regs: &[Value],
        vars: &mut [Value],
    ) -> Result<Value, RuntimeError> {
        match &recv {
            Value::List(_) if is_list_mutator(method) => {
                // Mutate the list at its place so the change persists.
                let place = recv_place.as_ref().ok_or_else(|| {
                    RuntimeError::Unsupported("vm: List mutator needs a place receiver".into())
                })?;
                match nav_mut(vars, regs, place)? {
                    Value::List(items) => apply_list_method(items, method, &args),
                    other => Err(RuntimeError::TypeError(format!(
                        "vm: List method on non-list place, got {}",
                        crate::runtime::type_name(other)
                    ))),
                }
            }
            Value::List(items) => list_query(items, method, &args),
            Value::Struct { name, .. } => {
                let fname = format!("{name}.{method}");
                let fidx = prog.index_of(&fname).ok_or_else(|| {
                    RuntimeError::Unsupported(format!("vm: unknown method '{fname}'"))
                })?;
                // Bind `self` first (parameter slot 0), then the method arguments
                // (slots 1..). Methods take only positional args (the checker defers
                // keyword/default args to methods), so the mapping is direct.
                let mut call_args = Vec::with_capacity(args.len() + 1);
                call_args.push(recv.clone());
                call_args.extend(args);
                let (ret, frame_vars) = self.call_frame(prog, fidx, call_args, &[])?;
                // Write back each `mut`/`ref` *ordinary* parameter (slot ≥ 1; slot 0
                // is `self`, handled below via `recv_place`) to its caller place,
                // reusing the free-function write-back machinery.
                let ref_params = &prog.mir.functions[fidx].1.ref_params;
                for i in 1..ref_params.len() {
                    if !ref_params[i] {
                        continue;
                    }
                    match arg_places.get(i - 1).and_then(|o| o.as_ref()) {
                        Some(place) => *nav_mut(vars, regs, place)? = frame_vars[i].clone(),
                        None => {
                            return Err(RuntimeError::Unsupported(format!(
                                "vm: a mut/ref argument to method '{fname}' must be a plain \
                                 variable or field (not a temporary or indexed place)"
                            )));
                        }
                    }
                }
                // `mut self`: write the (possibly mutated) receiver back.
                let is_mut = prog
                    .structs
                    .get(name)
                    .is_some_and(|d| d.mut_self_methods.contains(method));
                if is_mut && let Some(place) = recv_place {
                    *nav_mut(vars, regs, place)? = frame_vars[0].clone();
                }
                Ok(ret)
            }
            other => Err(RuntimeError::Unsupported(format!(
                "vm backend does not support methods on {} yet",
                crate::runtime::type_name(other)
            ))),
        }
    }

    /// Recursively destroy a value (ASAP drop): run a struct's `__del__` if it
    /// defines one, then drop its fields in reverse declaration order; `List`/
    /// `Tuple` elements likewise. A no-op for scalars and destructor-less structs.
    fn drop_value(&mut self, prog: &Prog, v: Value) -> Result<(), RuntimeError> {
        match v {
            Value::Struct { name, fields, .. } => {
                let del = format!("{name}.__del__");
                if let Some(idx) = prog.index_of(&del) {
                    // `self` is the whole struct; the return value is discarded.
                    let self_val = Value::Struct {
                        name: name.clone(),
                        fields: fields.clone(),
                        value_params: Vec::new(),
                    };
                    self.call_function(prog, idx, vec![self_val], &[])?;
                }
                for (_, fv) in fields.into_iter().rev() {
                    self.drop_value(prog, fv)?;
                }
            }
            Value::List(items) | Value::Tuple(items) => {
                for item in items.into_iter().rev() {
                    self.drop_value(prog, item)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Dispatch a call by name: a built-in intrinsic, a struct constructor, or a
    /// user function (with default/keyword/`*args` slot-matching). `param_vals`
    /// holds the supplied compile-time value-parameter arguments (`Name[...](...)`),
    /// used to reify a constructed struct's `value_params`.
    fn call_named(
        &mut self,
        prog: &Prog,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
        param_vals: &[Option<Value>],
    ) -> Result<Value, RuntimeError> {
        // Built-ins and constructors take positional arguments only (the checker
        // rejects keyword args to them).
        if !kwargs.is_empty() && !prog.sigs.contains_key(name) {
            return Err(RuntimeError::Unsupported(format!(
                "vm: keyword arguments to '{name}' are not supported"
            )));
        }
        match name {
            "print" => {
                let cells: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                self.output.push_str(&cells.join(" "));
                self.output.push('\n');
                Ok(Value::None)
            }
            "String" => Ok(Value::Str(
                args.first().map(|v| v.to_string()).unwrap_or_default(),
            )),
            "len" => match args.first() {
                Some(Value::Str(s)) => Ok(Value::Int(s.len() as i64)),
                Some(Value::List(items)) => Ok(Value::Int(items.len() as i64)),
                _ => Err(RuntimeError::Unsupported(
                    "vm: len supports String and List so far".into(),
                )),
            },
            "range" => build_range(&args),
            // Utility numeric built-ins — the same value-level semantics the
            // tree-walker uses (shared `builtin_*` helpers), so the backends agree.
            "abs" => builtin_abs(arg1(name, args)?),
            "min" => {
                let (a, b) = arg2(name, args)?;
                builtin_min_max(true, a, b)
            }
            "max" => {
                let (a, b) = arg2(name, args)?;
                builtin_min_max(false, a, b)
            }
            "round" => builtin_round(arg1(name, args)?),
            "Int" | "UInt" | "Float64" => builtin_convert(name, arg1(name, args)?),
            "Error" => builtin_error(arg1(name, args)?),
            // `List[T]()` / `List(a, b, …)` construction (the literal `[a, b]`
            // lowers to `MakeList` instead). The element type in `[T]` is dropped
            // by the MIR but the runtime list is dynamically typed, so args suffice.
            "List" => {
                let mut items = args;
                promote_numeric_elems(&mut items);
                Ok(Value::List(items))
            }
            // A struct constructor (`Point(3, 4)`).
            _ if prog.structs.contains_key(name) => {
                construct(&prog.structs[name], name, args, param_vals)
            }
            _ => match prog.index_of(name) {
                Some(idx) => {
                    // Match positional + keyword args to the parameter slots (fill
                    // defaults, collect `*args`) when a signature is known; else a
                    // plain positional call.
                    let bound = match prog.sigs.get(name) {
                        Some(sig) => bind_args(name, sig, args, kwargs)?.0,
                        None => args,
                    };
                    // Reify the function's value parameters (`doubled[21]()`): pair
                    // each declared value parameter with its supplied comptime arg.
                    let value_params: Vec<(String, Value)> = match prog.sigs.get(name) {
                        Some(sig) => sig
                            .param_decls
                            .iter()
                            .zip(param_vals)
                            .filter(|((_, is_value), _)| *is_value)
                            .map(|((pname, _), val)| (pname.clone(), val.clone().unwrap_or(Value::None)))
                            .collect(),
                        None => Vec::new(),
                    };
                    self.call_function(prog, idx, bound, &value_params)
                }
                None => Err(RuntimeError::Unsupported(format!(
                    "vm backend does not support the built-in or callee '{name}' yet"
                ))),
            },
        }
    }
}

impl Backend for VmBackend {
    fn run(&mut self, program: &[Stmt]) -> Result<(), RuntimeError> {
        let prog = Prog {
            // Elaborate ASAP drops: splice a `DropVar` after each variable's last
            // use, so a struct's `__del__` runs there (Phase 4). A no-op for values
            // without a destructor, so parity with the tree-walker is preserved.
            mir: crate::analysis::elaborate_drops_program(crate::mir::lower_program(program)),
            structs: build_structs(program),
            sigs: build_sigs(program),
        };
        // Run the top-level block, then `main()`. Capture the top-level frame's
        // user variables (skipping synthetic `$…` temporaries) as the global
        // bindings.
        if let Some(top) = prog.index_of("__toplevel__") {
            let (_, vars) = self.call_frame(&prog, top, Vec::new(), &[])?;
            let names = &prog.mir.functions[top].1.var_names;
            self.bindings = names
                .iter()
                .zip(&vars)
                .filter(|(name, _)| !name.starts_with('$'))
                .map(|(name, v)| (name.clone(), v.clone()))
                .collect();
        }
        if let Some(main) = prog.index_of("main") {
            self.call_function(&prog, main, Vec::new(), &[])?;
        }
        Ok(())
    }

    fn output(&self) -> String {
        self.output.clone()
    }

    fn bindings(&self) -> Vec<(String, Value)> {
        self.bindings.clone()
    }
}

/// Build the struct registry from the program's top-level `struct` declarations.
fn build_structs(program: &[Stmt]) -> HashMap<String, StructDef> {
    let mut map = HashMap::new();
    for s in program {
        if let StmtKind::Struct { name, type_params, fields, methods, fieldwise_init, .. } = &s.kind
        {
            let mut_self_methods = methods
                .iter()
                .filter(|m| matches!(m.self_convention, Some(ArgConvention::Mut)))
                .map(|m| m.name.clone())
                .collect();
            map.insert(
                name.clone(),
                StructDef {
                    fields: fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect(),
                    mut_self_methods,
                    fieldwise_init: *fieldwise_init,
                    param_decls: crate::runtime::classify_param_decls(type_params),
                },
            );
        }
    }
    map
}

/// Build the calling-signature registry from the program's top-level `def`s.
fn build_sigs(program: &[Stmt]) -> HashMap<String, FnSig> {
    let mut map = HashMap::new();
    for s in program {
        if let StmtKind::Def { name, type_params, params, .. } = &s.kind {
            // Split the regular parameters from a trailing `*args` (mirrors the
            // checker / evaluator).
            let nreg = params
                .iter()
                .position(|p| p.kind == ParamKind::Variadic)
                .unwrap_or(params.len());
            let regular = &params[..nreg];
            let required = regular.iter().take_while(|p| p.default.is_none()).count();
            map.insert(
                name.clone(),
                FnSig {
                    param_names: regular.iter().map(|p| p.name.clone()).collect(),
                    param_types: regular.iter().map(|p| p.ty.clone()).collect(),
                    defaults: regular
                        .iter()
                        .map(|p| p.default.as_ref().and_then(const_default))
                        .collect(),
                    required,
                    variadic: params.get(nreg).map(|p| p.ty.clone()),
                    param_decls: crate::runtime::classify_param_decls(type_params),
                },
            );
        }
    }
    map
}

/// Const-fold a default-argument expression to a value. Handles the literal forms
/// (and a unary minus over one) that defaults use in practice; a non-constant
/// default folds to `None` and errors only if that slot is actually taken.
fn const_default(e: &Expr) -> Option<Value> {
    match &e.kind {
        ExprKind::Int(n) => Some(Value::Int(*n)),
        ExprKind::Float(x) => Some(Value::Float64(*x)),
        ExprKind::Bool(b) => Some(Value::Bool(*b)),
        ExprKind::Str(s) => Some(Value::Str(s.clone())),
        ExprKind::None => Some(Value::None),
        ExprKind::Prefix(PrefixOp::Neg, inner) => match const_default(inner)? {
            Value::Int(n) => Some(Value::Int(-n)),
            Value::Float64(x) => Some(Value::Float64(-x)),
            _ => None,
        },
        _ => None,
    }
}

/// Match positional + keyword arguments to a function's parameter slots, producing
/// the ordered argument values the frame binds — filling defaults and collecting a
/// trailing `*args` into a `List`. Mirrors the tree-walker's `eval_call`.
fn bind_args(
    name: &str,
    sig: &FnSig,
    argv: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<(Vec<Value>, Vec<ArgSlot>), RuntimeError> {
    let kw_names: Vec<&str> = kwargs.iter().map(|(n, _)| n.as_str()).collect();
    let (slots, overflow) = match_call_slots(
        &sig.param_names,
        sig.param_names.len(),
        sig.required,
        argv.len(),
        &kw_names,
        sig.variadic.is_some(),
    )
    .map_err(|e| e.into_runtime_error(name))?;

    let mut out = Vec::with_capacity(slots.len() + 1);
    for (i, slot) in slots.iter().enumerate() {
        let value = match slot {
            ArgSlot::Positional(p) => argv[*p].clone(),
            ArgSlot::Keyword(k) => kwargs[*k].1.clone(),
            ArgSlot::Default => sig.defaults[i].clone().ok_or_else(|| {
                RuntimeError::Unsupported(format!(
                    "vm: non-constant default for parameter '{}' of '{name}'",
                    sig.param_names[i]
                ))
            })?,
        };
        out.push(coerce(value, &sig.param_types[i]));
    }
    // Collect overflow positional args into the `*args` list.
    if let Some(elem_ty) = &sig.variadic {
        let items = overflow
            .iter()
            .map(|&idx| coerce(argv[idx].clone(), elem_ty))
            .collect();
        out.push(Value::List(items));
    }
    Ok((out, slots))
}

/// Build a struct instance (fieldwise), coercing each argument to its field type.
fn construct(
    def: &StructDef,
    name: &str,
    args: Vec<Value>,
    param_vals: &[Option<Value>],
) -> Result<Value, RuntimeError> {
    if !def.fieldwise_init {
        return Err(RuntimeError::TypeError(format!(
            "struct '{name}' has no constructor"
        )));
    }
    if def.fields.len() != args.len() {
        return Err(RuntimeError::ArityMismatch {
            name: name.to_string(),
            expected: def.fields.len(),
            got: args.len(),
        });
    }
    let fields = def
        .fields
        .iter()
        .zip(args)
        .map(|((fname, fty), arg)| (fname.clone(), coerce(arg, fty)))
        .collect();
    // Reify the value parameters onto the instance (type parameters stay erased):
    // pair each declared value parameter with its supplied comptime `Int` argument.
    // Explicit `Name[...](...)` supplies every parameter positionally, so the decls
    // align with `param_vals`.
    let value_params = def
        .param_decls
        .iter()
        .zip(param_vals)
        .filter(|((_, is_value), _)| *is_value)
        .map(|((pname, _), val)| (pname.clone(), val.clone().unwrap_or(Value::None)))
        .collect();
    Ok(Value::Struct { name: name.to_string(), fields, value_params })
}

/// Read a struct field (or a reified value parameter, e.g. `Self.n`) by name.
fn get_field(base: &Value, field: &str) -> Result<Value, RuntimeError> {
    match base {
        Value::Struct { fields, value_params, .. } => fields
            .iter()
            .chain(value_params.iter())
            .find(|(f, _)| f == field)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| RuntimeError::TypeError(format!("no field '{field}'"))),
        other => Err(RuntimeError::TypeError(format!(
            "field access on non-struct {}",
            crate::runtime::type_name(other)
        ))),
    }
}

/// Index into a `List` or `Tuple` (an `Int` index, bounds-checked).
fn index_value(base: &Value, idx: i64) -> Result<Value, RuntimeError> {
    match base {
        Value::List(items) => {
            let i = crate::runtime::bounds_check(idx, items.len(), "list index")?;
            Ok(items[i].clone())
        }
        Value::Tuple(items) => {
            let i = crate::runtime::bounds_check(idx, items.len(), "tuple index")?;
            Ok(items[i].clone())
        }
        // A SIMD lane read returns the width-1 scalar (a width-1 `float64` lane is
        // a `Float64`, per the SIMD/Float64 unification).
        Value::Simd { dtype, lanes } => read_simd_lane(*dtype, lanes, idx),
        other => Err(RuntimeError::TypeError(format!(
            "cannot index {}",
            crate::runtime::type_name(other)
        ))),
    }
}

/// Construct a `range(...)` value (mirrors `eval_range`).
/// Take the single argument of a one-arg built-in (the checker guarantees arity;
/// a mismatch is a defensive clean error, never a panic).
fn arg1(name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
    let mut args = args;
    if args.len() != 1 {
        return Err(RuntimeError::ArityMismatch { name: name.to_string(), expected: 1, got: args.len() });
    }
    Ok(args.pop().unwrap())
}

/// Take the two arguments of a two-arg built-in (`min`/`max`).
fn arg2(name: &str, args: Vec<Value>) -> Result<(Value, Value), RuntimeError> {
    if args.len() != 2 {
        return Err(RuntimeError::ArityMismatch { name: name.to_string(), expected: 2, got: args.len() });
    }
    let mut it = args.into_iter();
    Ok((it.next().unwrap(), it.next().unwrap()))
}

fn build_range(args: &[Value]) -> Result<Value, RuntimeError> {
    let mut ints = Vec::with_capacity(args.len());
    for a in args {
        match a {
            Value::Int(n) => ints.push(*n),
            other => {
                return Err(RuntimeError::TypeError(format!(
                    "range() expects Int arguments, got {}",
                    crate::runtime::type_name(other)
                )));
            }
        }
    }
    let (start, stop, step) = match ints.as_slice() {
        [stop] => (0, *stop, 1),
        [start, stop] => (*start, *stop, 1),
        [start, stop, step] => (*start, *stop, *step),
        _ => {
            return Err(RuntimeError::TypeError(format!(
                "range() takes 1 to 3 arguments, got {}",
                ints.len()
            )));
        }
    };
    if step == 0 {
        return Err(RuntimeError::TypeError(
            "range() step must not be zero".to_string(),
        ));
    }
    Ok(Value::Range { start, stop, step })
}

/// Navigate one projection step from a container slot to an inner mutable slot.
/// A SIMD lane is *not* a `Value` slot, so it is not reachable here — callers that
/// may target a lane (`store_place`/`load_place`) special-case a final `Index`
/// into a `Value::Simd` before reaching this.
fn nav_step<'a>(
    slot: &'a mut Value,
    proj: &Proj,
    regs: &[Value],
) -> Result<&'a mut Value, RuntimeError> {
    match proj {
        Proj::Field(name) => match slot {
            // Search declared fields first, then value parameters (a `Self.n`
            // read) — mirroring `get_field`, so a place read through this matches
            // the register-based `GetField`.
            Value::Struct { fields, value_params, .. } => {
                if let Some(pos) = fields.iter().position(|(f, _)| f == name) {
                    Ok(&mut fields[pos].1)
                } else if let Some(pos) = value_params.iter().position(|(f, _)| f == name) {
                    Ok(&mut value_params[pos].1)
                } else {
                    Err(RuntimeError::TypeError(format!("no field '{name}'")))
                }
            }
            other => Err(RuntimeError::TypeError(format!(
                "field access on non-struct {}",
                crate::runtime::type_name(other)
            ))),
        },
        Proj::Index(reg) => {
            let idx = value_as_index(&regs[reg.0 as usize])?;
            match slot {
                Value::List(items) => {
                    let i = crate::runtime::bounds_check(idx, items.len(), "list index")?;
                    Ok(&mut items[i])
                }
                Value::Tuple(items) => {
                    let i = crate::runtime::bounds_check(idx, items.len(), "tuple index")?;
                    Ok(&mut items[i])
                }
                other => Err(RuntimeError::TypeError(format!(
                    "cannot index {}",
                    crate::runtime::type_name(other)
                ))),
            }
        }
    }
}

/// Navigate a [`MirPlace`] to a mutable slot: the root variable followed by field
/// and index projections. Used for method write-back and `MovePlace` (a pure
/// field chain). A SIMD lane isn't a `Value` slot; use `store_place`/`load_place`
/// for a place that may end in a lane.
fn nav_mut<'a>(
    vars: &'a mut [Value],
    regs: &[Value],
    place: &MirPlace,
) -> Result<&'a mut Value, RuntimeError> {
    let mut slot = &mut vars[place.root as usize];
    for proj in &place.proj {
        slot = nav_step(slot, proj, regs)?;
    }
    Ok(slot)
}

/// Write `value` into a place, handling a **SIMD lane** target (`v[i] = e`,
/// `obj.vec[i] = e`) — a lane isn't a `Value` slot, so it is set via
/// `set_simd_lane` (dtype wrap/round) after navigating the container.
fn store_place(
    vars: &mut [Value],
    regs: &[Value],
    place: &MirPlace,
    value: Value,
) -> Result<(), RuntimeError> {
    match place.proj.split_last() {
        None => {
            vars[place.root as usize] = value;
            Ok(())
        }
        Some((last, prefix)) => {
            let mut slot = &mut vars[place.root as usize];
            for proj in prefix {
                slot = nav_step(slot, proj, regs)?;
            }
            if let Proj::Index(ireg) = last
                && let Value::Simd { dtype, lanes } = slot
            {
                let idx = value_as_index(&regs[ireg.0 as usize])?;
                return crate::runtime::set_simd_lane(*dtype, lanes, idx, value);
            }
            *nav_step(slot, last, regs)? = value;
            Ok(())
        }
    }
}

/// Read (clone) the value at a place, handling a **SIMD lane** read (`v[i]`,
/// `obj.vec[i]`) via `read_simd_lane`.
fn load_place(vars: &mut [Value], regs: &[Value], place: &MirPlace) -> Result<Value, RuntimeError> {
    if let Some((Proj::Index(ireg), prefix)) = place.proj.split_last() {
        let mut slot = &mut vars[place.root as usize];
        for proj in prefix {
            slot = nav_step(slot, proj, regs)?;
        }
        if let Value::Simd { dtype, lanes } = slot {
            let idx = value_as_index(&regs[ireg.0 as usize])?;
            return read_simd_lane(*dtype, lanes, idx);
        }
        // Not a SIMD parent — fall through to the final index step below.
        return Ok(nav_step(slot, &Proj::Index(*ireg), regs)?.clone());
    }
    Ok(nav_mut(vars, regs, place)?.clone())
}

/// Whether a branch condition register holds `True`.
fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Materialize a MIR constant into a runtime value.
fn const_value(k: &Const) -> Value {
    match k {
        Const::Int(n) => Value::Int(*n),
        Const::Float(x) => Value::Float64(*x),
        Const::Bool(b) => Value::Bool(*b),
        Const::Str(s) => Value::Str(s.clone()),
        Const::None => Value::None,
    }
}
