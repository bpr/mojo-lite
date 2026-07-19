//! Standalone semantic verification for typed MIR.
//!
//! The verifier consumes a lowered [`MirProgram`] plus its checked declaration
//! metadata — never source AST — and reports every violation as a message in
//! the program's `invariant_errors` style. Production compilation and the VM
//! reject programs with any finding; ownership dataflow remains owned by
//! `crate::analysis` and is composed with this verifier by the pipeline.
//!
//! Check classes:
//! - typed-place completeness and projection consistency;
//! - register bounds and register-type completeness;
//! - instruction and call type consistency (via the checker's coercion
//!   predicate; calls are compared against `MirFunctionDeclaration` facts when
//!   the callee is declared — builtin callees have no declaration and only
//!   participate in register checks);
//! - CFG edges: jump-target bounds per region, `FallOff`/`EscapeJump` only
//!   inside `try` sub-regions;
//! - effects: a raising site (a `Raise`, or a call carrying a checked error
//!   type) inside a nonraising function must be protected by a handler;
//! - reference invariants: `StoreRef` initializes reference storage, and a
//!   declared write-back parameter receives a caller place.

use super::{
    FuncRef, MirBlock, MirDeclarations, MirFunction, MirFunctionDeclaration, MirInstr, MirPlace,
    MirProgram, MirTerm, Proj, Reg,
};
use crate::types::Ty;

pub fn verify(program: &MirProgram) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, function) in &program.functions {
        verify_function(name, function, &program.declarations, &mut errors);
    }
    errors
}

/// The result registers an instruction defines (call/operation destinations
/// and loan/consumption markers).
pub(crate) fn instruction_result_regs(instruction: &MirInstr, out: &mut Vec<Reg>) {
    match instruction {
        MirInstr::MakeRef { dest, .. }
        | MirInstr::ReadRef { dest, .. }
        | MirInstr::Const { dest, .. }
        | MirInstr::UseVar { dest, .. }
        | MirInstr::MovePlace { dest, .. }
        | MirInstr::UnOp { dest, .. }
        | MirInstr::BinOp { dest, .. }
        | MirInstr::Call { dest, .. }
        | MirInstr::CallIndirect { dest, .. }
        | MirInstr::MethodCall { dest, .. }
        | MirInstr::GetField { dest, .. }
        | MirInstr::Index { dest, .. }
        | MirInstr::Slice { dest, .. }
        | MirInstr::MultiIndex { dest, .. }
        | MirInstr::LoadPlace { dest, .. }
        | MirInstr::MakeList { dest, .. }
        | MirInstr::MakeSet { dest, .. }
        | MirInstr::MakeDict { dest, .. }
        | MirInstr::MakeTuple { dest, .. }
        | MirInstr::MakeVariant { dest, .. }
        | MirInstr::MakeSimd { dest, .. }
        | MirInstr::MakeClosure { dest, .. }
        | MirInstr::VariantIs { dest, .. }
        | MirInstr::VariantGet { dest, .. }
        | MirInstr::VariantSet { dest, .. }
        | MirInstr::VariantTake { dest, .. }
        | MirInstr::VariantReplace { dest, .. }
        | MirInstr::HasNext { dest, .. }
        | MirInstr::Next { dest, .. } => out.push(*dest),
        MirInstr::BeginLoan { marker, .. } | MirInstr::ConsumePlace { marker, .. } => {
            out.push(*marker)
        }
        _ => {}
    }
}

/// The registers an instruction reads (operands, arguments, stored values, and
/// place index registers). `Try` sub-regions are walked separately.
fn instruction_operand_regs(instruction: &MirInstr, out: &mut Vec<Reg>) {
    let place = |p: &MirPlace, out: &mut Vec<Reg>| {
        for projection in &p.proj {
            if let Proj::Index(register) = projection {
                out.push(*register);
            }
        }
    };
    match instruction {
        MirInstr::BeginLoan { place: p, .. }
        | MirInstr::ConsumePlace { place: p, .. }
        | MirInstr::MakeRef { place: p, .. }
        | MirInstr::MovePlace { place: p, .. }
        | MirInstr::LoadPlace { place: p, .. } => place(p, out),
        MirInstr::ReadRef { reference, .. } => out.push(*reference),
        MirInstr::WriteRef { reference, value } => out.extend([*reference, *value]),
        MirInstr::UnOp { a, .. } => out.push(*a),
        MirInstr::BinOp { a, b, .. } => out.extend([*a, *b]),
        MirInstr::Store { place: p, src } => {
            place(p, out);
            out.push(*src);
        }
        MirInstr::StoreRef {
            place: p,
            reference,
        } => {
            place(p, out);
            out.push(*reference);
        }
        MirInstr::MultiSet {
            receiver_place,
            args,
            value,
            ..
        } => {
            place(receiver_place, out);
            for argument in args {
                subscript_arg_regs(argument, out);
            }
            out.push(*value);
        }
        MirInstr::Call {
            args,
            kwargs,
            arg_places,
            param_arg_regs,
            ..
        } => {
            out.extend(args.iter().copied());
            out.extend(kwargs.iter().map(|(_, register)| *register));
            for p in arg_places.iter().flatten() {
                place(p, out);
            }
            out.extend(param_arg_regs.iter().flatten().copied());
        }
        MirInstr::CallIndirect {
            callee,
            args,
            kwargs,
            ..
        } => {
            out.push(*callee);
            out.extend(args.iter().copied());
            out.extend(kwargs.iter().map(|(_, register)| *register));
        }
        MirInstr::MethodCall {
            recv,
            args,
            kwargs,
            recv_place,
            arg_places,
            ..
        } => {
            out.push(*recv);
            out.extend(args.iter().copied());
            out.extend(kwargs.iter().map(|(_, register)| *register));
            for p in recv_place.iter().chain(arg_places.iter().flatten()) {
                place(p, out);
            }
        }
        MirInstr::GetField { base, .. } => out.push(*base),
        MirInstr::Index { base, index, .. } => out.extend([*base, *index]),
        MirInstr::Slice {
            object,
            lower,
            upper,
            step,
            ..
        } => {
            out.push(*object);
            out.extend([lower, upper, step].into_iter().flatten().copied());
        }
        MirInstr::MultiIndex { object, args, .. } => {
            out.push(*object);
            for argument in args {
                subscript_arg_regs(argument, out);
            }
        }
        MirInstr::MakeList { elems, .. }
        | MirInstr::MakeSet { elems, .. }
        | MirInstr::MakeTuple { elems, .. }
        | MirInstr::MakeSimd { elems, .. } => out.extend(elems.iter().copied()),
        MirInstr::MakeDict { entries, .. } => {
            for (key, value) in entries {
                out.extend([*key, *value]);
            }
        }
        MirInstr::MakeVariant { value, .. } => out.push(*value),
        MirInstr::MakeClosure { captures, .. } => {
            for capture in captures {
                place(&capture.place, out);
            }
        }
        MirInstr::CollectionInsert { key, value, .. } => {
            out.extend(key.iter().copied());
            out.push(*value);
        }
        MirInstr::VariantIs { variant, .. } | MirInstr::VariantGet { variant, .. } => {
            out.push(*variant)
        }
        MirInstr::VariantTake { variant, .. } => out.push(*variant),
        MirInstr::VariantSet {
            place: p, value, ..
        } => {
            place(p, out);
            out.push(*value);
        }
        MirInstr::VariantReplace {
            place: p, value, ..
        } => {
            place(p, out);
            out.push(*value);
        }
        MirInstr::Raise { src } => out.push(*src),
        MirInstr::Drop { reg } => out.push(*reg),
        MirInstr::DefVar { src, .. } => out.push(*src),
        MirInstr::Const { .. }
        | MirInstr::UseVar { .. }
        | MirInstr::KeepAlive { .. }
        | MirInstr::DropVar { .. }
        | MirInstr::ConsumeVar { .. }
        | MirInstr::GetIter { .. }
        | MirInstr::HasNext { .. }
        | MirInstr::Next { .. }
        | MirInstr::Unsupported(_)
        | MirInstr::Try { .. } => {}
    }
}

fn subscript_arg_regs(argument: &super::MirSubscriptArg, out: &mut Vec<Reg>) {
    match argument {
        super::MirSubscriptArg::Index(register) => out.push(*register),
        super::MirSubscriptArg::Slice {
            lower, upper, step, ..
        } => out.extend([lower, upper, step].into_iter().flatten().copied()),
    }
}

/// Where a run of blocks sits: the function's top level, or one `try`
/// sub-region (with its handler-protection status).
struct RegionContext {
    /// Number of blocks in this region — the bound for region-local jumps.
    region_len: usize,
    /// Number of blocks in the enclosing function — the bound for
    /// `EscapeJump` targets.
    function_len: usize,
    /// Whether this run of blocks is a `try` sub-region (where `FallOff` and
    /// `EscapeJump` are legal terminators).
    in_try_region: bool,
    /// Whether a raise from this position reaches an `except` handler before
    /// leaving the function.
    protected: bool,
}

fn verify_function(
    name: &str,
    function: &MirFunction,
    declarations: &MirDeclarations,
    errors: &mut Vec<String>,
) {
    if function.var_names.len() != function.n_vars {
        errors.push(format!(
            "MIR function '{name}' has {} variable names for {} slots",
            function.var_names.len(),
            function.n_vars
        ));
    }
    let context = RegionContext {
        region_len: function.blocks.len(),
        function_len: function.blocks.len(),
        in_try_region: false,
        protected: false,
    };
    verify_blocks(
        name,
        function,
        declarations,
        &function.blocks,
        &context,
        errors,
    );
}

fn verify_blocks(
    name: &str,
    function: &MirFunction,
    declarations: &MirDeclarations,
    blocks: &[MirBlock],
    context: &RegionContext,
    errors: &mut Vec<String>,
) {
    for (block_index, block) in blocks.iter().enumerate() {
        for instruction in &block.instrs {
            verify_instruction(
                name,
                function,
                declarations,
                block_index,
                instruction,
                context,
                errors,
            );
        }
        verify_terminator(name, function, block_index, &block.term, context, errors);
    }
}

fn verify_instruction(
    name: &str,
    function: &MirFunction,
    declarations: &MirDeclarations,
    block_index: usize,
    instruction: &MirInstr,
    context: &RegionContext,
    errors: &mut Vec<String>,
) {
    let prefix = format!("MIR function '{name}' block {block_index}");
    for place in instruction_places(instruction) {
        verify_place(name, block_index, function, declarations, place, errors);
    }
    // Register bounds and type completeness.
    let mut regs = Vec::new();
    instruction_result_regs(instruction, &mut regs);
    instruction_operand_regs(instruction, &mut regs);
    for register in &regs {
        if register.0 >= function.n_regs {
            errors.push(format!("{prefix}: invalid register r{}", register.0));
        } else if !function.reg_types.contains_key(&register.0) {
            errors.push(format!("{prefix}: untyped register r{}", register.0));
        }
    }
    let reg_ty = |register: &Reg| function.reg_types.get(&register.0);
    match instruction {
        MirInstr::Store { place, src } => {
            if let (Some(expected), Some(found)) = (place.ty.as_ref(), reg_ty(src)) {
                let target = match expected {
                    Ty::Ref(reference) => reference.referent.as_ref(),
                    other => other,
                };
                if !types_compatible(found, target) {
                    errors.push(format!(
                        "{prefix}: store of {found} into storage of type {target}"
                    ));
                }
            }
        }
        MirInstr::StoreRef { place, .. } => {
            if let Some(storage) = place.ty.as_ref()
                && !matches!(storage, Ty::Ref(_))
            {
                errors.push(format!(
                    "{prefix}: StoreRef into non-reference storage of type {storage}"
                ));
            }
        }
        MirInstr::DefVar {
            src,
            binding_ty: Some(expected),
            ..
        } => {
            if let Some(found) = reg_ty(src)
                && !types_compatible(found, expected)
            {
                errors.push(format!(
                    "{prefix}: binding of {found} to a slot of type {expected}"
                ));
            }
        }
        MirInstr::MakeVariant {
            alternatives,
            index,
            value,
            ..
        } => {
            if *index >= alternatives.len() {
                errors.push(format!(
                    "{prefix}: variant construction index {index} out of {} alternatives",
                    alternatives.len()
                ));
            } else if let Some(found) = reg_ty(value)
                && !types_compatible(found, &alternatives[*index])
            {
                errors.push(format!(
                    "{prefix}: variant payload {found} does not fit alternative {}",
                    alternatives[*index]
                ));
            }
        }
        MirInstr::Call {
            func: FuncRef(callee),
            args,
            kwargs,
            arg_places,
            ..
        } => {
            if let Some(declaration) = declared(declarations, callee) {
                verify_direct_call(
                    &prefix,
                    function,
                    declaration,
                    args,
                    kwargs,
                    arg_places,
                    errors,
                );
            }
        }
        MirInstr::MethodCall {
            resolved: Some(callee),
            args,
            kwargs,
            arg_places,
            ..
        } => {
            if let Some(declaration) = declared(declarations, callee) {
                verify_direct_call(
                    &prefix,
                    function,
                    declaration,
                    args,
                    kwargs,
                    arg_places,
                    errors,
                );
            }
        }
        MirInstr::Raise { .. } => {
            if !function.raises && !context.protected {
                errors.push(format!(
                    "{prefix}: unprotected raise in nonraising function"
                ));
            }
        }
        MirInstr::Try {
            body,
            handler,
            orelse,
            finalbody,
            ..
        } => {
            let body_context = RegionContext {
                region_len: body.len(),
                function_len: context.function_len,
                in_try_region: true,
                protected: handler.is_some() || context.protected,
            };
            verify_blocks(name, function, declarations, body, &body_context, errors);
            for region in handler
                .iter()
                .map(|(_, blocks)| blocks)
                .chain(orelse.iter())
                .chain(finalbody.iter())
            {
                let region_context = RegionContext {
                    region_len: region.len(),
                    function_len: context.function_len,
                    in_try_region: true,
                    protected: context.protected,
                };
                verify_blocks(
                    name,
                    function,
                    declarations,
                    region,
                    &region_context,
                    errors,
                );
            }
        }
        _ => {}
    }
    // A call carrying a checked error contract raises unless handled.
    if let MirInstr::Call {
        raises: Some(_), ..
    }
    | MirInstr::CallIndirect {
        raises: Some(_), ..
    }
    | MirInstr::MethodCall {
        raises: Some(_), ..
    } = instruction
        && !function.raises
        && !context.protected
    {
        errors.push(format!(
            "{prefix}: unprotected raising call in nonraising function"
        ));
    }
}

/// Arity, argument-type, and write-back checks against a declaration. Only the
/// plain positional shape is compared — defaulted, keyword, and variadic calls
/// are bound by the runtime matcher, whose slotting the verifier does not
/// replicate.
fn verify_direct_call(
    prefix: &str,
    function: &MirFunction,
    declaration: &MirFunctionDeclaration,
    args: &[Reg],
    kwargs: &[(String, Reg)],
    arg_places: &[Option<MirPlace>],
    errors: &mut Vec<String>,
) {
    let plain = kwargs.is_empty()
        && declaration.variadic.is_none()
        && declaration.kw_variadic.is_none()
        && args.len() == declaration.param_types.len();
    if !plain {
        return;
    }
    for (index, (argument, expected)) in args.iter().zip(&declaration.param_types).enumerate() {
        if let Some(found) = function.reg_types.get(&argument.0)
            && !types_compatible(found, expected)
        {
            errors.push(format!(
                "{prefix}: argument {index} of '{}' has type {found}, declared {expected}",
                declaration.lowered_name
            ));
        }
        if declaration.ref_params.get(index).copied().unwrap_or(false)
            && arg_places
                .get(index)
                .map(Option::as_ref)
                .unwrap_or(None)
                .is_none()
        {
            errors.push(format!(
                "{prefix}: write-back parameter {index} of '{}' has no caller place",
                declaration.lowered_name
            ));
        }
    }
}

fn verify_terminator(
    name: &str,
    function: &MirFunction,
    block_index: usize,
    terminator: &MirTerm,
    context: &RegionContext,
    errors: &mut Vec<String>,
) {
    let prefix = format!("MIR function '{name}' block {block_index}");
    match terminator {
        MirTerm::Jump(target) => {
            if *target >= context.region_len {
                errors.push(format!("{prefix}: jump to invalid block {target}"));
            }
        }
        MirTerm::Branch {
            cond,
            then_b,
            else_b,
        } => {
            for target in [then_b, else_b] {
                if *target >= context.region_len {
                    errors.push(format!("{prefix}: branch to invalid block {target}"));
                }
            }
            if let Some(found) = function.reg_types.get(&cond.0)
                && *found != Ty::Bool
            {
                errors.push(format!("{prefix}: branch condition has type {found}"));
            }
        }
        MirTerm::Return(value) => {
            // `Return(None)` doubles as the lowering placeholder terminator, so
            // only value-carrying returns are checked.
            if let Some(register) = value
                && !function.returns_reference
                && let (Some(found), Some(expected)) = (
                    function.reg_types.get(&register.0),
                    function.ret_ty.as_ref(),
                )
                && !types_compatible(found, expected)
            {
                errors.push(format!(
                    "{prefix}: return of {found} from a function returning {expected}"
                ));
            }
        }
        MirTerm::FallOff => {
            if !context.in_try_region {
                errors.push(format!("{prefix}: FallOff terminator outside a try region"));
            }
        }
        MirTerm::EscapeJump { target, .. } => {
            if !context.in_try_region {
                errors.push(format!(
                    "{prefix}: EscapeJump terminator outside a try region"
                ));
            }
            if *target >= context.function_len {
                errors.push(format!("{prefix}: escape to invalid block {target}"));
            }
        }
    }
}

fn declared<'a>(
    declarations: &'a MirDeclarations,
    callee: &str,
) -> Option<&'a MirFunctionDeclaration> {
    declarations
        .functions
        .iter()
        .find(|declaration| declaration.lowered_name == callee)
}

/// Compatibility for verification purposes: either direction of the checker's
/// coercion predicate. Lowering emits checker-approved conversions before
/// values flow, so remaining differences are representational (literal
/// materialization, generic instantiation), not errors to re-litigate. A type
/// mentioning an unsubstituted parameter is not compared — instantiation is
/// the checker's domain and the verifier never re-derives it.
fn types_compatible(found: &Ty, expected: &Ty) -> bool {
    if contains_type_param(found) || contains_type_param(expected) {
        return true;
    }
    // A bare `Struct(name, [])` is the established erased spelling for a
    // receiver or synthesized construction of any instantiation of `name`.
    if let (Ty::Struct(found_name, found_args), Ty::Struct(expected_name, expected_args)) =
        (found, expected)
        && found_name == expected_name
        && (found_args.is_empty() || expected_args.is_empty())
    {
        return true;
    }
    // A contextual selection narrows an overload set to one member.
    if let Ty::Overload(members) = found {
        return members
            .iter()
            .any(|member| types_compatible(member, expected));
    }
    // A struct may nominally conform to a `def(...)` callable trait; the
    // conformance is checker-verified and not yet recorded in MIR
    // declarations, so the verifier does not re-check it here.
    if matches!(found, Ty::Struct(..)) && matches!(expected, Ty::Func { .. }) {
        return true;
    }
    crate::checker::value_coerces(found, expected) || crate::checker::value_coerces(expected, found)
}

fn contains_type_param(ty: &Ty) -> bool {
    match ty {
        Ty::Param { .. } | Ty::Assoc { .. } => true,
        Ty::List(inner) | Ty::Set(inner) | Ty::Pointer { element: inner, .. } => {
            contains_type_param(inner)
        }
        Ty::Dict(key, value) => contains_type_param(key) || contains_type_param(value),
        Ty::Tuple(elements) | Ty::Variant(elements) => elements.iter().any(contains_type_param),
        Ty::Ref(reference) => contains_type_param(&reference.referent),
        Ty::Struct(_, arguments) => arguments.iter().any(|argument| match argument {
            crate::types::TyArg::Ty(inner) => contains_type_param(inner),
            crate::types::TyArg::Val(_) => false,
        }),
        Ty::Func { params, ret, .. } | Ty::GenericFunc { params, ret, .. } => {
            params.iter().any(contains_type_param) || contains_type_param(ret)
        }
        _ => false,
    }
}

fn instruction_places(instruction: &MirInstr) -> Vec<&MirPlace> {
    match instruction {
        MirInstr::BeginLoan { place, .. }
        | MirInstr::MakeRef { place, .. }
        | MirInstr::MovePlace { place, .. }
        | MirInstr::Store { place, .. }
        | MirInstr::StoreRef { place, .. }
        | MirInstr::MultiSet {
            receiver_place: place,
            ..
        }
        | MirInstr::LoadPlace { place, .. }
        | MirInstr::VariantSet { place, .. }
        | MirInstr::VariantReplace { place, .. }
        | MirInstr::ConsumePlace { place, .. } => vec![place],
        MirInstr::MakeClosure { captures, .. } => {
            captures.iter().map(|capture| &capture.place).collect()
        }
        MirInstr::Call { arg_places, .. } => arg_places.iter().flatten().collect(),
        MirInstr::MethodCall {
            recv_place,
            arg_places,
            ..
        } => recv_place
            .iter()
            .chain(arg_places.iter().flatten())
            .collect(),
        _ => Vec::new(),
    }
}

fn verify_place(
    function_name: &str,
    block: usize,
    function: &MirFunction,
    declarations: &MirDeclarations,
    place: &MirPlace,
    errors: &mut Vec<String>,
) {
    let prefix = format!("MIR function '{function_name}' block {block}");
    if place.root as usize >= function.n_vars {
        errors.push(format!(
            "{prefix} place has invalid root slot {}",
            place.root
        ));
    }
    if !place.is_typed() {
        errors.push(format!(
            "{prefix} place rooted at slot {} lacks complete checked type metadata",
            place.root
        ));
        return;
    }
    let mut current = place.root_ty.clone();
    for (projection, projected) in place.proj.iter().zip(&place.projection_tys) {
        match projection {
            Proj::Index(register) => {
                if register.0 >= function.n_regs {
                    errors.push(format!(
                        "{prefix} place index uses invalid register r{}",
                        register.0
                    ));
                }
            }
            Proj::Field(field) => {
                // A concrete non-generic struct's field projection must agree
                // with its declared layout; generic layouts would need
                // substitution the verifier deliberately does not re-derive.
                if let Some(Ty::Struct(struct_name, arguments)) = &current
                    && arguments.is_empty()
                    && let Some(declaration) = declarations
                        .structs
                        .iter()
                        .find(|declaration| &declaration.name == struct_name)
                {
                    match declaration
                        .fields
                        .iter()
                        .find(|(candidate, _)| candidate == field)
                    {
                        // A value parameter reads through field syntax; its
                        // declaration lives in `param_decls`, not the layout.
                        None if declaration
                            .param_decls
                            .iter()
                            .any(|(candidate, _)| candidate == field) => {}
                        None => errors.push(format!(
                            "{prefix} place projects unknown field '{field}' of '{struct_name}'"
                        )),
                        Some((_, declared)) if !types_compatible(projected, declared) => errors
                            .push(format!(
                                "{prefix} place field '{field}' of '{struct_name}' typed \
                                 {projected}, declared {declared}"
                            )),
                        Some(_) => {}
                    }
                }
            }
            Proj::Variant(index) => {
                if let Some(Ty::Variant(alternatives)) = &current
                    && *index >= alternatives.len()
                {
                    errors.push(format!(
                        "{prefix} place projects variant alternative {index} out of {}",
                        alternatives.len()
                    ));
                }
            }
        }
        current = Some(projected.clone());
    }
}
