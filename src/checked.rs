//! Checked semantic handoff between the frontend and lowering.

use crate::ast::{Expr, ExprKind, PrefixOp};
use crate::ast::{SourceType, Stmt};
use crate::token::{SourceSpan, Span};
use crate::types::Ty;
use std::collections::HashMap;

/// A successfully checked program plus semantic facts that downstream phases
/// previously recomputed from AST syntax or checker-private side tables.
#[derive(Debug, Clone)]
pub struct CheckedProgram {
    statements: Vec<Stmt>,
    overload_targets: HashMap<SourceSpan, String>,
    checked_types: HashMap<AnnotationSite, Ty>,
}

/// The declaration-owned location of a source annotation. Unlike `SourceType`
/// syntax itself,
/// this identity preserves the scope in which syntax such as a bare `T` was
/// resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum AnnotationSite {
    FunctionParam {
        module: Option<String>,
        declaration: Span,
        param: usize,
    },
    StructField {
        module: Option<String>,
        declaration: Span,
        field: usize,
    },
    MethodParam {
        module: Option<String>,
        declaration: Span,
        method: usize,
        param: usize,
    },
}

#[derive(Debug, Clone)]
pub enum CheckedConst {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    None,
}

impl CheckedConst {
    pub fn from_expr(expr: &Expr) -> Option<Self> {
        match &expr.kind {
            ExprKind::Int(value) => Some(Self::Int(*value)),
            ExprKind::Float(value) => Some(Self::Float(*value)),
            ExprKind::Bool(value) => Some(Self::Bool(*value)),
            ExprKind::Str(value) => Some(Self::String(value.clone())),
            ExprKind::None => Some(Self::None),
            ExprKind::Prefix(PrefixOp::Neg, inner) => match Self::from_expr(inner)? {
                Self::Int(value) => Some(Self::Int(-value)),
                Self::Float(value) => Some(Self::Float(-value)),
                _ => None,
            },
            _ => None,
        }
    }
}

impl CheckedProgram {
    pub(crate) fn new(
        statements: Vec<Stmt>,
        overload_targets: HashMap<SourceSpan, String>,
        checked_types: HashMap<AnnotationSite, Ty>,
    ) -> Self {
        Self {
            statements,
            overload_targets,
            checked_types,
        }
    }

    pub fn statements(&self) -> &[Stmt] {
        &self.statements
    }

    pub fn overload_targets(&self) -> &HashMap<SourceSpan, String> {
        &self.overload_targets
    }

    pub(crate) fn checked_type_at(&self, site: &AnnotationSite) -> Option<&Ty> {
        self.checked_types.get(site)
    }

    pub fn declaration_module<'a>(&self, location: &'a SourceSpan) -> Option<&'a str> {
        location.source.as_deref()
    }

    /// Compatibility snapshot for internally generated CTFE programs that were
    /// already proven safe before specialization but intentionally omit some
    /// source declarations. Production compilation must use `check_program`.
    pub(crate) fn prevalidated_ctfe(statements: &[Stmt]) -> Self {
        let mut checked_types = HashMap::new();
        collect_source_type_approximations(statements, &mut checked_types);
        Self::new(statements.to_vec(), HashMap::new(), checked_types)
    }
}

fn collect_source_type_approximations(statements: &[Stmt], out: &mut HashMap<AnnotationSite, Ty>) {
    for statement in statements {
        match &statement.kind {
            crate::ast::StmtKind::Def { params, body, .. } => {
                for (index, param) in params.iter().enumerate() {
                    out.insert(
                        AnnotationSite::FunctionParam {
                            module: statement.module.clone(),
                            declaration: statement.span,
                            param: index,
                        },
                        approximate_source_type(&param.ty),
                    );
                }
                collect_source_type_approximations(body, out);
            }
            crate::ast::StmtKind::Struct {
                fields, methods, ..
            } => {
                for (index, field) in fields.iter().enumerate() {
                    out.insert(
                        AnnotationSite::StructField {
                            module: statement.module.clone(),
                            declaration: statement.span,
                            field: index,
                        },
                        approximate_source_type(&field.ty),
                    );
                }
                for (method_index, method) in methods.iter().enumerate() {
                    for (param_index, param) in method.params.iter().enumerate() {
                        out.insert(
                            AnnotationSite::MethodParam {
                                module: statement.module.clone(),
                                declaration: statement.span,
                                method: method_index,
                                param: param_index,
                            },
                            approximate_source_type(&param.ty),
                        );
                    }
                    collect_source_type_approximations(&method.body, out);
                }
            }
            _ => {}
        }
    }
}

fn approximate_source_type(ty: &SourceType) -> Ty {
    match ty {
        SourceType::Int => Ty::Int,
        SourceType::UInt => Ty::UInt,
        SourceType::Bool => Ty::Bool,
        SourceType::String => Ty::String,
        SourceType::Float64 => Ty::Float64,
        SourceType::None => Ty::None,
        SourceType::Named(name, args) if name == "List" && args.len() == 1 => {
            let elem = match &args[0] {
                crate::ast::ParamArg::Type(ty) => approximate_source_type(ty),
                _ => Ty::None,
            };
            Ty::List(Box::new(elem))
        }
        SourceType::Named(name, args) => Ty::Struct(
            name.clone(),
            args.iter()
                .map(|arg| match arg {
                    crate::ast::ParamArg::Type(ty) => {
                        crate::types::TyArg::Ty(approximate_source_type(ty))
                    }
                    crate::ast::ParamArg::Value(expr) => {
                        crate::types::TyArg::Val(match &expr.kind {
                            ExprKind::Int(value) => crate::CtValue::Int(*value),
                            _ => crate::CtValue::Int(0),
                        })
                    }
                })
                .collect(),
        ),
        SourceType::SelfType | SourceType::SelfParam(_) | SourceType::Assoc { .. } => Ty::None,
        SourceType::Func { .. } => Ty::None,
        SourceType::Ref { referent, .. } => approximate_source_type(referent),
    }
}
