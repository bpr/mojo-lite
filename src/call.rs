//! Phase-neutral function-call argument binding.
//!
//! Checking, MIR declaration lowering, and runtime frame construction share
//! this structural contract. It deliberately knows nothing about types or
//! values; each phase interprets the matched slots and maps errors itself.

use crate::ast::{FnParam, ParamKind};

#[derive(Clone, Copy)]
/// Source argument selected for one regular parameter slot.
pub(crate) enum ArgSlot {
    Positional(usize),
    Keyword(usize),
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Structural reason an argument list cannot bind to a declaration.
pub(crate) enum MatchError {
    TooManyPositional { expected: usize, got: usize },
    UnknownKeyword(String),
    PositionalOnly(String),
    Duplicate(String),
    Missing(String),
}

#[derive(Clone, Copy)]
/// Whether positional and keyword overflow collectors are present.
pub(crate) struct CallVariadics {
    pub positional: bool,
    pub keyword: bool,
}

/// Fully matched regular slots plus variadic overflow source indexes.
pub(crate) struct CallSlots {
    pub slots: Vec<ArgSlot>,
    pub positional_overflow: Vec<usize>,
    pub keyword_overflow: Vec<usize>,
}

/// Match positional and keyword arguments to regular parameter slots.
pub(crate) fn match_call_slots(
    param_names: &[String],
    required: &[bool],
    positional_only: Option<usize>,
    keyword_only: Option<usize>,
    npos: usize,
    kw_names: &[&str],
    variadics: CallVariadics,
) -> Result<CallSlots, MatchError> {
    let nparams = param_names.len();
    debug_assert_eq!(required.len(), nparams);
    let positional_limit = keyword_only.unwrap_or(nparams).min(nparams);
    if npos > positional_limit && !variadics.positional {
        return Err(MatchError::TooManyPositional {
            expected: positional_limit,
            got: npos + kw_names.len(),
        });
    }
    let n_regular_pos = npos.min(positional_limit);
    let positional_overflow = if variadics.positional {
        (positional_limit..npos).collect()
    } else {
        Vec::new()
    };
    let mut slots = vec![None; nparams];
    let mut keyword_overflow = Vec::new();
    for (index, slot) in slots.iter_mut().enumerate().take(n_regular_pos) {
        *slot = Some(ArgSlot::Positional(index));
    }
    for (keyword_index, keyword) in kw_names.iter().enumerate() {
        if kw_names[..keyword_index].contains(keyword) {
            return Err(MatchError::Duplicate((*keyword).to_string()));
        }
        let Some(parameter_index) = param_names.iter().position(|name| name == keyword) else {
            if variadics.keyword {
                keyword_overflow.push(keyword_index);
                continue;
            }
            return Err(MatchError::UnknownKeyword((*keyword).to_string()));
        };
        if positional_only.is_some_and(|limit| parameter_index < limit) {
            return Err(MatchError::PositionalOnly((*keyword).to_string()));
        }
        if slots[parameter_index].is_some() {
            return Err(MatchError::Duplicate((*keyword).to_string()));
        }
        slots[parameter_index] = Some(ArgSlot::Keyword(keyword_index));
    }
    let mut matched = Vec::with_capacity(nparams);
    for (index, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(slot) => matched.push(slot),
            None if required[index] => {
                return Err(MatchError::Missing(param_names[index].clone()));
            }
            None => matched.push(ArgSlot::Default),
        }
    }
    Ok(CallSlots {
        slots: matched,
        positional_overflow,
        keyword_overflow,
    })
}

/// Convert a parser marker to the regular-parameter index space used by calls.
pub(crate) fn regular_marker_index(params: &[FnParam], marker: Option<usize>) -> Option<usize> {
    marker.map(|index| {
        params[..index]
            .iter()
            .filter(|parameter| parameter.kind == ParamKind::Regular)
            .count()
    })
}

/// Effective keyword-only boundary after combining `*` and `*args` markers.
pub(crate) fn effective_keyword_only_index(
    params: &[FnParam],
    keyword_only: Option<usize>,
    variadic_index: Option<usize>,
) -> Option<usize> {
    [
        regular_marker_index(params, keyword_only),
        regular_marker_index(params, variadic_index),
    ]
    .into_iter()
    .flatten()
    .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["a", "b", "c"].map(str::to_string).to_vec()
    }

    #[test]
    fn matches_mixed_arguments_defaults_and_both_overflows() {
        let matched = match_call_slots(
            &names(),
            &[true, false, true],
            None,
            Some(2),
            3,
            &["c", "extra"],
            CallVariadics {
                positional: true,
                keyword: true,
            },
        )
        .unwrap();

        assert!(matches!(matched.slots[0], ArgSlot::Positional(0)));
        assert!(matches!(matched.slots[1], ArgSlot::Positional(1)));
        assert!(matches!(matched.slots[2], ArgSlot::Keyword(0)));
        assert_eq!(matched.positional_overflow, [2]);
        assert_eq!(matched.keyword_overflow, [1]);
    }

    #[test]
    fn rejects_keyword_binding_to_a_positional_only_parameter() {
        let error = match_call_slots(
            &names(),
            &[true, false, false],
            Some(1),
            None,
            0,
            &["a"],
            CallVariadics {
                positional: false,
                keyword: false,
            },
        )
        .err()
        .unwrap();
        assert_eq!(error, MatchError::PositionalOnly("a".to_string()));
    }

    #[test]
    fn rejects_duplicate_keyword_collector_entries() {
        let error = match_call_slots(
            &[],
            &[],
            None,
            None,
            0,
            &["extra", "extra"],
            CallVariadics {
                positional: false,
                keyword: true,
            },
        )
        .err()
        .expect("collector keys must remain unique");
        assert_eq!(error, MatchError::Duplicate("extra".to_string()));
    }
}
