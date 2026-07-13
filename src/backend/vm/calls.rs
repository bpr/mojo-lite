use super::*;

fn runtime_match_error(error: crate::call::MatchError, function: &str) -> RuntimeError {
    use crate::call::MatchError;
    match error {
        MatchError::TooManyPositional { expected, got } => RuntimeError::ArityMismatch {
            name: function.to_string(),
            expected,
            got,
        },
        MatchError::UnknownKeyword(keyword) => RuntimeError::TypeError(format!(
            "'{function}' got an unexpected keyword argument '{keyword}'"
        )),
        MatchError::PositionalOnly(keyword) => RuntimeError::TypeError(format!(
            "'{function}' got positional-only argument '{keyword}' passed as keyword"
        )),
        MatchError::Duplicate(keyword) => RuntimeError::TypeError(format!(
            "'{function}' got argument '{keyword}' more than once"
        )),
        MatchError::Missing(parameter) => RuntimeError::TypeError(format!(
            "'{function}' missing required argument '{parameter}'"
        )),
    }
}

/// Const-fold a default-argument expression to a value. Handles the literal forms
/// (and a unary minus over one) that defaults use in practice; a non-constant
/// default folds to `None` and errors only if that slot is actually taken.
pub(super) fn checked_const_value(value: &CheckedConst) -> Value {
    match value {
        CheckedConst::Int(value) => Value::Int(*value),
        CheckedConst::Float(value) => Value::Float64(*value),
        CheckedConst::Bool(value) => Value::Bool(*value),
        CheckedConst::String(value) => Value::Str(value.clone()),
        CheckedConst::None => Value::None,
    }
}

/// Match positional + keyword arguments to a function's parameter slots, producing
/// the ordered argument values the frame binds — filling defaults and collecting a
/// trailing `*args` into a `List` according to the shared call contract.
pub(super) fn bind_args(
    name: &str,
    sig: &FnSig,
    argv: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<(Vec<Value>, Vec<ArgSlot>), RuntimeError> {
    let kw_names: Vec<&str> = kwargs.iter().map(|(n, _)| n.as_str()).collect();
    let matched = match_call_slots(
        &sig.param_names,
        &sig.required,
        sig.positional_only,
        sig.keyword_only,
        argv.len(),
        &kw_names,
        CallVariadics {
            positional: sig.variadic.is_some(),
            keyword: sig.kw_variadic.is_some(),
        },
    )
    .map_err(|error| runtime_match_error(error, name))?;
    let (slots, overflow) = (matched.slots, matched.positional_overflow);

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
        regular_values.push(crate::runtime::coerce_checked(value, &sig.param_types[i]));
    }
    // Collect overflow positional args into the `*args` list.
    let (out, frame_slots) = if let Some(elem_ty) = &sig.variadic {
        let items = overflow
            .iter()
            .map(|&idx| crate::runtime::coerce_checked(argv[idx].clone(), elem_ty))
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
    let (mut out, mut frame_slots) = (out, frame_slots);
    if let Some(index) = sig.kw_variadic_index {
        out.insert(index, Value::None);
        frame_slots.insert(index, ArgSlot::Default);
    }
    Ok((out, frame_slots))
}

/// Build a struct instance (fieldwise), coercing each argument to its field type.
pub(super) fn construct(
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
        .map(|((fname, fty), arg)| (fname.clone(), crate::runtime::coerce_checked(arg, fty)))
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
