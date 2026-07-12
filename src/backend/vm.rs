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
use crate::ast::{Expr, ExprKind, PrefixOp, Stmt, Type};
use crate::checker::{ArgSlot, match_call_slots};
use crate::error::RuntimeError;
use crate::hir::VarId;
use crate::mir::{Const, MirBlock, MirInstr, MirPlace, MirProgram, MirTerm, Proj, Reg};
use crate::runtime::{
    Value, apply_infix, apply_list_method, apply_prefix, builtin_abs, builtin_convert,
    builtin_divmod, builtin_error, builtin_input, builtin_min_max, builtin_round, coerce,
    is_list_mutator, list_query, promote_numeric_elems, read_simd_lane, simd_from_values,
    value_as_index,
};
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
    required: Vec<bool>,
    variadic: Option<Type>,
    /// Where the collected `*args` list belongs among source parameters. For a
    /// signature like `def f(a, *xs, b)`, this is `Some(1)`.
    variadic_index: Option<usize>,
    /// Indexes into the regular-parameter list.
    positional_only: Option<usize>,
    keyword_only: Option<usize>,
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

struct CallerFrame<'a> {
    registers: &'a mut [Value],
    variables: &'a mut [Value],
}

struct WritebackCall<'a> {
    function_name: &'a str,
    function_index: usize,
    positional_args: Vec<Value>,
    keyword_args: Vec<(String, Value)>,
    argument_places: &'a [Option<MirPlace>],
}

struct TryRegions<'a> {
    body: &'a [MirBlock],
    handler: &'a Option<(Option<VarId>, Vec<MirBlock>)>,
    orelse: &'a Option<Vec<MirBlock>>,
    finalbody: &'a Option<Vec<MirBlock>>,
    cleanup: &'a [VarId],
}

struct MethodInvocation<'a> {
    receiver: Value,
    method: &'a str,
    resolved_name: Option<&'a str>,
    arguments: Vec<Value>,
    receiver_place: &'a Option<MirPlace>,
    argument_places: &'a [Option<MirPlace>],
}

impl Prog {
    fn index_of(&self, name: &str) -> Option<usize> {
        self.mir.functions.iter().position(|(n, _)| n == name)
    }

    /// Whether any function name ends with `suffix` (e.g. `.__copyinit__`) — used to
    /// decide whether copy/move needs the lifecycle-method path at all.
    fn defines(&self, suffix: &str) -> bool {
        self.mir.functions.iter().any(|(n, _)| n.ends_with(suffix))
    }

    /// Arity-based overload fallback: resolve a *source* name to the lowered
    /// function it must mean, for the calls the checker records no per-span
    /// target for. Its callers are the VM-synthesized dispatches — operator/
    /// `__str__`/`__hash__` dunders (`call_dunder`), `__setitem__`,
    /// the `for`-loop `__next__` protocol, `__init__` construction reached
    /// without a recorded target, and `method_call` when `resolved` is absent
    /// (i.e. the method isn't overloaded). Checker-resolved calls carry their
    /// exact lowered callee and never come through here.
    fn overload_name(&self, name: &str, argc: usize) -> String {
        if self.index_of(name).is_some() {
            return name.to_string();
        }
        let expected_params = if name.contains('.') { argc + 1 } else { argc };
        let mut matches = self
            .mir
            .functions
            .iter()
            .filter(|(fname, f)| {
                crate::symbol::is_overload_of(fname, name) && f.n_params == expected_params
            })
            .map(|(fname, _)| fname.clone())
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            matches.remove(0)
        } else {
            name.to_string()
        }
    }
}

#[derive(Default)]
pub struct VmBackend {
    output: String,
    /// The final top-level (`__toplevel__`) variable values, by name — the global
    /// bindings, captured after execution for the CLI `run` dump and tests.
    bindings: Vec<(String, Value)>,
    /// The type-erased heap arena backing `UnsafePointer[T]` (Option B): an
    /// `UnsafePointer(base)` is an offset into this `Vec`. `alloc(n)` appends `n`
    /// slots and returns the base; `free()` is a no-op (a model-level arena that
    /// never reclaims — fine for a bounded interpreter).
    heap: Vec<Value>,
    /// Whether the program defines any `__copyinit__` / `__moveinit__`. When false,
    /// a value copy/move is the default (a raw deep `Clone` / a slot transfer) — the
    /// common fast path, keeping non-lifecycle programs unchanged. When true, a
    /// struct copy/move routes through its lifecycle method (`clone_value`/
    /// `move_value`), giving a pointer-owning type correct value semantics.
    has_copyinit: bool,
    has_moveinit: bool,
    /// Optional compile-time execution budget. Runtime VM execution leaves this
    /// `None`; VM-backed CTFE sets it and every function/block/instruction burns
    /// from it so compile-time execution cannot hang the compiler.
    ctfe_fuel: Option<usize>,
}

impl VmBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute a named top-level function and return its value without running the
    /// program's top-level block or `main`. This is the narrow API used by
    /// VM-backed CTFE: the caller has already checked that the function is
    /// compile-time safe and supplied any reified value parameters.
    pub fn run_function_value(
        &mut self,
        program: &[Stmt],
        name: &str,
        args: Vec<Value>,
        value_params: &[(String, Value)],
        fuel: usize,
    ) -> Result<(Value, usize), RuntimeError> {
        let prog = build_prog(program);
        self.configure_lifecycle(&prog);
        let idx = prog.index_of(name).ok_or_else(|| {
            RuntimeError::Unsupported(format!("vm: unknown compile-time function '{name}'"))
        })?;
        self.ctfe_fuel = Some(fuel);
        let result = self.call_function(&prog, idx, args, value_params);
        let remaining = self.ctfe_fuel.unwrap_or(0);
        self.ctfe_fuel = None;
        result.map(|value| (value, remaining))
    }

    fn configure_lifecycle(&mut self, prog: &Prog) {
        // A program with no lifecycle copy/move methods uses the default (raw clone /
        // slot transfer) path everywhere — so non-lifecycle programs are unchanged.
        self.has_copyinit = prog.defines(".__copyinit__");
        self.has_moveinit = prog.defines(".__moveinit__");
    }

    fn burn_ctfe(&mut self) -> Result<(), RuntimeError> {
        if let Some(fuel) = &mut self.ctfe_fuel {
            *fuel = fuel.checked_sub(1).ok_or_else(|| {
                RuntimeError::Unsupported(
                    "compile-time execution exceeded the VM CTFE fuel quota".to_string(),
                )
            })?;
        }
        Ok(())
    }

    /// Allocate `n` uninitialized (`None`) slots in the heap arena, returning a
    /// pointer to the base. A negative/absurd count is a runtime error.
    fn heap_alloc(&mut self, n: i64) -> Result<Value, RuntimeError> {
        if n < 0 {
            return Err(RuntimeError::TypeError(
                "vm: UnsafePointer.alloc count must be non-negative".to_string(),
            ));
        }
        let base = self.heap.len();
        self.heap.resize(base + n as usize, Value::None);
        Ok(Value::Pointer(base))
    }

    /// Resolve `base + offset` to an arena index, bounds-checking against the arena
    /// (a truly out-of-arena access errors rather than panicking; an in-arena but
    /// past-allocation access is permitted — `UnsafePointer` is unchecked).
    fn heap_index(&self, base: usize, offset: i64) -> Result<usize, RuntimeError> {
        let i = base as i64 + offset;
        if i < 0 || i as usize >= self.heap.len() {
            return Err(RuntimeError::TypeError(
                "vm: UnsafePointer access out of bounds".to_string(),
            ));
        }
        Ok(i as usize)
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
        self.burn_ctfe()?;
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
            self.burn_ctfe()?;
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
                MirTerm::Branch {
                    cond,
                    then_b,
                    else_b,
                } => {
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

    /// Call a struct dunder `Type.method(args…)` (`args[0]` is the receiver). The
    /// checker has already verified the method exists and its argument types, so a
    /// missing method here is a compiler bug (reported cleanly rather than a panic).
    fn call_dunder(
        &mut self,
        prog: &Prog,
        sname: &str,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let source_fname = format!("{sname}.{method}");
        let fname = prog.overload_name(&source_fname, args.len().saturating_sub(1));
        let idx = prog.index_of(&fname).ok_or_else(|| {
            RuntimeError::Unsupported(format!("vm: struct '{sname}' has no method '{method}'"))
        })?;
        self.call_function(prog, idx, args, &[])
    }

    /// Apply a binary operator, dispatching to a user struct's **dunder** when an
    /// operand is a struct (operator overloading): `a OP b` → `a.__op__(b)` for a
    /// struct left operand; `x in c` / `x not in c` → `c.__contains__(x)` (negated
    /// for `not in`). Primitive operands go through the shared `apply_infix`.
    fn apply_binop(
        &mut self,
        prog: &Prog,
        op: crate::ast::InfixOp,
        l: Value,
        r: Value,
    ) -> Result<Value, RuntimeError> {
        use crate::ast::InfixOp;
        if matches!(op, InfixOp::In | InfixOp::NotIn) {
            if let Value::Struct { name, .. } = &r {
                let sname = name.clone();
                let res = self.call_dunder(prog, &sname, "__contains__", vec![r, l])?;
                return Ok(match (op, res) {
                    (InfixOp::NotIn, Value::Bool(b)) => Value::Bool(!b),
                    (_, v) => v,
                });
            }
        } else if let Value::Struct { name, .. } = &l
            && let Some(dunder) = op.dunder()
        {
            let sname = name.clone();
            return self.call_dunder(prog, &sname, dunder, vec![l, r]);
        }
        apply_infix(op, l, r)
    }

    /// `c[i] = value` where the container `c` (at `parent`) is a user struct →
    /// `c.__setitem__(i, value)`, writing the mutated `self` back to `c`'s place.
    /// The MIR has already evaluated the receiver root, index, and RHS exactly once;
    /// this clones the receiver, runs the `mut self` method, and stores its resulting
    /// `self` (frame slot 0) back — the same write-back a normal `mut self` call uses.
    fn store_index_dunder(
        &mut self,
        prog: &Prog,
        parent: &MirPlace,
        idx: Value,
        value: Value,
        regs: &[Value],
        vars: &mut [Value],
    ) -> Result<(), RuntimeError> {
        let recv = nav_mut(vars, regs, parent)?.clone();
        let Value::Struct { name, .. } = &recv else {
            unreachable!("store_index_dunder is only called on a struct container");
        };
        let fname = prog.overload_name(&format!("{name}.__setitem__"), 2);
        let fidx = prog.index_of(&fname).ok_or_else(|| {
            RuntimeError::Unsupported(format!("vm: struct '{name}' has no method '__setitem__'"))
        })?;
        let (_, frame_vars) = self.call_frame(prog, fidx, vec![recv, idx, value], &[])?;
        *nav_mut(vars, regs, parent)? = frame_vars.into_iter().next().unwrap_or(Value::None);
        Ok(())
    }

    /// Construct a struct via a hand-written `def __init__(out self, …)`: build an
    /// uninitialized `self` skeleton (fields = `None` placeholders, value parameters
    /// reified), run `__init__(self, args…)`, and return the initialized `self`
    /// (frame slot 0). The checker's definite-init check guarantees every field is
    /// assigned in the body, so no placeholder survives. Arguments are coerced to the
    /// `__init__` parameter types by the normal call ABI.
    fn construct_via_init(
        &mut self,
        prog: &Prog,
        name: &str,
        target: Option<&str>,
        args: Vec<Value>,
        param_vals: &[Option<Value>],
    ) -> Result<Value, RuntimeError> {
        let def = &prog.structs[name];
        let fields = def
            .fields
            .iter()
            .map(|(f, _)| (f.clone(), Value::None))
            .collect();
        let value_params = def
            .param_decls
            .iter()
            .zip(param_vals)
            .filter(|((_, is_value), _)| *is_value)
            .map(|((pname, _), val)| (pname.clone(), val.clone().unwrap_or(Value::None)))
            .collect();
        let skeleton = Value::Struct {
            name: name.to_string(),
            fields,
            value_params,
        };
        let fidx = prog
            .index_of(
                &target
                    .map(str::to_string)
                    .unwrap_or_else(|| prog.overload_name(&format!("{name}.__init__"), args.len())),
            )
            .expect("caller checked __init__ is present");
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(skeleton);
        call_args.extend(args);
        let (_, frame_vars) = self.call_frame(prog, fidx, call_args, &[])?;
        Ok(frame_vars.into_iter().next().unwrap_or(Value::None))
    }

    fn construct_via_copy(
        &mut self,
        prog: &Prog,
        name: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
        param_vals: &[Option<Value>],
    ) -> Result<Value, RuntimeError> {
        if !args.is_empty() || kwargs.len() != 1 || kwargs[0].0 != "copy" {
            return Err(RuntimeError::Unsupported(format!(
                "vm: keyword arguments to '{name}' are not supported"
            )));
        }
        let fidx = prog
            .index_of(&format!("{name}.__copyinit__"))
            .ok_or_else(|| {
                RuntimeError::Unsupported(format!("vm: struct '{name}' has no copy constructor"))
            })?;
        let def = &prog.structs[name];
        let value_params = def
            .param_decls
            .iter()
            .zip(param_vals)
            .filter(|((_, is_value), _)| *is_value)
            .map(|((pname, _), val)| (pname.clone(), val.clone().unwrap_or(Value::None)))
            .collect();
        let skeleton = self.struct_skeleton(prog, name, value_params);
        let (_, frame_vars) =
            self.call_frame(prog, fidx, vec![skeleton, kwargs[0].1.clone()], &[])?;
        Ok(frame_vars.into_iter().next().unwrap_or(Value::None))
    }

    /// Produce a semantically-correct **copy** of a value (a `UseVar { Copy }` read,
    /// a by-value argument, or a return). For a struct that defines `__copyinit__`,
    /// run it (so a pointer-owning type deep-copies its storage instead of aliasing);
    /// for a struct without one, recurse into fields (a nested field may define it);
    /// `List`/`Tuple` recurse element-wise. Only reached when `has_copyinit` is set.
    fn clone_value(&mut self, prog: &Prog, v: &Value) -> Result<Value, RuntimeError> {
        match v {
            Value::Struct {
                name,
                fields,
                value_params,
            } => {
                if let Some(fidx) = prog.index_of(&format!("{name}.__copyinit__")) {
                    let skeleton = self.struct_skeleton(prog, name, value_params.clone());
                    let (_, frame_vars) =
                        self.call_frame(prog, fidx, vec![skeleton, v.clone()], &[])?;
                    Ok(frame_vars.into_iter().next().unwrap_or(Value::None))
                } else {
                    let mut new_fields = Vec::with_capacity(fields.len());
                    for (f, fv) in fields {
                        new_fields.push((f.clone(), self.clone_value(prog, fv)?));
                    }
                    Ok(Value::Struct {
                        name: name.clone(),
                        fields: new_fields,
                        value_params: value_params.clone(),
                    })
                }
            }
            Value::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.clone_value(prog, it)?);
                }
                Ok(Value::List(out))
            }
            Value::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.clone_value(prog, it)?);
                }
                Ok(Value::Tuple(out))
            }
            // Scalars alias/copy trivially; a bare pointer copy *aliases* (correct —
            // deep-copy is the owning struct's `__copyinit__` job, handled above).
            other => Ok(other.clone()),
        }
    }

    /// Relocate a **moved** value (a `UseVar { Move }` / `^` transfer). For a struct
    /// that defines `__moveinit__`, run it (`existing` is consumed); otherwise the
    /// default move — the value's slot was already tombstoned — suffices. Only
    /// reached when `has_moveinit` is set.
    fn move_value(&mut self, prog: &Prog, v: Value) -> Result<Value, RuntimeError> {
        if let Value::Struct {
            name, value_params, ..
        } = &v
            && let Some(fidx) = prog.index_of(&format!("{name}.__moveinit__"))
        {
            let skeleton = self.struct_skeleton(prog, name, value_params.clone());
            let (_, frame_vars) = self.call_frame(prog, fidx, vec![skeleton, v], &[])?;
            return Ok(frame_vars.into_iter().next().unwrap_or(Value::None));
        }
        Ok(v)
    }

    /// Build an uninitialized `self` skeleton for `name` (fields = `None`), carrying
    /// the given reified `value_params`. Shared by `__init__`/`__copyinit__`/
    /// `__moveinit__` construction.
    fn struct_skeleton(
        &self,
        prog: &Prog,
        name: &str,
        value_params: Vec<(String, Value)>,
    ) -> Value {
        let fields = prog.structs[name]
            .fields
            .iter()
            .map(|(f, _)| (f.clone(), Value::None))
            .collect();
        Value::Struct {
            name: name.to_string(),
            fields,
            value_params,
        }
    }

    /// If `place` is `c[i]` with `c` a user struct or an `UnsafePointer`, read it via
    /// `c.__getitem__(i)` / the heap arena — the read half of `c[i] += e` on such a
    /// container (a projected `LoadPlace`). Returns `None` otherwise, so the caller
    /// uses `load_place` (a slot read or a SIMD-lane read).
    fn load_index_dunder(
        &mut self,
        prog: &Prog,
        place: &MirPlace,
        regs: &[Value],
        vars: &mut [Value],
    ) -> Result<Option<Value>, RuntimeError> {
        let Some((Proj::Index(ireg), prefix)) = place.proj.split_last() else {
            return Ok(None);
        };
        let parent = MirPlace {
            root: place.root,
            proj: prefix.to_vec(),
        };
        let recv = nav_mut(vars, regs, &parent)?.clone();
        match &recv {
            Value::Struct { name, .. } => {
                let sname = name.clone();
                let idx = regs[ireg.0 as usize].clone();
                Ok(Some(self.call_dunder(
                    prog,
                    &sname,
                    "__getitem__",
                    vec![recv, idx],
                )?))
            }
            Value::Pointer(b) => {
                let off = value_as_index(&regs[ireg.0 as usize])?;
                let i = self.heap_index(*b, off)?;
                Ok(Some(self.heap[i].clone()))
            }
            _ => Ok(None),
        }
    }

    /// Call a free function that has `mut`/`ref` parameters, writing each one's
    /// final value back to the caller's argument place (`arg_places`). This is the
    /// runtime half of reference parameters — the tree-walker's `eval_call`
    /// write-back, done over the caller's frame (`regs`/`vars`).
    fn call_with_writeback(
        &mut self,
        prog: &Prog,
        call: WritebackCall<'_>,
        frame: CallerFrame<'_>,
    ) -> Result<Value, RuntimeError> {
        let WritebackCall {
            function_name: name,
            function_index: idx,
            positional_args: argv,
            keyword_args: kwargs,
            argument_places: arg_places,
        } = call;
        let CallerFrame {
            registers: regs,
            variables: vars,
        } = frame;
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
        self.burn_ctfe()?;
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
                    crate::mir::UseMode::Move => {
                        let moved = std::mem::replace(&mut vars[slot], Value::Moved);
                        if self.has_moveinit {
                            self.move_value(prog, moved)?
                        } else {
                            moved
                        }
                    }
                    // A copy runs `__copyinit__` (deep copy) for a lifecycle type;
                    // otherwise a plain deep `Clone`.
                    _ if self.has_copyinit => self.clone_value(prog, &vars[slot])?,
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
                regs[dest.0 as usize] = self.apply_binop(prog, *op, l, r)?;
            }
            MirInstr::Call {
                dest,
                func,
                args,
                kwargs,
                arg_places,
                param_arg_regs,
            } => {
                let argv: Vec<Value> = args.iter().map(|r| regs[r.0 as usize].clone()).collect();
                let kw: Vec<(String, Value)> = kwargs
                    .iter()
                    .map(|(n, r)| (n.clone(), regs[r.0 as usize].clone()))
                    .collect();
                // The supplied compile-time value-parameter arguments (a type
                // parameter is `None`), used to reify a constructed struct's
                // `value_params`.
                let pvals: Vec<Option<Value>> = param_arg_regs
                    .iter()
                    .map(|o| o.map(|r| regs[r.0 as usize].clone()))
                    .collect();
                // A free function with `mut`/`ref` parameters writes each one's final
                // value back to the caller's argument place after the call.
                let writeback = prog
                    .index_of(&func.0)
                    .filter(|&idx| prog.mir.functions[idx].1.ref_params.iter().any(|&r| r));
                let result = match writeback {
                    Some(idx) => self.call_with_writeback(
                        prog,
                        WritebackCall {
                            function_name: &func.0,
                            function_index: idx,
                            positional_args: argv,
                            keyword_args: kw,
                            argument_places: arg_places,
                        },
                        CallerFrame {
                            registers: regs,
                            variables: vars,
                        },
                    )?,
                    None => self.call_named(prog, &func.0, argv, kw, &pvals)?,
                };
                regs[dest.0 as usize] = result;
            }
            MirInstr::MethodCall {
                dest,
                recv,
                method,
                resolved,
                args,
                recv_place,
                arg_places,
            } => {
                let recv_val = regs[recv.0 as usize].clone();
                let argv: Vec<Value> = args.iter().map(|r| regs[r.0 as usize].clone()).collect();
                regs[dest.0 as usize] = self.method_call(
                    prog,
                    MethodInvocation {
                        receiver: recv_val,
                        method,
                        resolved_name: resolved.as_deref(),
                        arguments: argv,
                        receiver_place: recv_place,
                        argument_places: arg_places,
                    },
                    CallerFrame {
                        registers: regs,
                        variables: vars,
                    },
                )?;
            }
            MirInstr::GetField { dest, base, field } => {
                regs[dest.0 as usize] = get_field(&regs[base.0 as usize], field)?;
            }
            MirInstr::Index { dest, base, index } => {
                match &regs[base.0 as usize] {
                    // A user struct with `__getitem__` is subscriptable: `c[i]` →
                    // `c.__getitem__(i)` (index passed as-is, not coerced to Int).
                    Value::Struct { name, .. } => {
                        let sname = name.clone();
                        let recv = regs[base.0 as usize].clone();
                        let idx = regs[index.0 as usize].clone();
                        regs[dest.0 as usize] =
                            self.call_dunder(prog, &sname, "__getitem__", vec![recv, idx])?;
                    }
                    // `ptr[i]` loads the pointee at `base + i` from the heap arena.
                    Value::Pointer(b) => {
                        let b = *b;
                        let off = value_as_index(&regs[index.0 as usize])?;
                        let i = self.heap_index(b, off)?;
                        regs[dest.0 as usize] = self.heap[i].clone();
                    }
                    _ => {
                        let idx = value_as_index(&regs[index.0 as usize])?;
                        regs[dest.0 as usize] = index_value(&regs[base.0 as usize], idx)?;
                    }
                }
            }
            MirInstr::Slice {
                dest,
                object,
                lower,
                upper,
                step,
            } => {
                let bound = |b: &Option<Reg>| -> Result<Option<i64>, RuntimeError> {
                    match b {
                        Some(r) => Ok(Some(value_as_index(&regs[r.0 as usize])?)),
                        None => Ok(None),
                    }
                };
                let (lo, hi, st) = (bound(lower)?, bound(upper)?, bound(step)?);
                regs[dest.0 as usize] =
                    crate::runtime::slice_value(&regs[object.0 as usize], lo, hi, st)?;
            }
            MirInstr::MakeList { dest, elems } => {
                let mut items: Vec<Value> =
                    elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                promote_numeric_elems(&mut items); // unify literal element kinds
                regs[dest.0 as usize] = Value::List(items);
            }
            MirInstr::MakeTuple { dest, elems } => {
                let items = elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                regs[dest.0 as usize] = Value::Tuple(items);
            }
            MirInstr::MakeSimd {
                dest,
                dtype,
                width,
                elems,
            } => {
                let vals: Vec<Value> = elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                regs[dest.0 as usize] = simd_from_values(*dtype, *width, &vals)?;
            }
            MirInstr::Store { place, src } => {
                let v = regs[src.0 as usize].clone();
                // Classify a projected `container[i] = v` by the container's runtime
                // type: a heap pointer writes the arena; a user struct dispatches
                // `__setitem__` (mut-self write-back); anything else (a slot / List /
                // SIMD lane) goes through `store_place`.
                enum StoreTarget {
                    Ptr(usize, Reg),
                    StructIdx(MirPlace, Reg),
                }
                let target = if let Some((Proj::Index(ireg), prefix)) = place.proj.split_last() {
                    let parent = MirPlace {
                        root: place.root,
                        proj: prefix.to_vec(),
                    };
                    let ireg = *ireg;
                    match nav_mut(vars, regs, &parent)? {
                        Value::Pointer(b) => Some(StoreTarget::Ptr(*b, ireg)),
                        Value::Struct { .. } => Some(StoreTarget::StructIdx(parent, ireg)),
                        _ => None,
                    }
                } else {
                    None
                };
                match target {
                    Some(StoreTarget::Ptr(base, ireg)) => {
                        let off = value_as_index(&regs[ireg.0 as usize])?;
                        let i = self.heap_index(base, off)?;
                        self.heap[i] = v;
                    }
                    Some(StoreTarget::StructIdx(parent, ireg)) => {
                        let idx = regs[ireg.0 as usize].clone();
                        self.store_index_dunder(prog, &parent, idx, v, regs, vars)?;
                    }
                    None => store_place(vars, regs, place, v)?,
                }
            }
            MirInstr::LoadPlace { dest, place } => {
                // The read half of `c[i] += e` on a user struct goes through
                // `c.__getitem__(i)`; any other place reads its slot / SIMD lane.
                regs[dest.0 as usize] = match self.load_index_dunder(prog, place, regs, vars)? {
                    Some(v) => v,
                    None => load_place(vars, regs, place)?,
                };
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
            MirInstr::GetIter { iter } => {
                // Normalize a user struct iterable to its iterator (`c.__iter__()`);
                // a built-in `range`/`List` iterates in place (no-op).
                let slot = *iter as usize;
                for _ in 0..8 {
                    let Value::Struct { name, .. } = &vars[slot] else {
                        break;
                    };
                    let sname = name.clone();
                    if prog
                        .index_of(&prog.overload_name(&format!("{sname}.__next__"), 0))
                        .is_some()
                    {
                        break;
                    }
                    let it = vars[slot].clone();
                    vars[slot] = self.call_dunder(prog, &sname, "__iter__", vec![it])?;
                }
            }
            MirInstr::HasNext { dest, iter } => {
                let slot = *iter as usize;
                // A struct iterator reports remaining length via `__len__` (bounded
                // iteration): more elements iff `len(it) > 0`.
                let has = if let Value::Struct { name, .. } = &vars[slot] {
                    let sname = name.clone();
                    let it = vars[slot].clone();
                    match self.call_dunder(prog, &sname, "__len__", vec![it])? {
                        Value::Int(n) => n > 0,
                        other => {
                            return Err(RuntimeError::TypeError(format!(
                                "vm: iterator __len__ must return Int, got {}",
                                crate::runtime::type_name(&other)
                            )));
                        }
                    }
                } else {
                    match &vars[slot] {
                        Value::Range { start, stop, step } => {
                            (*step > 0 && start < stop) || (*step < 0 && start > stop)
                        }
                        Value::List(items) => !items.is_empty(),
                        other => {
                            return Err(RuntimeError::Unsupported(format!(
                                "vm: `for` iterator must be a range, List, or a type with \
                                 __iter__, got {}",
                                crate::runtime::type_name(other)
                            )));
                        }
                    }
                };
                regs[dest.0 as usize] = Value::Bool(has);
            }
            MirInstr::Next { dest, iter } => {
                let slot = *iter as usize;
                // A struct iterator advances via `__next__(mut self)`: the element is
                // the return value, and the advanced iterator (frame slot 0) is
                // written back into the iterator variable.
                if let Value::Struct { name, .. } = &vars[slot] {
                    let sname = name.clone();
                    let it = vars[slot].clone();
                    let fidx = prog
                        .index_of(&prog.overload_name(&format!("{sname}.__next__"), 0))
                        .ok_or_else(|| {
                            RuntimeError::Unsupported(format!(
                                "vm: struct '{sname}' has no '__next__'"
                            ))
                        })?;
                    let (ret, frame_vars) = self.call_frame(prog, fidx, vec![it], &[])?;
                    vars[slot] = frame_vars.into_iter().next().unwrap_or(Value::None);
                    regs[dest.0 as usize] = ret;
                } else {
                    match &mut vars[slot] {
                        Value::Range { start, stop, step } => {
                            let (cur, st, sp) = (*start, *stop, *step);
                            regs[dest.0 as usize] = Value::Int(cur);
                            vars[slot] = Value::Range {
                                start: cur + sp,
                                stop: st,
                                step: sp,
                            };
                        }
                        Value::List(items) => {
                            regs[dest.0 as usize] = items.remove(0);
                        }
                        ref other => {
                            return Err(RuntimeError::Unsupported(format!(
                                "vm: `for` iterator must be a range, List, or a type with \
                                 __iter__, got {}",
                                crate::runtime::type_name(other)
                            )));
                        }
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
            MirInstr::Try {
                body,
                handler,
                orelse,
                finalbody,
                cleanup,
            } => {
                // A `try` may complete with a `return` that crossed its boundary;
                // propagate that outcome to the block driver.
                return self.exec_try(
                    prog,
                    TryRegions {
                        body,
                        handler,
                        orelse,
                        finalbody,
                        cleanup,
                    },
                    CallerFrame {
                        registers: regs,
                        variables: vars,
                    },
                );
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
    fn exec_try(
        &mut self,
        prog: &Prog,
        regions: TryRegions<'_>,
        frame: CallerFrame<'_>,
    ) -> Result<Flow, RuntimeError> {
        let TryRegions {
            body,
            handler,
            orelse,
            finalbody,
            cleanup,
        } = regions;
        let CallerFrame {
            registers: regs,
            variables: vars,
        } = frame;
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
                MirTerm::Branch {
                    cond,
                    then_b,
                    else_b,
                } => {
                    block = if is_true(&regs[cond.0 as usize]) {
                        *then_b
                    } else {
                        *else_b
                    };
                }
                MirTerm::Return(r) => {
                    let v = r
                        .as_ref()
                        .map(|r| regs[r.0 as usize].clone())
                        .unwrap_or(Value::None);
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
    fn method_call(
        &mut self,
        prog: &Prog,
        invocation: MethodInvocation<'_>,
        frame: CallerFrame<'_>,
    ) -> Result<Value, RuntimeError> {
        let MethodInvocation {
            receiver: recv,
            method,
            resolved_name: resolved,
            arguments: args,
            receiver_place: recv_place,
            argument_places: arg_places,
        } = invocation;
        let CallerFrame {
            registers: regs,
            variables: vars,
        } = frame;
        // Intrinsic dunders on a built-in numeric/hashable value; a struct with
        // its own implementation still dispatches to its method below.
        if !matches!(recv, Value::Struct { .. }) {
            match (method, args.len()) {
                // `Hashable` — `x.__hash__()`.
                ("__hash__", 0) => return crate::runtime::builtin_hash(&recv).map(Value::UInt),
                // `Floorable`/`Ceilable`/`Truncable` — `x.__floor__()` etc. (Phase 7).
                ("__floor__" | "__ceil__" | "__trunc__", 0) => {
                    return crate::runtime::builtin_round_dir(method, &recv);
                }
                // `CeilDivable` — `x.__ceildiv__(y)`.
                ("__ceildiv__", 1) => return crate::runtime::builtin_ceildiv(&recv, &args[0]),
                _ => {}
            }
        }
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
            // `UnsafePointer` methods: `free()` releases the allocation (a no-op in
            // the arena model — the arena never reclaims).
            Value::Pointer(_) => match method {
                "free" => Ok(Value::None),
                _ => Err(RuntimeError::Unsupported(format!(
                    "vm: UnsafePointer has no method '{method}'"
                ))),
            },
            Value::Struct { name, .. } => {
                let method_argc = args.len();
                let source_fname = format!("{name}.{method}");
                let fname = resolved
                    .map(str::to_string)
                    .unwrap_or_else(|| prog.overload_name(&source_fname, method_argc));
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
                let is_mut = prog.structs.get(name).is_some_and(|d| {
                    let key = if fname != source_fname {
                        fname.as_str()
                    } else {
                        method
                    };
                    d.mut_self_methods.contains(key)
                });
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
        // Built-ins take positional arguments only, and user functions handle
        // keywords through their signatures below. Struct constructors get a
        // narrow exception for Mojo's lifecycle copy constructor (`copy:`).
        if !kwargs.is_empty() && !prog.sigs.contains_key(name) && !prog.structs.contains_key(name) {
            return Err(RuntimeError::Unsupported(format!(
                "vm: keyword arguments to '{name}' are not supported"
            )));
        }
        if let Some(struct_name) = crate::symbol::init_overload_struct(name)
            && prog.structs.contains_key(struct_name)
        {
            if !kwargs.is_empty() {
                return Err(RuntimeError::Unsupported(format!(
                    "vm: keyword arguments to '{struct_name}' are not supported"
                )));
            }
            return self.construct_via_init(prog, struct_name, Some(name), args, param_vals);
        }
        match name {
            "print" => {
                let cells: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                self.output.push_str(&cells.join(" "));
                self.output.push('\n');
                Ok(Value::None)
            }
            // `String(c)` on a user struct dispatches to `c.__str__()`; a primitive
            // value uses its `Display`.
            "String" => match args.into_iter().next() {
                Some(Value::Struct {
                    name,
                    fields,
                    value_params,
                }) => {
                    let recv = Value::Struct {
                        name: name.clone(),
                        fields,
                        value_params,
                    };
                    self.call_dunder(prog, &name, "__str__", vec![recv])
                }
                other => Ok(Value::Str(other.map(|v| v.to_string()).unwrap_or_default())),
            },
            // `len(c)` on a user struct dispatches to `c.__len__()`.
            "len" => match args.into_iter().next() {
                Some(Value::Str(s)) => Ok(Value::Int(s.len() as i64)),
                Some(Value::List(items)) => Ok(Value::Int(items.len() as i64)),
                Some(Value::Struct {
                    name,
                    fields,
                    value_params,
                }) => {
                    let recv = Value::Struct {
                        name: name.clone(),
                        fields,
                        value_params,
                    };
                    self.call_dunder(prog, &name, "__len__", vec![recv])
                }
                _ => Err(RuntimeError::Unsupported(
                    "vm: len supports String, List, and structs with __len__".into(),
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
            "input" => builtin_input(arg1(name, args)?),
            "Int" | "UInt" | "Float64" | "Bool" => builtin_convert(name, arg1(name, args)?),
            "divmod" => {
                let (a, b) = arg2(name, args)?;
                builtin_divmod(a, b)
            }
            "Error" => builtin_error(arg1(name, args)?),
            // A struct constructor. A hand-written `def __init__(out self, …)`
            // takes precedence over the fieldwise constructor: build an uninitialized
            // `self` skeleton, run `__init__`, and return the initialized value.
            _ if prog.structs.contains_key(name) => {
                if !kwargs.is_empty() {
                    self.construct_via_copy(prog, name, args, kwargs, param_vals)
                } else if prog
                    .index_of(&prog.overload_name(&format!("{name}.__init__"), args.len()))
                    .is_some()
                {
                    self.construct_via_init(prog, name, None, args, param_vals)
                } else {
                    construct(&prog.structs[name], name, args, param_vals)
                }
            }
            // `List[T]()` / `List(a, b, …)` construction (the literal `[a, b]`
            // lowers to `MakeList` instead). The element type in `[T]` is dropped
            // by the MIR but the runtime list is dynamically typed, so args suffice.
            "List" => {
                let mut items = args;
                promote_numeric_elems(&mut items);
                Ok(Value::List(items))
            }
            // `UnsafePointer[T].alloc(n)` — reserve `n` slots in the heap arena and
            // return a pointer to the base (the element type is erased).
            "UnsafePointer.alloc" => {
                let n = match arg1(name, args)? {
                    Value::Int(n) => n,
                    other => {
                        return Err(RuntimeError::TypeError(format!(
                            "vm: UnsafePointer.alloc expects an Int, got {}",
                            crate::runtime::type_name(&other)
                        )));
                    }
                };
                self.heap_alloc(n)
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
                            .map(|((pname, _), val)| {
                                (pname.clone(), val.clone().unwrap_or(Value::None))
                            })
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
        let prog = build_prog(program);
        self.configure_lifecycle(&prog);
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

fn build_prog(program: &[Stmt]) -> Prog {
    let mir = crate::analysis::elaborate_drops_program(crate::mir::lower_program(program));
    let structs = build_structs(&mir.declarations);
    let sigs = build_sigs(&mir.declarations);
    Prog {
        // Elaborate ASAP drops: splice a `DropVar` after each variable's last
        // use, so a struct's `__del__` runs there (Phase 4). A no-op for values
        // without a destructor, so parity with the tree-walker is preserved.
        mir,
        structs,
        sigs,
    }
}

/// Build the VM registry from declaration metadata carried by MIR.
fn build_structs(declarations: &crate::mir::MirDeclarations) -> HashMap<String, StructDef> {
    declarations
        .structs
        .iter()
        .map(|declaration| {
            (
                declaration.name.clone(),
                StructDef {
                    fields: declaration.fields.clone(),
                    mut_self_methods: declaration.mut_self_methods.clone(),
                    fieldwise_init: declaration.fieldwise_init,
                    param_decls: declaration.param_decls.clone(),
                },
            )
        })
        .collect()
}

/// Build the VM calling registry from declaration metadata carried by MIR.
fn build_sigs(declarations: &crate::mir::MirDeclarations) -> HashMap<String, FnSig> {
    declarations
        .functions
        .iter()
        .map(|declaration| {
            (
                declaration.lowered_name.clone(),
                FnSig {
                    param_names: declaration.param_names.clone(),
                    param_types: declaration.param_types.clone(),
                    defaults: declaration
                        .defaults
                        .iter()
                        .map(|default| default.as_ref().and_then(const_default))
                        .collect(),
                    required: declaration.required.clone(),
                    variadic: declaration.variadic.clone(),
                    variadic_index: declaration.variadic_index,
                    positional_only: declaration.positional_only,
                    keyword_only: declaration.keyword_only,
                    param_decls: declaration.param_decls.clone(),
                },
            )
        })
        .collect()
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
        &sig.required,
        sig.positional_only,
        sig.keyword_only,
        argv.len(),
        &kw_names,
        sig.variadic.is_some(),
    )
    .map_err(|e| e.into_runtime_error(name))?;

    let mut regular_values = Vec::with_capacity(slots.len());
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
        regular_values.push(coerce(value, &sig.param_types[i]));
    }
    // Collect overflow positional args into the `*args` list.
    let (out, frame_slots) = if let Some(elem_ty) = &sig.variadic {
        let items = overflow
            .iter()
            .map(|&idx| coerce(argv[idx].clone(), elem_ty))
            .collect();
        let idx = sig.variadic_index.unwrap_or(regular_values.len());
        let mut out = Vec::with_capacity(regular_values.len() + 1);
        out.extend(regular_values[..idx].iter().cloned());
        out.push(Value::List(items));
        out.extend(regular_values[idx..].iter().cloned());
        let mut frame_slots = Vec::with_capacity(slots.len() + 1);
        frame_slots.extend(slots[..idx].iter().copied());
        frame_slots.push(ArgSlot::Default);
        frame_slots.extend(slots[idx..].iter().copied());
        (out, frame_slots)
    } else {
        (regular_values, slots)
    };
    Ok((out, frame_slots))
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
    Ok(Value::Struct {
        name: name.to_string(),
        fields,
        value_params,
    })
}

/// Read a struct field (or a reified value parameter, e.g. `Self.n`) by name.
fn get_field(base: &Value, field: &str) -> Result<Value, RuntimeError> {
    match base {
        Value::Struct {
            fields,
            value_params,
            ..
        } => fields
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
        return Err(RuntimeError::ArityMismatch {
            name: name.to_string(),
            expected: 1,
            got: args.len(),
        });
    }
    Ok(args.pop().unwrap())
}

/// Take the two arguments of a two-arg built-in (`min`/`max`).
fn arg2(name: &str, args: Vec<Value>) -> Result<(Value, Value), RuntimeError> {
    if args.len() != 2 {
        return Err(RuntimeError::ArityMismatch {
            name: name.to_string(),
            expected: 2,
            got: args.len(),
        });
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
            Value::Struct {
                fields,
                value_params,
                ..
            } => {
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
