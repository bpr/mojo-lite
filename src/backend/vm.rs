//! Register-VM backend and Mojito's sole runtime.
//!
//! The VM executes verified, drop-elaborated [`MirProgram`]s over per-call
//! register and variable frames. Language-level validation belongs to the
//! checker and ownership analysis; this module implements the checked call ABI,
//! places, structured control flow, exceptions, destruction, and runtime
//! primitives. See `docs/features.md` for the supported language surface.

use crate::ast::Stmt;
use crate::call::{ArgSlot, CallVariadics, match_call_slots};
use crate::checked::CheckedConst;
use crate::error::RuntimeError;
use crate::hir::VarId;
use crate::mir::{
    Const, MirBlock, MirInstr, MirPlace, MirProgram, MirSubscriptArg, MirTerm, Proj, Reg,
};
use crate::runtime::{
    RefProjection, Value, apply_infix, apply_list_method, apply_prefix, builtin_abs,
    builtin_convert, builtin_divmod, builtin_error, builtin_input, builtin_min_max, builtin_round,
    is_list_mutator, list_query, promote_numeric_elems, read_simd_lane, simd_from_values,
    value_as_index, values_equal,
};
use crate::types::Ty;
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
    fields: Vec<(String, Ty)>,
    mut_self_methods: std::collections::HashSet<String>,
    fieldwise_init: bool,
    /// The struct's compile-time parameters (`[...]`), each `(name, is_value)` —
    /// `true` for a value parameter (reified onto the instance at construction),
    /// `false` for an erased type parameter. Aligns positionally with a
    /// construction's supplied parameter arguments.
    param_decls: Vec<(String, bool)>,
    explicit_destroy: bool,
}

/// A free function's calling signature (the MIR doesn't keep it), for matching
/// positional + keyword arguments to parameter slots — filling defaults and
/// collecting a trailing `*args`. Covers only the *regular* parameters; `variadic`
/// is the trailing `*args` element type, if any.
struct FnSig {
    param_names: Vec<String>,
    param_types: Vec<Ty>,
    /// Const-evaluated default per regular parameter (`None` = no default, or a
    /// non-constant default the VM can't fold — using such a slot errors).
    defaults: Vec<Option<Value>>,
    required: Vec<bool>,
    variadic: Option<Ty>,
    /// Where the collected `*args` list belongs among source parameters. For a
    /// signature like `def f(a, *xs, b)`, this is `Some(1)`.
    variadic_index: Option<usize>,
    kw_variadic: Option<Ty>,
    kw_variadic_index: Option<usize>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FrameId(u64);

struct ReturnContinuation {
    dest: Reg,
    writebacks: Vec<(usize, MirPlace)>,
}

struct Frame {
    id: FrameId,
    function: usize,
    registers: Vec<Value>,
    variables: Vec<Value>,
    block: usize,
    instruction: usize,
    continuation: Option<ReturnContinuation>,
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
    keyword_arguments: Vec<(String, Value)>,
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

    /// Resolve a selected method signature against the receiver's concrete
    /// runtime type. Bounded generic calls carry an abstract checker symbol;
    /// retargeting its suffix preserves overload selection even when every
    /// overload has the same positional arity (for example `**kwargs` methods).
    fn runtime_method_name(
        &self,
        receiver_type: &str,
        method: &str,
        resolved: Option<&str>,
        argc: usize,
    ) -> String {
        if let Some(selected) = resolved {
            if let Some(retargeted) = crate::symbol::retarget_method_symbol(selected, receiver_type)
                && self.index_of(&retargeted).is_some()
            {
                return retargeted;
            }
            if self.index_of(selected).is_some() {
                return selected.to_string();
            }
        }
        self.overload_name(&format!("{receiver_type}.{method}"), argc)
    }
}

#[derive(Default)]
struct HeapAllocation {
    slots: Vec<Value>,
    #[allow(dead_code)]
    alignment: usize,
    live: bool,
}

#[derive(Default)]
pub struct VmBackend {
    output: String,
    /// The final top-level (`__toplevel__`) variable values, by name — the global
    /// bindings, captured after execution for the CLI `run` dump and tests.
    bindings: Vec<(String, Value)>,
    /// Provenance-bearing allocations. Pointer copies retain an allocation id;
    /// freeing invalidates every alias and allocation bounds are never confused
    /// with adjacent allocations.
    heap: Vec<HeapAllocation>,
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
    frames: Vec<Frame>,
    next_frame_id: u64,
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
        let checked = crate::checker::check_program(program).map_err(|error| {
            RuntimeError::TypeError(format!(
                "VM compile-time program failed the checked boundary: {error}"
            ))
        })?;
        let prog = build_prog_checked(&checked)?;
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
    fn heap_alloc(&mut self, n: i64, alignment: i64) -> Result<Value, RuntimeError> {
        if n < 0 {
            return Err(RuntimeError::TypeError(
                "vm: UnsafePointer.alloc count must be non-negative".to_string(),
            ));
        }
        if alignment <= 0 || !(alignment as u64).is_power_of_two() {
            return Err(RuntimeError::TypeError(
                "vm: UnsafePointer allocation alignment must be a positive power of two"
                    .to_string(),
            ));
        }
        self.heap.push(HeapAllocation {
            slots: vec![Value::None; n as usize],
            alignment: alignment as usize,
            live: true,
        });
        Ok(Value::Pointer {
            allocation: self.heap.len() as u64,
            offset: 0,
        })
    }

    /// Resolve `base + offset` to an arena index, bounds-checking against the arena
    /// (a truly out-of-arena access errors rather than panicking; an in-arena but
    /// past-allocation access is permitted — `UnsafePointer` is unchecked).
    fn heap_index(
        &self,
        allocation: u64,
        base: i64,
        offset: i64,
    ) -> Result<(usize, usize), RuntimeError> {
        if allocation == 0 {
            return Err(RuntimeError::TypeError(
                "vm: dereference of dangling UnsafePointer".to_string(),
            ));
        }
        let allocation_index = usize::try_from(allocation - 1).map_err(|_| {
            RuntimeError::TypeError("vm: invalid UnsafePointer provenance".to_string())
        })?;
        let region = self.heap.get(allocation_index).ok_or_else(|| {
            RuntimeError::TypeError("vm: invalid UnsafePointer provenance".to_string())
        })?;
        if !region.live {
            return Err(RuntimeError::TypeError(
                "vm: use after UnsafePointer.free()".to_string(),
            ));
        }
        let i = base.checked_add(offset).ok_or_else(|| {
            RuntimeError::TypeError("vm: UnsafePointer offset overflow".to_string())
        })?;
        if i < 0 || i as usize >= region.slots.len() {
            return Err(RuntimeError::TypeError(
                "vm: UnsafePointer access out of bounds".to_string(),
            ));
        }
        Ok((allocation_index, i as usize))
    }

    fn heap_free(&mut self, allocation: u64, offset: i64) -> Result<(), RuntimeError> {
        if allocation == 0 || offset != 0 {
            return Err(RuntimeError::TypeError(
                "vm: free requires a live allocation-base pointer".to_string(),
            ));
        }
        let region = self
            .heap
            .get_mut((allocation - 1) as usize)
            .ok_or_else(|| {
                RuntimeError::TypeError("vm: invalid UnsafePointer provenance".to_string())
            })?;
        if !region.live {
            return Err(RuntimeError::TypeError(
                "vm: double free of UnsafePointer allocation".to_string(),
            ));
        }
        region.live = false;
        region.slots.clear();
        Ok(())
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
        let frame = self.make_frame(prog, fidx, args, value_params, None)?;
        let target = frame.id;
        self.frames.push(frame);
        self.drive_frames(prog, target)
    }

    fn make_frame(
        &mut self,
        prog: &Prog,
        fidx: usize,
        args: Vec<Value>,
        value_params: &[(String, Value)],
        continuation: Option<ReturnContinuation>,
    ) -> Result<Frame, RuntimeError> {
        self.burn_ctfe()?;
        let f = &prog.mir.functions[fidx].1;
        // The MIR flattens only positional arguments; a mismatched count means a
        // default/keyword/`*args` form (not lowered yet) — refuse rather than
        // silently bind `None`.
        if args.len() != f.n_params {
            return Err(RuntimeError::Unsupported(format!(
                "vm backend does not support default/keyword/variadic arguments yet \
                 (call passed {} args to {}-parameter function '{}')",
                args.len(),
                f.n_params,
                prog.mir.functions[fidx].0,
            )));
        }
        let regs = vec![Value::None; f.n_regs as usize];
        let mut vars = vec![Value::None; f.n_vars];
        for (i, arg) in args.into_iter().enumerate() {
            vars[i] = match f.param_types.get(i) {
                Some(t) => crate::runtime::coerce_checked(arg, t),
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

        let id = FrameId(self.next_frame_id);
        self.next_frame_id += 1;
        Ok(Frame {
            id,
            function: fidx,
            registers: regs,
            variables: vars,
            block: 0,
            instruction: 0,
            continuation,
        })
    }

    fn drive_frames(
        &mut self,
        prog: &Prog,
        target: FrameId,
    ) -> Result<(Value, Vec<Value>), RuntimeError> {
        loop {
            let mut frame = self.frames.pop().ok_or_else(|| {
                RuntimeError::Unsupported("vm: frame stack underflow".to_string())
            })?;
            self.burn_ctfe()?;
            let function = &prog.mir.functions[frame.function].1;
            let block = &function.blocks[frame.block];
            if frame.instruction < block.instrs.len() {
                let instruction = block.instrs[frame.instruction].clone();
                frame.instruction += 1;
                if let Some(child) = self.prepare_direct_call(prog, &frame, &instruction)? {
                    self.frames.push(frame);
                    self.frames.push(child);
                    continue;
                }
                match self.exec_instr(
                    prog,
                    &instruction,
                    frame.id,
                    &mut frame.registers,
                    &mut frame.variables,
                )? {
                    Flow::Normal => {
                        self.frames.push(frame);
                        continue;
                    }
                    Flow::Jump(block) => {
                        frame.block = block;
                        frame.instruction = 0;
                        self.frames.push(frame);
                        continue;
                    }
                    Flow::Return(value) => {
                        if let Some(done) = self.finish_frame(target, frame, value)? {
                            return Ok(done);
                        }
                        continue;
                    }
                }
            }
            let returned = match &block.term {
                MirTerm::Jump(next) => {
                    frame.block = *next;
                    frame.instruction = 0;
                    self.frames.push(frame);
                    continue;
                }
                MirTerm::Branch {
                    cond,
                    then_b,
                    else_b,
                } => {
                    frame.block = if is_true(&frame.registers[cond.0 as usize]) {
                        *then_b
                    } else {
                        *else_b
                    };
                    frame.instruction = 0;
                    self.frames.push(frame);
                    continue;
                }
                MirTerm::Return(reg) => reg
                    .as_ref()
                    .map(|reg| frame.registers[reg.0 as usize].clone())
                    .unwrap_or(Value::None),
                MirTerm::FallOff | MirTerm::EscapeJump { .. } => Value::None,
            };
            if let Some(done) = self.finish_frame(target, frame, returned)? {
                return Ok(done);
            }
        }
    }

    fn finish_frame(
        &mut self,
        target: FrameId,
        frame: Frame,
        value: Value,
    ) -> Result<Option<(Value, Vec<Value>)>, RuntimeError> {
        if frame.id == target {
            return Ok(Some((value, frame.variables)));
        }
        let continuation = frame.continuation.ok_or_else(|| {
            RuntimeError::Unsupported("vm: child frame has no return continuation".to_string())
        })?;
        let caller = self.frames.last_mut().ok_or_else(|| {
            RuntimeError::Unsupported("vm: returning child has no caller frame".to_string())
        })?;
        caller.registers[continuation.dest.0 as usize] = value;
        for (parameter, place) in continuation.writebacks {
            *nav_mut(&mut caller.variables, &caller.registers, &place)? =
                frame.variables[parameter].clone();
        }
        Ok(None)
    }

    fn prepare_direct_call(
        &mut self,
        prog: &Prog,
        caller: &Frame,
        instruction: &MirInstr,
    ) -> Result<Option<Frame>, RuntimeError> {
        if let MirInstr::CallIndirect {
            dest,
            callee,
            args,
            kwargs,
            ..
        } = instruction
        {
            let callable = &caller.registers[callee.0 as usize];
            let mut nominal_receiver = None;
            let (function_name, captured) = match callable {
                Value::Function(function_name) => (function_name.clone(), Vec::new()),
                Value::Closure { function, captures } => (function.clone(), captures.clone()),
                Value::Struct { name, .. } => {
                    nominal_receiver = Some(callable.clone());
                    (
                        prog.overload_name(&format!("{name}.__call__"), args.len()),
                        Vec::new(),
                    )
                }
                value => {
                    return Err(RuntimeError::NotCallable(crate::runtime::type_name(value)));
                }
            };
            let index = prog
                .index_of(&function_name)
                .ok_or_else(|| RuntimeError::NotCallable(function_name.clone()))?;
            let mut positional = captured;
            positional.extend(
                args.iter()
                    .map(|register| caller.registers[register.0 as usize].clone()),
            );
            let keywords = kwargs
                .iter()
                .map(|(name, register)| {
                    (name.clone(), caller.registers[register.0 as usize].clone())
                })
                .collect();
            let (mut bound, _) = match prog.sigs.get(&function_name) {
                Some(signature) => {
                    self.bind_for_call(prog, &function_name, signature, positional, keywords)?
                }
                None => (
                    positional,
                    (0..args.len()).map(ArgSlot::Positional).collect(),
                ),
            };
            if let Some(receiver) = nominal_receiver {
                bound.insert(0, receiver);
            }
            return self
                .make_frame(
                    prog,
                    index,
                    bound,
                    &[],
                    Some(ReturnContinuation {
                        dest: *dest,
                        writebacks: Vec::new(),
                    }),
                )
                .map(Some);
        }
        if let MirInstr::MethodCall {
            dest,
            recv,
            method,
            resolved,
            args,
            kwargs,
            recv_place,
            arg_places,
            ..
        } = instruction
            && let Value::Struct { name, .. } = &caller.registers[recv.0 as usize]
        {
            let function_name =
                prog.runtime_method_name(name, method, resolved.as_deref(), args.len());
            let Some(index) = prog.index_of(&function_name) else {
                return Ok(None);
            };
            if !prog.mir.functions[index].1.returns_reference {
                return Ok(None);
            }
            let positional: Vec<Value> = args
                .iter()
                .map(|register| caller.registers[register.0 as usize].clone())
                .collect();
            let keywords: Vec<(String, Value)> = kwargs
                .iter()
                .map(|(key, register)| (key.clone(), caller.registers[register.0 as usize].clone()))
                .collect();
            let (mut bound, slots) = match prog.sigs.get(&function_name) {
                Some(signature) => {
                    self.bind_for_call(prog, &function_name, signature, positional, keywords)?
                }
                None => (
                    positional,
                    (0..args.len()).map(ArgSlot::Positional).collect(),
                ),
            };
            let definition = &prog.mir.functions[index].1;
            for parameter in 1..definition.ref_params.len() {
                if !definition.ref_params[parameter] {
                    continue;
                }
                let place = match slots.get(parameter - 1) {
                    Some(ArgSlot::Positional(argument)) => {
                        arg_places.get(*argument).and_then(|place| place.as_ref())
                    }
                    _ => None,
                }
                .ok_or_else(|| {
                    RuntimeError::Unsupported(format!(
                        "vm: a mut/ref argument to method '{function_name}' must be a place"
                    ))
                })?;
                bound[parameter - 1] = self.reference_to_place(caller, place)?;
            }
            let receiver = if definition.ref_params.first().copied().unwrap_or(false) {
                let place = recv_place.as_ref().ok_or_else(|| {
                    RuntimeError::Unsupported(format!(
                        "vm: mutable receiver for '{function_name}' must be a place"
                    ))
                })?;
                self.reference_to_place(caller, place)?
            } else {
                caller.registers[recv.0 as usize].clone()
            };
            let mut call_args = Vec::with_capacity(bound.len() + 1);
            call_args.push(receiver);
            call_args.extend(bound);
            return self
                .make_frame(
                    prog,
                    index,
                    call_args,
                    &[],
                    Some(ReturnContinuation {
                        dest: *dest,
                        writebacks: Vec::new(),
                    }),
                )
                .map(Some);
        }
        let MirInstr::Call {
            dest,
            func,
            args,
            kwargs,
            arg_places,
            param_arg_regs,
            ..
        } = instruction
        else {
            return Ok(None);
        };
        if crate::symbol::init_overload_struct(&func.0).is_some() {
            return Ok(None);
        }
        let Some(index) = prog.index_of(&func.0) else {
            return Ok(None);
        };
        let positional: Vec<Value> = args
            .iter()
            .map(|reg| caller.registers[reg.0 as usize].clone())
            .collect();
        let keywords: Vec<(String, Value)> = kwargs
            .iter()
            .map(|(name, reg)| (name.clone(), caller.registers[reg.0 as usize].clone()))
            .collect();
        let (mut bound, slots) = match prog.sigs.get(&func.0) {
            Some(signature) => {
                self.bind_for_call(prog, &func.0, signature, positional, keywords)?
            }
            None => (
                positional,
                (0..args.len()).map(ArgSlot::Positional).collect(),
            ),
        };
        let value_params: Vec<(String, Value)> = prog
            .sigs
            .get(&func.0)
            .map(|signature| {
                signature
                    .param_decls
                    .iter()
                    .zip(param_arg_regs)
                    .filter(|((_, is_value), _)| *is_value)
                    .map(|((name, _), reg)| {
                        (
                            name.clone(),
                            reg.map(|reg| caller.registers[reg.0 as usize].clone())
                                .unwrap_or(Value::None),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (parameter, is_ref) in prog.mir.functions[index].1.ref_params.iter().enumerate() {
            if !is_ref {
                continue;
            }
            let place = match slots.get(parameter) {
                Some(ArgSlot::Positional(argument)) => {
                    arg_places.get(*argument).and_then(|place| place.as_ref())
                }
                _ => None,
            }
            .ok_or_else(|| {
                RuntimeError::Unsupported(format!(
                    "vm: a mut/ref argument to '{}' must be a place",
                    func.0
                ))
            })?;
            bound[parameter] = self.reference_to_place(caller, place)?;
        }
        self.make_frame(
            prog,
            index,
            bound,
            &value_params,
            Some(ReturnContinuation {
                dest: *dest,
                writebacks: Vec::new(),
            }),
        )
        .map(Some)
    }

    fn reference_to_place(&self, frame: &Frame, place: &MirPlace) -> Result<Value, RuntimeError> {
        Self::reference_to_place_parts(frame.id, &frame.registers, &frame.variables, place)
    }

    fn reference_to_place_parts(
        frame_id: FrameId,
        registers: &[Value],
        variables: &[Value],
        place: &MirPlace,
    ) -> Result<Value, RuntimeError> {
        let (target_frame, slot, mut projection) = match &variables[place.root as usize] {
            Value::Ref {
                frame,
                slot,
                projection,
            } => (*frame, *slot, projection.clone()),
            _ => (frame_id.0, place.root as usize, Vec::new()),
        };
        for segment in &place.proj {
            projection.push(match segment {
                Proj::Field(name) => RefProjection::Field(name.clone()),
                Proj::Index(register) => {
                    RefProjection::Index(value_as_index(&registers[register.0 as usize])? as usize)
                }
                Proj::Variant(index) => RefProjection::Variant(*index),
            });
        }
        Ok(Value::Ref {
            frame: target_frame,
            slot,
            projection,
        })
    }

    fn reference_slot(&self, reference: &Value) -> Result<&Value, RuntimeError> {
        let Value::Ref {
            frame,
            slot,
            projection,
        } = reference
        else {
            return Err(RuntimeError::TypeError(
                "vm: expected reference handle".to_string(),
            ));
        };
        let owner = self
            .frames
            .iter()
            .find(|candidate| candidate.id.0 == *frame)
            .ok_or_else(|| {
                RuntimeError::TypeError(format!("vm: stale reference to frame {frame}"))
            })?;
        navigate_reference(&owner.variables[*slot], projection)
    }

    fn reference_slot_mut(&mut self, reference: &Value) -> Result<&mut Value, RuntimeError> {
        let Value::Ref {
            frame,
            slot,
            projection,
        } = reference
        else {
            return Err(RuntimeError::TypeError(
                "vm: expected reference handle".to_string(),
            ));
        };
        let owner = self
            .frames
            .iter_mut()
            .find(|candidate| candidate.id.0 == *frame)
            .ok_or_else(|| {
                RuntimeError::TypeError(format!("vm: stale reference to frame {frame}"))
            })?;
        navigate_reference_mut(&mut owner.variables[*slot], projection)
    }

    fn read_reference(
        &self,
        reference: &Value,
        current: FrameId,
        current_variables: &[Value],
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref {
            frame,
            slot,
            projection,
        } = reference
            && *frame == current.0
        {
            return Ok(navigate_reference(&current_variables[*slot], projection)?.clone());
        }
        Ok(self.reference_slot(reference)?.clone())
    }

    fn write_reference(
        &mut self,
        reference: &Value,
        current: FrameId,
        current_variables: &mut [Value],
        value: Value,
    ) -> Result<(), RuntimeError> {
        if let Value::Ref {
            frame,
            slot,
            projection,
        } = reference
            && *frame == current.0
        {
            *navigate_reference_mut(&mut current_variables[*slot], projection)? = value;
            return Ok(());
        }
        *self.reference_slot_mut(reference)? = value;
        Ok(())
    }

    fn extend_reference(
        &self,
        root: &Value,
        projection_path: &[Proj],
        registers: &[Value],
    ) -> Result<Option<Value>, RuntimeError> {
        let Value::Ref {
            frame,
            slot,
            projection,
        } = root
        else {
            return Ok(None);
        };
        let mut projection = projection.clone();
        for segment in projection_path {
            projection.push(match segment {
                Proj::Field(name) => RefProjection::Field(name.clone()),
                Proj::Index(register) => {
                    RefProjection::Index(value_as_index(&registers[register.0 as usize])? as usize)
                }
                Proj::Variant(index) => RefProjection::Variant(*index),
            });
        }
        Ok(Some(Value::Ref {
            frame: *frame,
            slot: *slot,
            projection,
        }))
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

    fn call_subscript_dunder(
        &mut self,
        prog: &Prog,
        receiver: Value,
        arguments: Vec<Value>,
        resolved: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        let Value::Struct { name, .. } = &receiver else {
            return Err(RuntimeError::TypeError(
                "user subscript dispatch requires a struct receiver".to_string(),
            ));
        };
        let source = format!("{name}.__getitem__");
        let function = resolved
            .map(str::to_string)
            .unwrap_or_else(|| prog.overload_name(&source, arguments.len()));
        let index = prog.index_of(&function).ok_or_else(|| {
            RuntimeError::Unsupported(format!("vm: struct '{name}' has no matching __getitem__"))
        })?;
        let user_arguments = match prog.sigs.get(&function) {
            Some(signature) => {
                self.bind_for_call(prog, &function, signature, arguments, Vec::new())?
                    .0
            }
            None => arguments,
        };
        let mut bound = Vec::with_capacity(user_arguments.len() + 1);
        bound.push(receiver);
        bound.extend(user_arguments);
        self.call_function(prog, index, bound, &[])
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
        match (&l, &r, op) {
            (
                Value::Pointer { allocation, offset },
                Value::Int(delta),
                InfixOp::Add | InfixOp::Sub,
            ) => {
                let delta = if op == InfixOp::Sub { -*delta } else { *delta };
                let offset = offset.checked_add(delta).ok_or_else(|| {
                    RuntimeError::TypeError("vm: UnsafePointer offset overflow".to_string())
                })?;
                return Ok(Value::Pointer {
                    allocation: *allocation,
                    offset,
                });
            }
            (
                Value::Pointer {
                    allocation: left_allocation,
                    offset: left_offset,
                },
                Value::Pointer {
                    allocation: right_allocation,
                    offset: right_offset,
                },
                InfixOp::Sub,
            ) => {
                if left_allocation != right_allocation {
                    return Err(RuntimeError::TypeError(
                        "vm: cannot subtract pointers with different provenance".to_string(),
                    ));
                }
                return Ok(Value::Int(left_offset - right_offset));
            }
            (
                Value::Pointer {
                    allocation: left_allocation,
                    offset: left_offset,
                },
                Value::Pointer {
                    allocation: right_allocation,
                    offset: right_offset,
                },
                InfixOp::Eq | InfixOp::Ne,
            ) => {
                let equal = left_allocation == right_allocation && left_offset == right_offset;
                return Ok(Value::Bool(if op == InfixOp::Eq { equal } else { !equal }));
            }
            _ => {}
        }
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
            return Err(RuntimeError::TypeError(
                "__setitem__ dispatch requires a struct container".to_string(),
            ));
        };
        let fname = prog.overload_name(&format!("{name}.__setitem__"), 2);
        let fidx = prog.index_of(&fname).ok_or_else(|| {
            RuntimeError::Unsupported(format!("vm: struct '{name}' has no method '__setitem__'"))
        })?;
        let (_, frame_vars) = self.call_frame(prog, fidx, vec![recv, idx, value], &[])?;
        *nav_mut(vars, regs, parent)? = frame_vars.into_iter().next().unwrap_or(Value::None);
        Ok(())
    }

    /// Store a value through any currently supported place shape. A final index
    /// into an arena pointer writes the heap; a final index into a user struct
    /// dispatches `__setitem__`; ordinary slots, fields, built-in lists, tuples,
    /// and SIMD lanes use `store_place`. Shared by MIR stores and mut-self
    /// write-back so `self.buckets[i].append(v)` follows the same path as
    /// `self.buckets[i] = row`.
    fn store_at_place(
        &mut self,
        prog: &Prog,
        place: &MirPlace,
        value: Value,
        regs: &[Value],
        vars: &mut [Value],
    ) -> Result<(), RuntimeError> {
        enum Target {
            Pointer(u64, i64, Reg),
            StructIndex(Box<MirPlace>, Reg),
            Ordinary,
        }
        let target = if let Some((Proj::Index(index), prefix)) = place.proj.split_last() {
            let parent = MirPlace {
                root: place.root,
                root_ty: place.root_ty.clone(),
                proj: prefix.to_vec(),
                projection_tys: place.projection_tys[..prefix.len()].to_vec(),
                ty: if prefix.is_empty() {
                    place.root_ty.clone()
                } else {
                    place.projection_tys.get(prefix.len() - 1).cloned()
                },
                through: place.through,
            };
            match nav_mut(vars, regs, &parent)? {
                Value::Pointer { allocation, offset } => {
                    Target::Pointer(*allocation, *offset, *index)
                }
                Value::Struct { .. } => Target::StructIndex(Box::new(parent), *index),
                _ => Target::Ordinary,
            }
        } else {
            Target::Ordinary
        };
        match target {
            Target::Pointer(allocation, base, index) => {
                let offset = value_as_index(&regs[index.0 as usize])?;
                let (region, slot) = self.heap_index(allocation, base, offset)?;
                self.heap[region].slots[slot] = value;
                Ok(())
            }
            Target::StructIndex(parent, index) => {
                let index = regs[index.0 as usize].clone();
                self.store_index_dunder(prog, &parent, index, value, regs, vars)
            }
            Target::Ordinary => store_place(vars, regs, place, value),
        }
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
        kwargs: Vec<(String, Value)>,
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
        let constructor = target
            .map(str::to_string)
            .unwrap_or_else(|| prog.overload_name(&format!("{name}.__init__"), args.len()));
        let fidx = prog.index_of(&constructor).ok_or_else(|| {
            RuntimeError::Unsupported(format!(
                "vm: checked constructor '{constructor}' is missing from MIR"
            ))
        })?;
        let user_args = match prog.sigs.get(&constructor) {
            Some(signature) => {
                self.bind_for_call(prog, &constructor, signature, args, kwargs)?
                    .0
            }
            None => args,
        };
        let mut bound = Vec::with_capacity(user_args.len() + 1);
        bound.push(skeleton);
        bound.extend(user_args);
        let (_, frame_vars) = self.call_frame(prog, fidx, bound, &[])?;
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
            Value::Set(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.clone_value(prog, item)?);
                }
                Ok(Value::Set(out))
            }
            Value::Dict(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    out.push((self.clone_value(prog, key)?, self.clone_value(prog, value)?));
                }
                Ok(Value::Dict(out))
            }
            Value::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.clone_value(prog, it)?);
                }
                Ok(Value::Tuple(out))
            }
            Value::Variant {
                alternatives,
                index,
                value,
            } => Ok(Value::Variant {
                alternatives: alternatives.clone(),
                index: *index,
                value: Box::new(self.clone_value(prog, value)?),
            }),
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
            root_ty: place.root_ty.clone(),
            proj: prefix.to_vec(),
            projection_tys: place.projection_tys[..prefix.len()].to_vec(),
            ty: if prefix.is_empty() {
                place.root_ty.clone()
            } else {
                place.projection_tys.get(prefix.len() - 1).cloned()
            },
            through: place.through,
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
            Value::Pointer { allocation, offset } => {
                let off = value_as_index(&regs[ireg.0 as usize])?;
                let (region, i) = self.heap_index(*allocation, *offset, off)?;
                let value = self.heap[region].slots[i].clone();
                Ok(Some(if self.has_copyinit {
                    self.clone_value(prog, &value)?
                } else {
                    value
                }))
            }
            _ => Ok(None),
        }
    }

    /// Call a free function that has `mut`/`ref` parameters, writing each one's
    /// final value back to the caller's argument place (`arg_places`). This is the
    /// runtime half of call-scoped reference parameters, performed over the
    /// caller's frame (`regs`/`vars`).
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
            Some(sig) => self.bind_for_call(prog, name, sig, argv, kwargs)?,
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

    fn bind_for_call(
        &mut self,
        prog: &Prog,
        name: &str,
        sig: &FnSig,
        argv: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(Vec<Value>, Vec<ArgSlot>), RuntimeError> {
        let mut expanded = Vec::new();
        let mut forwarded = false;
        for (name, value) in kwargs {
            if name == crate::ast::FORWARDED_KWARGS_NAME {
                if forwarded {
                    return Err(RuntimeError::TypeError(
                        "a call may forward only one StringDict".to_string(),
                    ));
                }
                forwarded = true;
                expanded.extend(self.take_forwarded_kwargs(value)?);
            } else {
                expanded.push((name, value));
            }
        }
        let kwargs = expanded;
        let collected: Vec<(String, Value)> = if sig.kw_variadic.is_some() {
            kwargs
                .iter()
                .filter(|(key, _)| !sig.param_names.contains(key))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let (mut bound, slots) = bind_args(name, sig, argv, kwargs)?;
        if let Some(index) = sig.kw_variadic_index {
            bound[index] = self.make_kwargs_dict(prog, collected)?;
        }
        Ok((bound, slots))
    }

    fn make_kwargs_dict(
        &mut self,
        prog: &Prog,
        entries: Vec<(String, Value)>,
    ) -> Result<Value, RuntimeError> {
        let mut dict =
            self.construct_via_init(prog, "StringDict", None, Vec::new(), Vec::new(), &[])?;
        let fname = prog.overload_name("StringDict.__setitem__", 2);
        let fidx = prog.index_of(&fname).ok_or_else(|| {
            RuntimeError::Unsupported("vm: kwargs StringDict has no __setitem__".to_string())
        })?;
        for (key, value) in entries {
            let (_, frame) =
                self.call_frame(prog, fidx, vec![dict, Value::Str(key), value], &[])?;
            dict = frame.into_iter().next().unwrap_or(Value::None);
        }
        Ok(dict)
    }

    /// Consume the self-hosted `StringDict` passed by `**kwargs^` and recover its
    /// insertion-ordered key/value entries. Its `entries` field is a self-hosted
    /// `List[DictEntry[String, V]]`, so taking the pointer slots is a true move:
    /// values are not copied and the transferred dictionary cannot be reused.
    fn take_forwarded_kwargs(
        &mut self,
        value: Value,
    ) -> Result<Vec<(String, Value)>, RuntimeError> {
        let Value::Struct { name, fields, .. } = value else {
            return Err(RuntimeError::TypeError(
                "`**kwargs^` requires a StringDict value".to_string(),
            ));
        };
        if name != "StringDict" {
            return Err(RuntimeError::TypeError(format!(
                "`**kwargs^` requires StringDict, got {name}"
            )));
        }
        let entries = fields
            .into_iter()
            .find_map(|(field, value)| (field == "entries").then_some(value))
            .ok_or_else(|| {
                RuntimeError::TypeError("StringDict has no entries storage".to_string())
            })?;
        let Value::Struct { fields, .. } = entries else {
            return Err(RuntimeError::TypeError(
                "StringDict entries storage is not a List".to_string(),
            ));
        };
        let mut data = None;
        let mut size = None;
        for (field, value) in fields {
            match (field.as_str(), value) {
                ("data", Value::Pointer { allocation, offset }) => {
                    data = Some((allocation, offset));
                }
                ("size", Value::Int(value)) => size = Some(value),
                _ => {}
            }
        }
        let (allocation, base) = data.ok_or_else(|| {
            RuntimeError::TypeError("StringDict entry List has no data pointer".to_string())
        })?;
        let size = size.ok_or_else(|| {
            RuntimeError::TypeError("StringDict entry List has no size".to_string())
        })?;
        let mut result = Vec::with_capacity(size.max(0) as usize);
        for offset in 0..size {
            let (region, slot) = self.heap_index(allocation, base, offset)?;
            let entry = std::mem::replace(&mut self.heap[region].slots[slot], Value::Moved);
            let Value::Struct { fields, .. } = entry else {
                return Err(RuntimeError::TypeError(
                    "StringDict contains a non-entry value".to_string(),
                ));
            };
            let mut key = None;
            let mut value = None;
            for (field, field_value) in fields {
                match field.as_str() {
                    "key" => key = Some(field_value),
                    "value" => value = Some(field_value),
                    _ => {}
                }
            }
            let Some(Value::Str(key)) = key else {
                return Err(RuntimeError::TypeError(
                    "StringDict entry key is not a String".to_string(),
                ));
            };
            let value = value.ok_or_else(|| {
                RuntimeError::TypeError("StringDict entry has no value".to_string())
            })?;
            result.push((key, value));
        }
        Ok(result)
    }

    /// Execute one straight-line MIR instruction against the current frame.
    /// Returns the control-flow outcome — `Normal`, or `Return` when a `return`
    /// inside a nested `try` region crosses out (all other instructions are
    /// `Normal`).
    fn exec_instr(
        &mut self,
        prog: &Prog,
        i: &MirInstr,
        frame_id: FrameId,
        regs: &mut [Value],
        vars: &mut [Value],
    ) -> Result<Flow, RuntimeError> {
        self.burn_ctfe()?;
        match i {
            MirInstr::BeginLoan { .. } => {}
            MirInstr::MakeRef { dest, place } => {
                let root = vars[place.root as usize].clone();
                let (frame, slot, mut projection) = match root {
                    Value::Ref {
                        frame,
                        slot,
                        projection,
                    } => (frame, slot, projection),
                    _ => (frame_id.0, place.root as usize, Vec::new()),
                };
                for segment in &place.proj {
                    projection.push(match segment {
                        Proj::Field(name) => RefProjection::Field(name.clone()),
                        Proj::Index(register) => RefProjection::Index(value_as_index(
                            &regs[register.0 as usize],
                        )?
                            as usize),
                        Proj::Variant(index) => RefProjection::Variant(*index),
                    });
                }
                regs[dest.0 as usize] = Value::Ref {
                    frame,
                    slot,
                    projection,
                };
            }
            MirInstr::ReadRef { dest, reference } => {
                regs[dest.0 as usize] =
                    self.read_reference(&regs[reference.0 as usize], frame_id, vars)?;
            }
            MirInstr::WriteRef { reference, value } => {
                let handle = regs[reference.0 as usize].clone();
                self.write_reference(&handle, frame_id, vars, regs[value.0 as usize].clone())?;
            }
            MirInstr::MakeClosure {
                dest,
                function,
                captures,
            } => {
                let mut environment = Vec::with_capacity(captures.len());
                for capture in captures {
                    if capture.moved {
                        let slot = capture.place.root as usize;
                        environment.push(std::mem::replace(&mut vars[slot], Value::Moved));
                    } else if let Some(reference) = self.extend_reference(
                        &vars[capture.place.root as usize],
                        &capture.place.proj,
                        regs,
                    )? {
                        environment.push(reference);
                    } else {
                        environment.push(Value::Ref {
                            frame: frame_id.0,
                            slot: capture.place.root as usize,
                            projection: Vec::new(),
                        });
                    }
                }
                regs[dest.0 as usize] = Value::Closure {
                    function: function.clone(),
                    captures: environment,
                };
            }
            MirInstr::KeepAlive { .. } => {}
            MirInstr::Const { dest, k } => regs[dest.0 as usize] = const_value(k),
            MirInstr::UseVar { dest, var, mode } => {
                let slot = *var as usize;
                // A `^` move **transfers** the value out of the source slot, leaving
                // a `Moved` tombstone; any other use copies. Either way, touching an
                // already-moved slot is a use-after-move — a loud runtime error (the
                // ownership analysis rejects this statically, so this only fires on a
                // compiler bug).
                let value = if let Value::Ref { .. } = &vars[slot] {
                    self.read_reference(&vars[slot], frame_id, vars)?
                } else {
                    match mode {
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
                    }
                };
                if matches!(value, Value::Moved) {
                    return Err(RuntimeError::TypeError(format!(
                        "vm: use of variable slot {slot} after it was moved"
                    )));
                }
                regs[dest.0 as usize] = value;
            }
            MirInstr::DefVar {
                var,
                src,
                binding_ty,
            } => {
                let v = regs[src.0 as usize].clone();
                let slot = *var as usize;
                let current = if let Value::Ref { .. } = &vars[slot] {
                    self.read_reference(&vars[slot], frame_id, vars)?
                } else {
                    vars[slot].clone()
                };
                let assigned = match binding_ty {
                    Some(t) => crate::runtime::coerce_checked(v, t),
                    None => crate::runtime::coerce_like(v, &current),
                };
                if let Value::Ref { .. } = &vars[slot] {
                    let handle = vars[slot].clone();
                    self.write_reference(&handle, frame_id, vars, assigned)?;
                } else {
                    vars[slot] = assigned;
                }
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
                ..
            } => {
                let mut argv: Vec<Value> =
                    args.iter().map(|r| regs[r.0 as usize].clone()).collect();
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
                // A handwritten constructor receives reference arguments as
                // caller-frame handles, just like an ordinary ref-parameter call.
                // Its synthetic `self` occupies parameter slot zero.
                let constructor_index =
                    if let Some(struct_name) = crate::symbol::init_overload_struct(&func.0) {
                        prog.structs
                            .contains_key(struct_name)
                            .then(|| prog.index_of(&func.0))
                            .flatten()
                    } else if prog.structs.contains_key(&func.0) {
                        let init_name = format!("{}.__init__", func.0);
                        prog.index_of(&init_name)
                            .or_else(|| prog.index_of(&prog.overload_name(&init_name, args.len())))
                    } else {
                        None
                    };
                if let Some(index) = constructor_index {
                    let reference_parameters = &prog.mir.functions[index].1.ref_params;
                    for (argument, value) in argv.iter_mut().enumerate() {
                        if !reference_parameters
                            .get(argument + 1)
                            .copied()
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        let place = arg_places
                            .get(argument)
                            .and_then(Option::as_ref)
                            .ok_or_else(|| {
                                RuntimeError::Unsupported(format!(
                                    "vm: reference constructor argument {} to '{}' must be a place",
                                    argument + 1,
                                    func.0
                                ))
                            })?;
                        *value = Self::reference_to_place_parts(frame_id, regs, vars, place)?;
                    }
                }
                // A free function with `mut`/`ref` parameters writes each one's final
                // value back to the caller's argument place after the call.
                let writeback = constructor_index
                    .is_none()
                    .then(|| {
                        prog.index_of(&func.0)
                            .filter(|&idx| prog.mir.functions[idx].1.ref_params.iter().any(|&r| r))
                    })
                    .flatten();
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
            MirInstr::CallIndirect {
                dest,
                callee,
                args,
                kwargs,
                ..
            } => {
                let callable = regs[callee.0 as usize].clone();
                let (function, mut positional) = match &callable {
                    Value::Function(function) => (function.clone(), Vec::new()),
                    Value::Closure { function, captures } => (function.clone(), captures.clone()),
                    Value::Struct { name, .. } => (
                        prog.overload_name(&format!("{name}.__call__"), args.len()),
                        vec![callable.clone()],
                    ),
                    value => {
                        return Err(RuntimeError::NotCallable(crate::runtime::type_name(value)));
                    }
                };
                positional.extend(
                    args.iter()
                        .map(|register| regs[register.0 as usize].clone()),
                );
                let keywords = kwargs
                    .iter()
                    .map(|(name, register)| (name.clone(), regs[register.0 as usize].clone()))
                    .collect();
                let index = prog
                    .index_of(&function)
                    .ok_or_else(|| RuntimeError::NotCallable(function.clone()))?;
                let bound = if matches!(callable, Value::Struct { .. }) {
                    let receiver = positional.remove(0);
                    let mut args = match prog.sigs.get(&function) {
                        Some(signature) => {
                            self.bind_for_call(prog, &function, signature, positional, keywords)?
                                .0
                        }
                        None => positional,
                    };
                    args.insert(0, receiver);
                    args
                } else {
                    match prog.sigs.get(&function) {
                        Some(signature) => {
                            self.bind_for_call(prog, &function, signature, positional, keywords)?
                                .0
                        }
                        None => positional,
                    }
                };
                regs[dest.0 as usize] = self.call_function(prog, index, bound, &[])?;
            }
            MirInstr::MethodCall {
                dest,
                recv,
                method,
                resolved,
                args,
                kwargs,
                recv_place,
                arg_places,
                ..
            } => {
                let recv_val = regs[recv.0 as usize].clone();
                let argv: Vec<Value> = args.iter().map(|r| regs[r.0 as usize].clone()).collect();
                let kw: Vec<(String, Value)> = kwargs
                    .iter()
                    .map(|(name, reg)| (name.clone(), regs[reg.0 as usize].clone()))
                    .collect();
                regs[dest.0 as usize] = self.method_call(
                    prog,
                    MethodInvocation {
                        receiver: recv_val,
                        method,
                        resolved_name: resolved.as_deref(),
                        arguments: argv,
                        keyword_arguments: kw,
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
                    Value::Pointer { allocation, offset } => {
                        let allocation = *allocation;
                        let base = *offset;
                        let off = self.normalize_index(prog, &regs[index.0 as usize])?;
                        let (region, i) = self.heap_index(allocation, base, off)?;
                        let value = self.heap[region].slots[i].clone();
                        regs[dest.0 as usize] = if self.has_copyinit {
                            self.clone_value(prog, &value)?
                        } else {
                            value
                        };
                    }
                    Value::Dict(entries) => {
                        let key = &regs[index.0 as usize];
                        regs[dest.0 as usize] = entries
                            .iter()
                            .find(|(candidate, _)| candidate == key)
                            .map(|(_, value)| value.clone())
                            .ok_or_else(|| {
                                RuntimeError::TypeError("dictionary key not found".to_string())
                            })?;
                    }
                    _ => {
                        let idx = self.normalize_index(prog, &regs[index.0 as usize])?;
                        regs[dest.0 as usize] = index_value(&regs[base.0 as usize], idx)?;
                    }
                }
            }
            MirInstr::Slice {
                dest,
                object,
                kind,
                lower,
                upper,
                step,
                resolved,
            } => {
                let bound = |b: &Option<Reg>| -> Result<Option<i64>, RuntimeError> {
                    match b {
                        Some(r) => Ok(Some(value_as_index(&regs[r.0 as usize])?)),
                        None => Ok(None),
                    }
                };
                let (lo, hi, st) = (bound(lower)?, bound(upper)?, bound(step)?);
                let receiver = regs[object.0 as usize].clone();
                regs[dest.0 as usize] = if let Value::Struct { name, .. } = &receiver {
                    let slice = Value::Slice {
                        kind: *kind,
                        start: lo,
                        end: hi,
                        step: st,
                    };
                    let _ = name;
                    self.call_subscript_dunder(prog, receiver, vec![slice], resolved.as_deref())?
                } else {
                    crate::runtime::slice_value(&receiver, lo, hi, st)?
                };
            }
            MirInstr::MultiIndex {
                dest,
                object,
                args,
                resolved,
            } => {
                let bound = |bound: &Option<Reg>| -> Result<Option<i64>, RuntimeError> {
                    bound
                        .map(|register| value_as_index(&regs[register.0 as usize]))
                        .transpose()
                };
                let mut arguments = Vec::with_capacity(args.len());
                for argument in args {
                    arguments.push(match argument {
                        MirSubscriptArg::Index(register) => regs[register.0 as usize].clone(),
                        MirSubscriptArg::Slice {
                            kind,
                            lower,
                            upper,
                            step,
                        } => Value::Slice {
                            kind: *kind,
                            start: bound(lower)?,
                            end: bound(upper)?,
                            step: bound(step)?,
                        },
                    });
                }
                regs[dest.0 as usize] = self.call_subscript_dunder(
                    prog,
                    regs[object.0 as usize].clone(),
                    arguments,
                    resolved.as_deref(),
                )?;
            }
            MirInstr::MultiSet {
                receiver_place,
                args,
                value,
                value_keyword,
                resolved,
            } => {
                let bound = |bound: &Option<Reg>| -> Result<Option<i64>, RuntimeError> {
                    bound
                        .map(|register| value_as_index(&regs[register.0 as usize]))
                        .transpose()
                };
                let mut arguments = Vec::with_capacity(args.len() + usize::from(!value_keyword));
                for argument in args {
                    arguments.push(match argument {
                        MirSubscriptArg::Index(register) => regs[register.0 as usize].clone(),
                        MirSubscriptArg::Slice {
                            kind,
                            lower,
                            upper,
                            step,
                        } => Value::Slice {
                            kind: *kind,
                            start: bound(lower)?,
                            end: bound(upper)?,
                            step: bound(step)?,
                        },
                    });
                }
                let keyword_arguments = if *value_keyword {
                    vec![("value".to_string(), regs[value.0 as usize].clone())]
                } else {
                    arguments.push(regs[value.0 as usize].clone());
                    Vec::new()
                };
                let argument_places = vec![None; arguments.len()];
                let receiver = nav_mut(vars, regs, receiver_place)?.clone();
                let place = Some(receiver_place.clone());
                let _ = self.method_call(
                    prog,
                    MethodInvocation {
                        receiver,
                        method: "__setitem__",
                        resolved_name: resolved.as_deref(),
                        arguments,
                        keyword_arguments,
                        receiver_place: &place,
                        argument_places: &argument_places,
                    },
                    CallerFrame {
                        registers: regs,
                        variables: vars,
                    },
                )?;
            }
            MirInstr::MakeList {
                dest,
                elems,
                element_type,
            } => {
                let mut items: Vec<Value> =
                    elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                if let Some(element_type) = element_type {
                    items = items
                        .into_iter()
                        .map(|value| crate::runtime::coerce_checked(value, element_type))
                        .collect();
                } else {
                    promote_numeric_elems(&mut items); // unify literal element kinds
                }
                regs[dest.0 as usize] = Value::List(items);
            }
            MirInstr::MakeSet {
                dest,
                elems,
                element_type,
            } => {
                let mut raw: Vec<Value> = elems
                    .iter()
                    .map(|register| regs[register.0 as usize].clone())
                    .collect();
                if let Some(element_type) = element_type {
                    raw = raw
                        .into_iter()
                        .map(|value| crate::runtime::coerce_checked(value, element_type))
                        .collect();
                } else {
                    promote_numeric_elems(&mut raw);
                }
                let mut items = Vec::with_capacity(raw.len());
                for item in raw {
                    if items.iter().any(|candidate| {
                        values_equal(candidate, &item).unwrap_or_else(|_| candidate == &item)
                    }) {
                        self.drop_value(prog, item)?;
                    } else {
                        items.push(item);
                    }
                }
                regs[dest.0 as usize] = Value::Set(items);
            }
            MirInstr::MakeDict {
                dest,
                entries,
                key_type,
                value_type,
            } => {
                let mut keys = entries
                    .iter()
                    .map(|(key, _)| regs[key.0 as usize].clone())
                    .collect::<Vec<_>>();
                let mut values = entries
                    .iter()
                    .map(|(_, value)| regs[value.0 as usize].clone())
                    .collect::<Vec<_>>();
                if let Some(key_type) = key_type {
                    keys = keys
                        .into_iter()
                        .map(|value| crate::runtime::coerce_checked(value, key_type))
                        .collect();
                } else {
                    promote_numeric_elems(&mut keys);
                }
                if let Some(value_type) = value_type {
                    values = values
                        .into_iter()
                        .map(|value| crate::runtime::coerce_checked(value, value_type))
                        .collect();
                } else {
                    promote_numeric_elems(&mut values);
                }
                let mut result = Vec::with_capacity(entries.len());
                for (key, value) in keys.into_iter().zip(values) {
                    if let Some(position) = result.iter().position(|(candidate, _)| {
                        values_equal(candidate, &key).unwrap_or_else(|_| candidate == &key)
                    }) {
                        let old = std::mem::replace(&mut result[position].1, value);
                        self.drop_value(prog, old)?;
                        self.drop_value(prog, key)?;
                    } else {
                        result.push((key, value));
                    }
                }
                regs[dest.0 as usize] = Value::Dict(result);
            }
            MirInstr::CollectionInsert {
                collection,
                key,
                value,
            } => {
                let value = regs[value.0 as usize].clone();
                match (&mut vars[*collection as usize], key) {
                    (Value::List(items), None) => items.push(value),
                    (Value::Set(items), None) => {
                        if items.iter().any(|candidate| {
                            values_equal(candidate, &value).unwrap_or_else(|_| candidate == &value)
                        }) {
                            self.drop_value(prog, value)?;
                        } else {
                            items.push(value);
                        }
                    }
                    (Value::Dict(entries), Some(key)) => {
                        let key = regs[key.0 as usize].clone();
                        if let Some(position) = entries.iter().position(|(candidate, _)| {
                            values_equal(candidate, &key).unwrap_or_else(|_| candidate == &key)
                        }) {
                            let old = std::mem::replace(&mut entries[position].1, value);
                            self.drop_value(prog, old)?;
                            self.drop_value(prog, key)?;
                        } else {
                            entries.push((key, value));
                        }
                    }
                    (collection, _) => {
                        return Err(RuntimeError::TypeError(format!(
                            "vm: collection-comprehension insertion does not match {}",
                            crate::runtime::type_name(collection)
                        )));
                    }
                }
            }
            MirInstr::MakeTuple {
                dest,
                elems,
                element_types,
            } => {
                let raw: Vec<Value> = elems.iter().map(|r| regs[r.0 as usize].clone()).collect();
                let items = match element_types {
                    Some(types) if types.len() == raw.len() => raw
                        .into_iter()
                        .zip(types)
                        .map(|(value, ty)| crate::runtime::coerce_checked(value, ty))
                        .collect(),
                    _ => raw,
                };
                regs[dest.0 as usize] = Value::Tuple(items);
            }
            MirInstr::MakeVariant {
                dest,
                alternatives,
                index,
                value,
            } => {
                let selected = alternatives.get(*index).ok_or_else(|| {
                    RuntimeError::TypeError("Variant construction has an invalid tag".to_string())
                })?;
                let payload =
                    crate::runtime::coerce_checked(regs[value.0 as usize].clone(), selected);
                regs[dest.0 as usize] = Value::Variant {
                    alternatives: alternatives.clone(),
                    index: *index,
                    value: Box::new(payload),
                };
            }
            MirInstr::VariantIs {
                dest,
                variant,
                index,
            } => {
                let Value::Variant {
                    alternatives,
                    index: active,
                    ..
                } = &regs[variant.0 as usize]
                else {
                    return Err(RuntimeError::TypeError(format!(
                        "Variant.isa applied to {}",
                        crate::runtime::type_name(&regs[variant.0 as usize])
                    )));
                };
                if *index >= alternatives.len() {
                    return Err(RuntimeError::TypeError(
                        "Variant.isa has an invalid checked tag".to_string(),
                    ));
                }
                regs[dest.0 as usize] = Value::Bool(active == index);
            }
            MirInstr::VariantGet {
                dest,
                variant,
                index,
            } => {
                let Value::Variant {
                    alternatives,
                    index: active,
                    value,
                } = &regs[variant.0 as usize]
                else {
                    return Err(RuntimeError::TypeError(format!(
                        "typed Variant projection applied to {}",
                        crate::runtime::type_name(&regs[variant.0 as usize])
                    )));
                };
                let expected = alternatives.get(*index).ok_or_else(|| {
                    RuntimeError::TypeError(
                        "typed Variant projection has an invalid checked tag".to_string(),
                    )
                })?;
                if active != index {
                    let found = alternatives
                        .get(*active)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "<invalid>".to_string());
                    return Err(RuntimeError::TypeError(format!(
                        "Variant holds '{found}', not '{expected}'"
                    )));
                }
                regs[dest.0 as usize] = value.as_ref().clone();
            }
            MirInstr::VariantTake {
                dest,
                variant,
                index,
                checked,
            } => {
                let Value::Variant {
                    alternatives,
                    index: active,
                    value,
                } = &regs[variant.0 as usize]
                else {
                    return Err(RuntimeError::TypeError(format!(
                        "Variant.take applied to {}",
                        crate::runtime::type_name(&regs[variant.0 as usize])
                    )));
                };
                let expected = alternatives.get(*index).ok_or_else(|| {
                    RuntimeError::TypeError("Variant.take has an invalid checked tag".to_string())
                })?;
                if *checked && active != index {
                    let found = alternatives
                        .get(*active)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "<invalid>".to_string());
                    return Err(RuntimeError::TypeError(format!(
                        "Variant holds '{found}', not '{expected}'"
                    )));
                }
                // The receiver place was moved to a tombstone before this
                // instruction, so ownership of the payload is transferred out.
                regs[dest.0 as usize] = value.as_ref().clone();
            }
            MirInstr::VariantSet {
                dest,
                place,
                index,
                value,
            } => {
                let old = load_place(vars, regs, place)?;
                let Value::Variant { alternatives, .. } = &old else {
                    return Err(RuntimeError::TypeError(format!(
                        "Variant.set applied to {}",
                        crate::runtime::type_name(&old)
                    )));
                };
                let selected = alternatives.get(*index).ok_or_else(|| {
                    RuntimeError::TypeError("Variant.set has an invalid checked tag".to_string())
                })?;
                let replacement = Value::Variant {
                    alternatives: alternatives.clone(),
                    index: *index,
                    value: Box::new(crate::runtime::coerce_checked(
                        regs[value.0 as usize].clone(),
                        selected,
                    )),
                };
                self.store_at_place(prog, place, replacement, regs, vars)?;
                self.drop_value(prog, old)?;
                regs[dest.0 as usize] = Value::None;
            }
            MirInstr::VariantReplace {
                dest,
                place,
                input_index,
                output_index,
                value,
                checked,
            } => {
                let old = load_place(vars, regs, place)?;
                let Value::Variant {
                    alternatives,
                    index: active,
                    value: old_payload,
                } = old
                else {
                    return Err(RuntimeError::TypeError(format!(
                        "Variant.replace applied to {}",
                        crate::runtime::type_name(&old)
                    )));
                };
                let input = alternatives.get(*input_index).cloned().ok_or_else(|| {
                    RuntimeError::TypeError("Variant.replace has an invalid input tag".to_string())
                })?;
                let output = alternatives.get(*output_index).cloned().ok_or_else(|| {
                    RuntimeError::TypeError("Variant.replace has an invalid output tag".to_string())
                })?;
                if *checked && active != *output_index {
                    let found = alternatives
                        .get(active)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "<invalid>".to_string());
                    return Err(RuntimeError::TypeError(format!(
                        "Variant holds '{found}', not '{output}'"
                    )));
                }
                let replacement = Value::Variant {
                    alternatives,
                    index: *input_index,
                    value: Box::new(crate::runtime::coerce_checked(
                        regs[value.0 as usize].clone(),
                        &input,
                    )),
                };
                self.store_at_place(prog, place, replacement, regs, vars)?;
                regs[dest.0 as usize] = *old_payload;
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
                if let Some(reference) =
                    self.extend_reference(&vars[place.root as usize], &place.proj, regs)?
                {
                    self.write_reference(&reference, frame_id, vars, v)?;
                } else if matches!(place.ty, Some(Ty::Ref(_))) {
                    let reference = load_place(vars, regs, place)?.clone();
                    self.write_reference(&reference, frame_id, vars, v)?;
                } else {
                    self.store_at_place(prog, place, v, regs, vars)?;
                }
            }
            MirInstr::StoreRef { place, reference } => {
                let handle = regs[reference.0 as usize].clone();
                if !matches!(handle, Value::Ref { .. }) {
                    return Err(RuntimeError::TypeError(
                        "vm: reference storage requires a reference handle".to_string(),
                    ));
                }
                self.store_at_place(prog, place, handle, regs, vars)?;
            }
            MirInstr::LoadPlace { dest, place } => {
                // The read half of `c[i] += e` on a user struct goes through
                // `c.__getitem__(i)`; any other place reads its slot / SIMD lane.
                regs[dest.0 as usize] = if let Some(reference) =
                    self.extend_reference(&vars[place.root as usize], &place.proj, regs)?
                {
                    self.read_reference(&reference, frame_id, vars)?
                } else {
                    match self.load_index_dunder(prog, place, regs, vars)? {
                        Some(v) => v,
                        None => {
                            let value = load_place(vars, regs, place)?;
                            if matches!(place.ty, Some(Ty::Ref(_))) {
                                self.read_reference(&value, frame_id, vars)?
                            } else {
                                value.clone()
                            }
                        }
                    }
                };
            }
            MirInstr::MovePlace { dest, place } => {
                // A partial move `p.a^`: transfer the field's value out, leaving a
                // `Moved` tombstone so a later drop of the whole struct skips it (no
                // double-drop) and any stray use fails loudly. The ownership analysis
                // has already proven the moved field is not read again.
                let reference =
                    self.extend_reference(&vars[place.root as usize], &place.proj, regs)?;
                let value = if let Some(reference) = reference {
                    let old = self.read_reference(&reference, frame_id, vars)?;
                    self.write_reference(&reference, frame_id, vars, Value::Moved)?;
                    old
                } else {
                    std::mem::replace(nav_mut(vars, regs, place)?, Value::Moved)
                };
                if matches!(value, Value::Moved) {
                    return Err(RuntimeError::TypeError(
                        "vm: partial use of an already-moved place".into(),
                    ));
                }
                regs[dest.0 as usize] = value;
            }
            // Iterator protocol (`for`). Range: counter with step direction. List:
            // consume the (copied) list from the front, preserving order.
            MirInstr::GetIter {
                iter,
                mode: _,
                prepare,
            } => {
                // Execute the exact normalization chain chosen by the checker.
                // Builtin ranges/collections carry an empty chain.
                let slot = *iter as usize;
                let dynamic_prepare = prepare
                    .iter()
                    .find(|symbol| symbol.starts_with("__trait_dispatch."))
                    .cloned();
                for selected in prepare {
                    let Value::Struct { name, .. } = &vars[slot] else {
                        return Err(RuntimeError::TypeError(format!(
                            "vm: checked iterator preparation applied to {}",
                            crate::runtime::type_name(&vars[slot])
                        )));
                    };
                    let sname = name.clone();
                    let target =
                        prog.runtime_method_name(&sname, "__iter__", Some(selected.as_str()), 0);
                    let fidx = prog.index_of(&target).ok_or_else(|| {
                        RuntimeError::Unsupported(format!(
                            "vm: checked iterator method '{target}' is missing from MIR"
                        ))
                    })?;
                    let (value, _) = self.call_frame(prog, fidx, vec![vars[slot].clone()], &[])?;
                    vars[slot] = value;
                }
                // A bounded `Iterable` may expose another iterable as its
                // associated `Iter` (the self-hosted Set yields a List). Its
                // concrete normalization depth is known only after generic
                // specialization, so repeat the checked trait operation until
                // the runtime type is an iterator. Concrete source types carry
                // their complete static `prepare` chain and skip this path.
                if let Some(selected) = dynamic_prepare {
                    for _ in 0..8 {
                        let Value::Struct { name, .. } = &vars[slot] else {
                            break;
                        };
                        let sname = name.clone();
                        let next = prog.runtime_method_name(&sname, "__next__", None, 0);
                        if prog.index_of(&next).is_some() {
                            break;
                        }
                        let target = prog.runtime_method_name(
                            &sname,
                            "__iter__",
                            Some(selected.as_str()),
                            0,
                        );
                        let fidx = prog.index_of(&target).ok_or_else(|| {
                            RuntimeError::Unsupported(format!(
                                "vm: checked iterator method '{target}' is missing from MIR"
                            ))
                        })?;
                        vars[slot] =
                            self.call_function(prog, fidx, vec![vars[slot].clone()], &[])?;
                    }
                }
            }
            MirInstr::HasNext { dest, iter, method } => {
                let slot = *iter as usize;
                // A struct iterator reports remaining length via `__len__` (bounded
                // iteration): more elements iff `len(it) > 0`.
                let has = if let Some(selected) = method {
                    let Value::Struct { name, .. } = &vars[slot] else {
                        return Err(RuntimeError::TypeError(format!(
                            "vm: checked iterator length applied to {}",
                            crate::runtime::type_name(&vars[slot])
                        )));
                    };
                    let sname = name.clone();
                    let target =
                        prog.runtime_method_name(&sname, "__len__", Some(selected.as_str()), 0);
                    let fidx = prog.index_of(&target).ok_or_else(|| {
                        RuntimeError::Unsupported(format!(
                            "vm: checked iterator method '{target}' is missing from MIR"
                        ))
                    })?;
                    match self
                        .call_frame(prog, fidx, vec![vars[slot].clone()], &[])?
                        .0
                    {
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
                        Value::List(items) | Value::Set(items) => !items.is_empty(),
                        Value::Dict(entries) => !entries.is_empty(),
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
            MirInstr::Next { dest, iter, method } => {
                let slot = *iter as usize;
                // A struct iterator advances via `__next__(mut self)`: the element is
                // the return value, and the advanced iterator (frame slot 0) is
                // written back into the iterator variable.
                if let Some(selected) = method {
                    let Value::Struct { name, .. } = &vars[slot] else {
                        return Err(RuntimeError::TypeError(format!(
                            "vm: checked iterator next applied to {}",
                            crate::runtime::type_name(&vars[slot])
                        )));
                    };
                    let sname = name.clone();
                    let target =
                        prog.runtime_method_name(&sname, "__next__", Some(selected.as_str()), 0);
                    let fidx = prog.index_of(&target).ok_or_else(|| {
                        RuntimeError::Unsupported(format!(
                            "vm: checked iterator method '{target}' is missing from MIR"
                        ))
                    })?;
                    let (ret, frame_vars) =
                        self.call_frame(prog, fidx, vec![vars[slot].clone()], &[])?;
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
                        Value::Set(items) => {
                            regs[dest.0 as usize] = items.remove(0);
                        }
                        Value::Dict(entries) => {
                            let (key, value) = entries.remove(0);
                            regs[dest.0 as usize] = key;
                            self.drop_value(prog, value)?;
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
            // ASAP destruction (Stage 7): drop the value at the variable's last
            // use, running its `__del__` if it has one.
            MirInstr::DropVar { var } => {
                let v = std::mem::replace(&mut vars[*var as usize], Value::None);
                self.drop_value(prog, v)?;
            }
            MirInstr::ConsumeVar { var } => {
                let value = std::mem::replace(&mut vars[*var as usize], Value::Moved);
                if let Value::Struct { fields, .. } = value {
                    // The named explicit destructor has already consumed the
                    // aggregate. Its fields still receive their ordinary
                    // reverse-order destruction after that call succeeds.
                    for (_, field) in fields.into_iter().rev() {
                        self.drop_value(prog, field)?;
                    }
                }
            }
            MirInstr::ConsumePlace { place, .. } => {
                let value = std::mem::replace(nav_mut(vars, regs, place)?, Value::Moved);
                if let Value::Struct { fields, .. } = value {
                    for (_, field) in fields.into_iter().rev() {
                        self.drop_value(prog, field)?;
                    }
                }
            }
            MirInstr::Unsupported(what) => {
                return Err(RuntimeError::Unsupported(format!(
                    "vm backend does not support {what} yet"
                )));
            }
            MirInstr::Raise { src } => {
                // Raise an error, propagating as `Raised` — the nearest enclosing
                // `Try` (if any) intercepts it; otherwise it unwinds the frame.
                let error = match &regs[src.0 as usize] {
                    Value::Str(message) => Value::Error(message.clone()),
                    other => other.clone(),
                };
                return Err(RuntimeError::Raised(error));
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
                    frame_id,
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

    /// Execute a `try`/`except`/`else`/`finally` region. Each sub-part runs as a
    /// mini-CFG in the current frame; a raise in
    /// the body unwinds to `handler` (after running the `cleanup` drops), `else` runs
    /// on normal completion, and `finally` always runs (its raise wins).
    fn exec_try(
        &mut self,
        prog: &Prog,
        frame_id: FrameId,
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
        let outcome = match self.run_region(prog, frame_id, body, regs, vars) {
            // The body raised: run the exceptional-edge cleanup (destroy the body's
            // locals as they go out of scope), then dispatch to the handler or
            // re-propagate.
            Err(RuntimeError::Raised(error)) => {
                self.run_cleanup(prog, cleanup, vars)?;
                match handler {
                    Some((err_slot, hblocks)) => {
                        if let Some(slot) = err_slot {
                            vars[*slot as usize] = error.clone();
                        }
                        self.run_region(prog, frame_id, hblocks, regs, vars)
                    }
                    None => Err(RuntimeError::Raised(error)),
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
                        Some(eblocks) => self.run_region(prog, frame_id, eblocks, regs, vars),
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
            match self.run_region(prog, frame_id, fblocks, regs, vars)? {
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
        frame_id: FrameId,
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
                match self.exec_instr(prog, instr, frame_id, regs, vars)? {
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

    /// Dispatch a method call. Builtin `List`/`Tuple` methods take intrinsic
    /// paths; struct methods resolve to the mangled `Type.method` function, with
    /// a `mut self` receiver written back.
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
            keyword_arguments: kwargs,
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
                ("__hash__", 0) => return self.hash_value(prog, recv).map(Value::UInt),
                // `Floorable`/`Ceilable`/`Truncable` — `x.__floor__()` etc.
                // (roadmap milestone 7).
                ("__floor__" | "__ceil__" | "__trunc__", 0) => {
                    return crate::runtime::builtin_round_dir(method, &recv);
                }
                // `CeilDivable` — `x.__ceildiv__(y)`.
                ("__ceildiv__", 1) => return crate::runtime::builtin_ceildiv(&recv, &args[0]),
                _ => {}
            }
        }
        match &recv {
            Value::UInt(state) if method == "update" && args.len() == 1 => {
                let place = recv_place.as_ref().ok_or_else(|| {
                    RuntimeError::Unsupported("vm: Hasher.update needs a mutable place".into())
                })?;
                let part = self.hash_value(prog, args[0].clone())?;
                self.store_at_place(
                    prog,
                    place,
                    Value::UInt(state.wrapping_mul(33).wrapping_add(part)),
                    regs,
                    vars,
                )?;
                Ok(Value::None)
            }
            Value::Str(template) if method == "format" => {
                self.format_template(prog, template, &args).map(Value::Str)
            }
            Value::Str(current) if method == "write" => {
                let place = recv_place.as_ref().ok_or_else(|| {
                    RuntimeError::Unsupported("vm: Writer.write needs a mutable place".into())
                })?;
                let mut text = current.clone();
                for argument in args {
                    text.push_str(&self.format_value(prog, argument, false)?);
                }
                self.store_at_place(prog, place, Value::Str(text), regs, vars)?;
                Ok(Value::None)
            }
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
            Value::Set(_) if method == "add" && args.len() == 1 => {
                let place = recv_place.as_ref().ok_or_else(|| {
                    RuntimeError::Unsupported("vm: Set.add needs a place receiver".into())
                })?;
                match nav_mut(vars, regs, place)? {
                    Value::Set(items) => {
                        if !items.contains(&args[0]) {
                            items.push(args[0].clone());
                        }
                        Ok(Value::None)
                    }
                    other => Err(RuntimeError::TypeError(format!(
                        "vm: Set.add on non-set place, got {}",
                        crate::runtime::type_name(other)
                    ))),
                }
            }
            Value::Tuple(items) => match (method, args.as_slice()) {
                ("reverse", []) => Ok(Value::Tuple(items.iter().rev().cloned().collect())),
                ("concat", [Value::Tuple(other)]) => {
                    let mut result = items.clone();
                    result.extend(other.iter().cloned());
                    Ok(Value::Tuple(result))
                }
                ("reverse", _) => Err(RuntimeError::ArityMismatch {
                    name: "reverse".to_string(),
                    expected: 0,
                    got: args.len(),
                }),
                ("concat", _) => Err(RuntimeError::TypeError(
                    "vm: Tuple.concat expects one Tuple argument".to_string(),
                )),
                _ => Err(RuntimeError::Unsupported(format!(
                    "vm: Tuple has no method '{method}'"
                ))),
            },
            Value::Slice {
                start, end, step, ..
            } if method == "indices" && args.len() == 1 => {
                let length = value_as_index(&args[0])?;
                let (start, end, step) =
                    crate::runtime::normalize_slice_bounds(length, *start, *end, *step)?;
                Ok(Value::Tuple(vec![
                    Value::Int(start),
                    Value::Int(end),
                    Value::Int(step),
                ]))
            }
            Value::Slice { kind, .. } => Err(RuntimeError::Unsupported(format!(
                "vm: {} has no method '{method}'",
                kind.type_name()
            ))),
            // `UnsafePointer` methods: `free()` releases the allocation (a no-op in
            // the arena model — the arena never reclaims).
            Value::Pointer { allocation, offset } => match method {
                "free" => {
                    self.heap_free(*allocation, *offset)?;
                    Ok(Value::None)
                }
                _ => Err(RuntimeError::Unsupported(format!(
                    "vm: UnsafePointer has no method '{method}'"
                ))),
            },
            Value::Struct { name, .. }
                if method == "write"
                    && prog.index_of(&format!("{name}.write_string")).is_some() =>
            {
                let place = recv_place.as_ref().ok_or_else(|| {
                    RuntimeError::Unsupported("vm: Writer.write needs a mutable place".into())
                })?;
                let mut writer = recv.clone();
                let index = prog
                    .index_of(&format!("{name}.write_string"))
                    .expect("guard established Writer.write_string");
                for argument in args {
                    let text = self.format_value(prog, argument, false)?;
                    let (_, variables) =
                        self.call_frame(prog, index, vec![writer, Value::Str(text)], &[])?;
                    writer = variables.into_iter().next().unwrap_or(Value::None);
                }
                self.store_at_place(prog, place, writer, regs, vars)?;
                Ok(Value::None)
            }
            Value::Struct { name, fields, .. } if name == "Optional" => {
                let values = fields
                    .iter()
                    .find_map(|(field, value)| (field == "values").then_some(value))
                    .and_then(|value| match value {
                        Value::List(values) => Some(values),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        RuntimeError::TypeError(
                            "builtin Optional has invalid values storage".to_string(),
                        )
                    })?;
                match (method, args.as_slice()) {
                    ("is_some", []) => Ok(Value::Bool(values.len() == 1)),
                    ("or_else", [default]) => {
                        Ok(values.first().cloned().unwrap_or_else(|| default.clone()))
                    }
                    _ => Err(RuntimeError::Unsupported(format!(
                        "vm: Optional has no method '{method}'"
                    ))),
                }
            }
            Value::Struct { name, .. } => {
                let method_argc = args.len();
                let source_fname = format!("{name}.{method}");
                let fname = prog.runtime_method_name(name, method, resolved, method_argc);
                let fidx = prog.index_of(&fname).ok_or_else(|| {
                    RuntimeError::Unsupported(format!("vm: unknown method '{fname}'"))
                })?;
                // Bind ordinary arguments through the same signature metadata as
                // free functions, then prepend `self` in frame slot zero.
                let (bound, slots) = match prog.sigs.get(&fname) {
                    Some(signature) => self.bind_for_call(prog, &fname, signature, args, kwargs)?,
                    None => {
                        let slots = (0..args.len()).map(ArgSlot::Positional).collect();
                        (args, slots)
                    }
                };
                let mut call_args = Vec::with_capacity(bound.len() + 1);
                call_args.push(recv.clone());
                call_args.extend(bound);
                let (ret, frame_vars) = self.call_frame(prog, fidx, call_args, &[])?;
                // Write back each `mut`/`ref` *ordinary* parameter (slot ≥ 1; slot 0
                // is `self`, handled below via `recv_place`) to its caller place,
                // reusing the free-function write-back machinery.
                let ref_params = &prog.mir.functions[fidx].1.ref_params;
                for i in 1..ref_params.len() {
                    if !ref_params[i] {
                        continue;
                    }
                    let place = match slots.get(i - 1) {
                        Some(ArgSlot::Positional(position)) => {
                            arg_places.get(*position).and_then(|place| place.as_ref())
                        }
                        _ => None,
                    };
                    match place {
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
                    self.store_at_place(prog, place, frame_vars[0].clone(), regs, vars)?;
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
                if prog
                    .structs
                    .get(&name)
                    .is_some_and(|definition| definition.explicit_destroy)
                {
                    // An intact linear aggregate must be consumed by a named
                    // destructor. Once a field has been moved or explicitly
                    // destroyed, the whole destructor is unavailable; dropping
                    // the residual aggregate destroys only its remaining fields.
                    if fields
                        .iter()
                        .any(|(_, value)| matches!(value, Value::Moved))
                    {
                        for (_, field) in fields.into_iter().rev() {
                            if !matches!(field, Value::Moved) {
                                self.drop_value(prog, field)?;
                            }
                        }
                    }
                    return Ok(());
                }
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
            Value::List(items) | Value::Set(items) | Value::Tuple(items) => {
                for item in items.into_iter().rev() {
                    self.drop_value(prog, item)?;
                }
            }
            Value::Dict(entries) => {
                for (key, value) in entries.into_iter().rev() {
                    self.drop_value(prog, value)?;
                    self.drop_value(prog, key)?;
                }
            }
            Value::Variant { value, .. } => self.drop_value(prog, *value)?,
            _ => {}
        }
        Ok(())
    }

    /// Normalize an `Indexer` to the VM's signed index representation. Int-like
    /// values take the intrinsic path; user conformers execute
    /// `__mlir_index__`, which is the source-level contract even though MIR
    /// represents its result as an `Int` rather than an MLIR index type.
    fn normalize_index(&mut self, prog: &Prog, value: &Value) -> Result<i64, RuntimeError> {
        if let Value::Struct { name, .. } = value {
            let normalized = self.call_dunder(prog, name, "__mlir_index__", vec![value.clone()])?;
            value_as_index(&normalized)
        } else {
            value_as_index(value)
        }
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
            return self.construct_via_init(
                prog,
                struct_name,
                Some(name),
                args,
                kwargs,
                param_vals,
            );
        }
        match name {
            "print" => {
                let mut cells = Vec::with_capacity(args.len());
                for value in args {
                    cells.push(self.format_value(prog, value, false)?);
                }
                self.output.push_str(&cells.join(" "));
                self.output.push('\n');
                Ok(Value::None)
            }
            "String" => Ok(Value::Str(match args.into_iter().next() {
                Some(value) => self.format_value(prog, value, false)?,
                None => String::new(),
            })),
            "repr" => match args.into_iter().next() {
                Some(value) => Ok(Value::Str(self.format_value(prog, value, true)?)),
                None => Err(RuntimeError::ArityMismatch {
                    name: "repr".to_string(),
                    expected: 1,
                    got: 0,
                }),
            },
            "hash" => match args.into_iter().next() {
                Some(value) => Ok(Value::UInt(self.hash_value(prog, value)?)),
                None => Err(RuntimeError::ArityMismatch {
                    name: "hash".to_string(),
                    expected: 1,
                    got: 0,
                }),
            },
            // `len(c)` on a user struct dispatches to `c.__len__()`.
            "len" => match args.into_iter().next() {
                Some(Value::Str(s)) => Ok(Value::Int(s.len() as i64)),
                Some(Value::List(items)) => Ok(Value::Int(items.len() as i64)),
                Some(Value::Set(items)) => Ok(Value::Int(items.len() as i64)),
                Some(Value::Dict(entries)) => Ok(Value::Int(entries.len() as i64)),
                Some(Value::Tuple(items)) => Ok(Value::Int(items.len() as i64)),
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
                    "vm: len supports String, List, Tuple, and structs with __len__".into(),
                )),
            },
            "range" => build_range(&args),
            "Slice" | "slice" => {
                let optional = |value: &Value| match value {
                    Value::Int(value) => Ok(Some(*value)),
                    Value::None => Ok(None),
                    other => Err(RuntimeError::TypeError(format!(
                        "slice bound must be Int or None, got {}",
                        crate::runtime::type_name(other)
                    ))),
                };
                let (start, end, step) = match (name, args.as_slice()) {
                    ("slice", [end]) => (None, optional(end)?, None),
                    ("slice" | "Slice", [start, end]) => (optional(start)?, optional(end)?, None),
                    ("slice" | "Slice", [start, end, step]) => {
                        (optional(start)?, optional(end)?, optional(step)?)
                    }
                    _ => {
                        return Err(RuntimeError::ArityMismatch {
                            name: name.to_string(),
                            expected: if name == "Slice" { 2 } else { 1 },
                            got: args.len(),
                        });
                    }
                };
                Ok(Value::Slice {
                    kind: crate::types::SliceKind::Slice,
                    start,
                    end,
                    step,
                })
            }
            // Utility numeric built-ins use the shared runtime value helpers.
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
            "Int" | "Scalar" | "UInt" | "Float64" | "Bool" => {
                builtin_convert(name, arg1(name, args)?)
            }
            "divmod" => {
                let (a, b) = arg2(name, args)?;
                builtin_divmod(a, b)
            }
            "Error" => builtin_error(arg1(name, args)?),
            // A struct constructor. A hand-written `def __init__(out self, …)`
            // takes precedence over the fieldwise constructor: build an uninitialized
            // `self` skeleton, run `__init__`, and return the initialized value.
            _ if prog.structs.contains_key(name) => {
                let init_name = format!("{name}.__init__");
                if (!args.is_empty() || kwargs.len() != 1 || kwargs[0].0 != "copy")
                    && (prog.index_of(&init_name).is_some()
                        || prog
                            .index_of(&prog.overload_name(&init_name, args.len()))
                            .is_some())
                {
                    return self.construct_via_init(prog, name, None, args, kwargs, param_vals);
                }
                if !kwargs.is_empty() {
                    self.construct_via_copy(prog, name, args, kwargs, param_vals)
                } else if prog
                    .index_of(&prog.overload_name(&init_name, args.len()))
                    .is_some()
                {
                    self.construct_via_init(prog, name, None, args, Vec::new(), param_vals)
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
            // Intrinsic collection constructors use the same insertion
            // semantics as their literal forms.  Parameter types are erased
            // by MIR lowering; the checker has already validated each value.
            "Set" => {
                let mut items = args;
                promote_numeric_elems(&mut items);
                let mut result = Vec::with_capacity(items.len());
                for item in items {
                    let mut duplicate = false;
                    for existing in &result {
                        if values_equal(existing, &item)? {
                            duplicate = true;
                            break;
                        }
                    }
                    if !duplicate {
                        result.push(item);
                    }
                }
                Ok(Value::Set(result))
            }
            "Dict" => {
                if !args.is_empty() {
                    return Err(RuntimeError::ArityMismatch {
                        name: name.to_string(),
                        expected: 0,
                        got: args.len(),
                    });
                }
                Ok(Value::Dict(Vec::new()))
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
                self.heap_alloc(n, std::mem::align_of::<Value>() as i64)
            }
            "UnsafePointer.alloc_aligned" => {
                if args.len() != 2 {
                    return Err(RuntimeError::ArityMismatch {
                        name: name.to_string(),
                        expected: 2,
                        got: args.len(),
                    });
                }
                let Value::Int(n) = args[0] else {
                    return Err(RuntimeError::TypeError(
                        "vm: UnsafePointer.alloc_aligned count must be Int".to_string(),
                    ));
                };
                let Value::Int(alignment) = args[1] else {
                    return Err(RuntimeError::TypeError(
                        "vm: UnsafePointer.alloc_aligned alignment must be Int".to_string(),
                    ));
                };
                self.heap_alloc(n, alignment)
            }
            "UnsafePointer.dangling" => {
                if !args.is_empty() {
                    return Err(RuntimeError::ArityMismatch {
                        name: name.to_string(),
                        expected: 0,
                        got: args.len(),
                    });
                }
                Ok(Value::Pointer {
                    allocation: 0,
                    offset: 0,
                })
            }
            _ => match prog.index_of(name) {
                Some(idx) => {
                    // Match positional + keyword args to the parameter slots (fill
                    // defaults, collect `*args`) when a signature is known; else a
                    // plain positional call.
                    let bound = match prog.sigs.get(name) {
                        Some(sig) => self.bind_for_call(prog, name, sig, args, kwargs)?.0,
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

    fn format_value(
        &mut self,
        prog: &Prog,
        value: Value,
        repr: bool,
    ) -> Result<String, RuntimeError> {
        if let Value::Variant { value, .. } = value {
            return self.format_value(prog, *value, repr);
        }
        let Value::Struct {
            name,
            fields,
            value_params,
        } = value
        else {
            return Ok(value.to_string());
        };
        let method = if repr { "write_repr_to" } else { "write_to" };
        let source = format!("{name}.{method}");
        if let Some(index) = prog.index_of(&prog.overload_name(&source, 1)) {
            let receiver = Value::Struct {
                name,
                fields,
                value_params,
            };
            let (_, variables) =
                self.call_frame(prog, index, vec![receiver, Value::Str(String::new())], &[])?;
            return match variables.get(1) {
                Some(Value::Str(text)) => Ok(text.clone()),
                other => Err(RuntimeError::TypeError(format!(
                    "{source} did not leave a Writer value, got {}",
                    other
                        .map(crate::runtime::type_name)
                        .unwrap_or_else(|| "missing".to_string())
                ))),
            };
        }
        let mut cells = Vec::with_capacity(fields.len());
        for (field, value) in fields {
            cells.push(format!("{field}={}", self.format_value(prog, value, repr)?));
        }
        Ok(format!("{name}({})", cells.join(", ")))
    }

    fn format_template(
        &mut self,
        prog: &Prog,
        template: &str,
        arguments: &[Value],
    ) -> Result<String, RuntimeError> {
        let chars: Vec<char> = template.chars().collect();
        let mut output = String::new();
        let mut automatic = 0usize;
        let mut cursor = 0usize;
        while cursor < chars.len() {
            if chars[cursor] == '{' {
                if chars.get(cursor + 1) == Some(&'{') {
                    output.push('{');
                    cursor += 2;
                    continue;
                }
                let Some(end_offset) = chars[cursor + 1..].iter().position(|ch| *ch == '}') else {
                    return Err(RuntimeError::TypeError("unclosed format field".to_string()));
                };
                let end = cursor + 1 + end_offset;
                let field: String = chars[cursor + 1..end].iter().collect();
                let repr = field.contains("!r");
                let spec = field.split_once(':').map(|(_, spec)| spec).unwrap_or("");
                let selector = field.split(['!', ':']).next().unwrap_or_default();
                let index = if selector.is_empty() {
                    let index = automatic;
                    automatic += 1;
                    index
                } else {
                    selector.parse::<usize>().map_err(|_| {
                        RuntimeError::TypeError(format!("invalid format field '{{{field}}}'"))
                    })?
                };
                let value = arguments
                    .get(index)
                    .ok_or_else(|| RuntimeError::ArityMismatch {
                        name: "String.format".to_string(),
                        expected: index + 1,
                        got: arguments.len(),
                    })?;
                let rendered = self.format_value(prog, value.clone(), repr)?;
                output.push_str(&apply_format_spec(value, &rendered, spec)?);
                cursor = end + 1;
                continue;
            }
            if chars[cursor] == '}' && chars.get(cursor + 1) == Some(&'}') {
                output.push('}');
                cursor += 2;
                continue;
            }
            output.push(chars[cursor]);
            cursor += 1;
        }
        Ok(output)
    }

    fn hash_value(&mut self, prog: &Prog, value: Value) -> Result<u64, RuntimeError> {
        if let Value::Variant { index, value, .. } = value {
            // Include both discriminant and payload. Equal payload bytes in
            // different alternatives must not collapse to the same Variant hash.
            let tag = crate::runtime::builtin_hash(&Value::UInt(index as u64))?;
            return Ok(5381u64
                .wrapping_mul(33)
                .wrapping_add(tag)
                .wrapping_mul(33)
                .wrapping_add(self.hash_value(prog, *value)?));
        }
        let Value::Struct {
            name,
            fields,
            value_params,
        } = value
        else {
            return crate::runtime::builtin_hash(&value);
        };
        let source = format!("{name}.__hash__");
        if let Some(index) = prog.index_of(&prog.overload_name(&source, 1)) {
            let receiver = Value::Struct {
                name,
                fields,
                value_params,
            };
            let (_, variables) =
                self.call_frame(prog, index, vec![receiver, Value::UInt(5381)], &[])?;
            return match variables.get(1) {
                Some(Value::UInt(hash)) => Ok(*hash),
                other => Err(RuntimeError::TypeError(format!(
                    "{source} did not leave a Hasher value, got {}",
                    other
                        .map(crate::runtime::type_name)
                        .unwrap_or_else(|| "missing".to_string())
                ))),
            };
        }
        let mut state = 5381u64;
        for (_, field) in fields {
            state = state
                .wrapping_mul(33)
                .wrapping_add(self.hash_value(prog, field)?);
        }
        Ok(state)
    }
}

impl VmBackend {
    /// Run a checked program, entering through `main()` when present.
    pub fn run(&mut self, program: &crate::checked::CheckedProgram) -> Result<(), RuntimeError> {
        self.run_prog(build_prog_checked(program)?)
    }

    /// Captured standard output.
    pub fn output(&self) -> String {
        self.output.clone()
    }

    /// Final top-level bindings, for the CLI `run` dump.
    pub fn bindings(&self) -> Vec<(String, Value)> {
        self.bindings.clone()
    }
}

impl VmBackend {
    fn run_prog(&mut self, prog: Prog) -> Result<(), RuntimeError> {
        self.configure_lifecycle(&prog);
        // Run module initialization, then `main()`. Capture the top-level frame's
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
}

fn build_prog_checked(checked: &crate::checked::CheckedProgram) -> Result<Prog, RuntimeError> {
    let mut mir =
        crate::analysis::elaborate_drops_program(crate::mir::lower_checked_program(checked));
    // The VM executes the drop-elaborated program, so it is re-verified after
    // the DropVar/edge-cleanup rewrite — the elaborated MIR must satisfy the
    // same contract the pre-elaboration program did.
    mir.invariant_errors
        .extend(crate::mir::verify::verify(&mir));
    if !mir.invariant_errors.is_empty() {
        return Err(RuntimeError::Unsupported(format!(
            "invalid checked program: {}",
            mir.invariant_errors.join("; ")
        )));
    }
    let structs = build_structs(&mir.declarations);
    let sigs = build_sigs(&mir.declarations);
    Ok(Prog {
        // Elaborate ASAP drops: splice a `DropVar` after each variable's last
        // use, so a struct's `__del__` runs there (Stage 7). A no-op for values
        // without a destructor.
        mir,
        structs,
        sigs,
    })
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
                    explicit_destroy: declaration.explicit_destroy_message.is_some(),
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
                        .map(|default| default.as_ref().map(checked_const_value))
                        .collect(),
                    required: declaration.required.clone(),
                    variadic: declaration.variadic.clone(),
                    variadic_index: declaration.variadic_index,
                    kw_variadic: declaration.kw_variadic.clone(),
                    kw_variadic_index: declaration.kw_variadic_index,
                    positional_only: declaration.positional_only,
                    keyword_only: declaration.keyword_only,
                    param_decls: declaration.param_decls.clone(),
                },
            )
        })
        .collect()
}

mod calls;
use calls::*;
/// Read a struct field (or a reified value parameter, e.g. `Self.n`) by name.
fn get_field(base: &Value, field: &str) -> Result<Value, RuntimeError> {
    match base {
        Value::Slice {
            start, end, step, ..
        } => {
            let value = match field {
                "start" => *start,
                "end" => *end,
                "step" => *step,
                _ => {
                    return Err(RuntimeError::TypeError(format!(
                        "Slice has no field '{field}'"
                    )));
                }
            };
            Ok(Value::Struct {
                name: "Optional".to_string(),
                fields: vec![(
                    "values".to_string(),
                    Value::List(value.map(Value::Int).into_iter().collect()),
                )],
                value_params: Vec::new(),
            })
        }
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

fn apply_format_spec(value: &Value, rendered: &str, spec: &str) -> Result<String, RuntimeError> {
    if spec.is_empty() {
        return Ok(rendered.to_string());
    }
    if let Some(precision) = spec
        .strip_prefix('.')
        .and_then(|tail| tail.strip_suffix('f'))
        .and_then(|digits| digits.parse::<usize>().ok())
        && let Value::Float64(number) = value
    {
        return Ok(format!("{number:.precision$}"));
    }
    let (alignment, width_text) = match spec.chars().next() {
        Some(character @ ('<' | '>' | '^')) => (character, &spec[1..]),
        _ => ('>', spec),
    };
    let width = width_text.parse::<usize>().map_err(|_| {
        RuntimeError::TypeError(format!("unsupported format specification '{spec}'"))
    })?;
    if rendered.len() >= width {
        return Ok(rendered.to_string());
    }
    let padding = width - rendered.len();
    let (left, right) = match alignment {
        '<' => (0, padding),
        '^' => (padding / 2, padding - padding / 2),
        _ => (padding, 0),
    };
    Ok(format!(
        "{}{}{}",
        " ".repeat(left),
        rendered,
        " ".repeat(right)
    ))
}

fn navigate_reference<'a>(
    mut value: &'a Value,
    projection: &[RefProjection],
) -> Result<&'a Value, RuntimeError> {
    for segment in projection {
        value = match (segment, value) {
            (RefProjection::Field(name), Value::Struct { fields, .. }) => fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value)
                .ok_or_else(|| RuntimeError::TypeError(format!("no field '{name}'")))?,
            (RefProjection::Index(index), Value::List(items) | Value::Tuple(items)) => {
                items.get(*index).ok_or_else(|| {
                    RuntimeError::TypeError("reference index out of bounds".to_string())
                })?
            }
            (
                RefProjection::Variant(expected),
                Value::Variant {
                    index,
                    value,
                    alternatives,
                },
            ) => {
                if index != expected {
                    return Err(RuntimeError::TypeError(format!(
                        "Variant holds '{}', not '{}'",
                        alternatives
                            .get(*index)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "<invalid>".to_string()),
                        alternatives
                            .get(*expected)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "<invalid>".to_string())
                    )));
                }
                value.as_ref()
            }
            _ => {
                return Err(RuntimeError::TypeError(
                    "invalid reference projection".to_string(),
                ));
            }
        };
    }
    Ok(value)
}

fn navigate_reference_mut<'a>(
    mut value: &'a mut Value,
    projection: &[RefProjection],
) -> Result<&'a mut Value, RuntimeError> {
    for segment in projection {
        value = match segment {
            RefProjection::Field(name) => match value {
                Value::Struct { fields, .. } => fields
                    .iter_mut()
                    .find(|(field, _)| field == name)
                    .map(|(_, value)| value)
                    .ok_or_else(|| RuntimeError::TypeError(format!("no field '{name}'")))?,
                _ => {
                    return Err(RuntimeError::TypeError(
                        "invalid reference field".to_string(),
                    ));
                }
            },
            RefProjection::Index(index) => match value {
                Value::List(items) => items.get_mut(*index).ok_or_else(|| {
                    RuntimeError::TypeError("reference index out of bounds".to_string())
                })?,
                _ => {
                    return Err(RuntimeError::TypeError(
                        "invalid mutable reference index".to_string(),
                    ));
                }
            },
            RefProjection::Variant(expected) => match value {
                Value::Variant {
                    index,
                    value,
                    alternatives,
                } if index == expected => value.as_mut(),
                Value::Variant {
                    index,
                    alternatives,
                    ..
                } => {
                    return Err(RuntimeError::TypeError(format!(
                        "Variant holds '{}', not '{}'",
                        alternatives
                            .get(*index)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "<invalid>".to_string()),
                        alternatives
                            .get(*expected)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "<invalid>".to_string())
                    )));
                }
                _ => {
                    return Err(RuntimeError::TypeError(
                        "invalid mutable Variant reference projection".to_string(),
                    ));
                }
            },
        };
    }
    Ok(value)
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
    Ok(args.pop().expect("arity checked above"))
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
    Ok((
        it.next().expect("arity checked above"),
        it.next().expect("arity checked above"),
    ))
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

mod places;
use places::*;
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
        Const::Function(name) => Value::Function(name.clone()),
        Const::None => Value::None,
    }
}
