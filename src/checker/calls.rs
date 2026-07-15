//! Checker adaptation of the phase-neutral call-slot matcher.

use super::*;

impl MatchError {
    pub(crate) fn into_type_error(self, func: &str) -> TypeError {
        match self {
            MatchError::TooManyPositional { expected, got } => TypeError::ArityMismatch {
                name: func.to_string(),
                expected,
                got,
            },
            MatchError::UnknownKeyword(k) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("unexpected keyword argument '{}'", k),
            },
            MatchError::PositionalOnly(k) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("argument '{}' is positional-only", k),
            },
            MatchError::Duplicate(k) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("argument '{}' supplied more than once", k),
            },
            MatchError::Missing(m) => TypeError::BadCall {
                func: func.to_string(),
                reason: format!("missing required argument '{}'", m),
            },
        }
    }
}

/// The per-slot **required** mask for regular parameters. Defaults must be
/// trailing within the positional-or-keyword section and within the keyword-only
/// section, but an optional positional parameter may be followed by a required
/// keyword-only parameter.
pub(super) fn required_mask(
    params: &[&crate::ast::FnParam],
    keyword_only: Option<usize>,
) -> Result<Vec<bool>, TypeError> {
    let kw_start = keyword_only.unwrap_or(params.len()).min(params.len());
    let mut required = Vec::with_capacity(params.len());
    validate_required_order(&params[..kw_start])?;
    validate_required_order(&params[kw_start..])?;
    for p in params {
        required.push(p.default.is_none());
    }
    Ok(required)
}

pub(super) fn validate_required_order(params: &[&crate::ast::FnParam]) -> Result<(), TypeError> {
    let mut seen_default = false;
    for p in params {
        if p.default.is_some() {
            seen_default = true;
        } else if seen_default {
            return Err(TypeError::Unsupported(format!(
                "a required parameter ('{}') cannot follow one with a default value",
                p.name
            )));
        }
    }
    Ok(())
}

pub(super) fn lifecycle_method_name(m: &Method) -> &str {
    if is_mojo_copy_constructor(m) {
        "__copyinit__"
    } else {
        &m.name
    }
}

pub(super) fn is_mojo_copy_constructor(m: &Method) -> bool {
    m.name == "__init__"
        && m.has_self
        && matches!(m.self_convention, Some(ArgConvention::Out))
        && m.positional_only.is_none()
        && m.keyword_only == Some(0)
        && m.params.len() == 1
        && is_copy_param(&m.params[0])
        && m.ret.is_none()
}

pub(super) fn is_copy_param(p: &FnParam) -> bool {
    p.name == "copy"
        && p.default.is_none()
        && p.kind == crate::ast::ParamKind::Regular
        && p.convention.is_none()
        && matches!(p.ty, SourceType::SelfType)
}

/// A call using keyword arguments (`name=value`) is parsed but not implemented.
pub(super) fn reject_kwargs(kwargs: &[crate::ast::KwArg]) -> Result<(), TypeError> {
    if kwargs.is_empty() {
        Ok(())
    } else {
        Err(TypeError::Unsupported("keyword arguments".to_string()))
    }
}
