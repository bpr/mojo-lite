//! Checked semantic handoff between the frontend and lowering.

use crate::ast::Stmt;
use crate::ast::{Expr, ExprKind, PrefixOp};
use crate::token::{SourceSpan, Span};
use crate::types::Ty;
use std::collections::HashMap;
use std::collections::HashSet;

/// Stable identity within one checked program. Unlike a source span this remains
/// unique for synthesized nodes and for multiple semantic nodes at one location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckedNodeId(pub u32);

/// How an expression participates in evaluation. New categories can be added
/// without changing source syntax; in particular, future patterns and suspended
/// computations can refer to nodes rather than impersonating ordinary values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueCategory {
    Value,
    Place,
    Type,
    CompileTime,
}

/// Extensible control/effect summary. Suspension is deliberately independent
/// from raising: future coroutines, generators, and delimited continuations can
/// introduce resume edges without being encoded as exceptions or calls.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectFacts {
    pub raises: Option<Ty>,
    pub may_suspend: bool,
    pub diverges: bool,
}

/// Ownership mode selected for a checked iteration expression.  This is a
/// semantic distinction, not a runtime guess: owned iteration must dispatch to
/// an `__iter__(var self)` implementation, while ordinary iteration uses a
/// borrowed receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IterationMode {
    Borrowed,
    Owned,
}

/// Fully resolved iterator protocol retained across the checked boundary.
/// `prepare` contains the exact `__iter__` symbols needed to normalize a user
/// iterable; builtin ranges/collections leave it empty.  User iterators carry
/// exact `__len__`/`__next__` symbols so the VM never performs name/arity
/// overload reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IterationProtocol {
    pub mode: IterationMode,
    pub prepare: Vec<String>,
    pub has_next: Option<String>,
    pub next: Option<String>,
}

/// Checker decisions which lowering must apply explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SemanticAdjustment {
    ResolveCallable(String),
    ImplicitConversion(String),
    BorrowShared,
    BorrowMutable,
    Move,
    ExplicitDestroy,
    Iterate(IterationProtocol),
    ConstructSimd {
        dtype: crate::ast::Dtype,
        width: i64,
    },
    /// Construct the selected alternative of a checked `Variant` type.
    ConstructVariant {
        alternatives: Vec<Ty>,
        index: usize,
    },
    /// Test the active runtime tag (`value.isa[T]()`).
    VariantIs {
        alternatives: Vec<Ty>,
        index: usize,
    },
    /// Compile-time membership query (`value.is_type_supported[T]()`).
    VariantTypeSupported {
        supported: bool,
    },
    /// Checked typed projection (`value[T]`).
    VariantProject {
        alternatives: Vec<Ty>,
        index: usize,
    },
    /// Replace the active alternative (`value.set[T](new_value)`).
    VariantSet {
        alternatives: Vec<Ty>,
        index: usize,
    },
    /// Consume a variant and move out the selected payload. `checked` controls
    /// whether execution validates the active tag (`take` versus `unsafe_take`).
    VariantTake {
        alternatives: Vec<Ty>,
        index: usize,
        checked: bool,
    },
    /// Atomically install `input_index` and move out `output_index`.
    VariantReplace {
        alternatives: Vec<Ty>,
        input_index: usize,
        output_index: usize,
        checked: bool,
    },
    /// Descriptor types selected for a subscript's arguments. `None` denotes an
    /// ordinary index; `Some` denotes a source slice and records whether overload
    /// selection chose the contiguous, strided, or general Slice protocol.
    SliceDescriptors {
        descriptors: Vec<Option<crate::types::SliceKind>>,
        /// A variadic `__setitem__` receives the assignment value through its
        /// required keyword-only `value` slot. Reads and fixed-arity writes leave
        /// this false.
        set_value_keyword: bool,
    },
}

/// One expression in the typed semantic arena. `syntax` is retained for
/// diagnostics and incremental migration only; identity, type, category, edges,
/// and adjustments no longer have to be reconstructed from it.
#[derive(Debug, Clone)]
pub struct CheckedExpr {
    pub id: CheckedNodeId,
    pub syntax: Expr,
    pub ty: Option<Ty>,
    /// Type physically stored at this place. For a reference-valued field this
    /// is `Ty::Ref`, while `ty` is the referent produced by an ordinary read.
    pub place_ty: Option<Ty>,
    /// Resolved destination type when this expression initializes an annotated
    /// or inferred binding. Lowering must not re-resolve its source annotation.
    pub binding_ty: Option<Ty>,
    pub category: ValueCategory,
    pub binding: Option<crate::origin::OwnerId>,
    pub effects: EffectFacts,
    pub adjustments: Vec<SemanticAdjustment>,
    pub children: Vec<CheckedNodeId>,
    /// Stable identities and storage types for generator binders introduced by
    /// a collection comprehension. Entries are in source `for`-clause order.
    /// Keeping these on the checked node prevents MIR from accidentally
    /// interning a binder by its spelling and aliasing an outer local.
    pub comprehension_bindings: Vec<CheckedComprehensionBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedComprehensionBinding {
    pub name: String,
    pub owner: crate::origin::OwnerId,
    pub ty: Ty,
    pub mutable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckedDeclId(pub u32);

/// Declaration identity is independent of declaration spelling. Future classes,
/// pattern binders, coroutine state declarations, and generated declarations can
/// add variants without changing consumers of the common metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckedDeclKind {
    Function,
    Struct,
    Trait,
    Binding,
    CompileTime,
}

#[derive(Debug, Clone)]
pub struct CheckedDeclaration {
    pub id: CheckedDeclId,
    pub kind: CheckedDeclKind,
    pub name: String,
    pub location: SourceSpan,
    pub ty: Option<Ty>,
    pub children: Vec<CheckedDeclId>,
}

/// A successfully checked program plus semantic facts that downstream phases
/// previously recomputed from AST syntax or checker-private side tables.
#[derive(Debug, Clone)]
pub struct CheckedProgram {
    statements: Vec<Stmt>,
    compatibility_overload_targets: HashMap<SourceSpan, String>,
    compatibility_implicit_conversions: HashMap<SourceSpan, String>,
    checked_types: HashMap<AnnotationSite, Ty>,
    expressions: Vec<CheckedExpr>,
    expression_index: HashMap<SourceSpan, Vec<CheckedNodeId>>,
    declarations: Vec<CheckedDeclaration>,
    explicit_destroy_types: HashMap<String, ExplicitDestroyInfo>,
}

#[derive(Debug, Clone)]
pub struct ExplicitDestroyInfo {
    pub message: String,
    pub destructors: HashMap<String, bool>,
    /// Direct fields whose types carry their own explicit-destroy obligation.
    pub fields: HashMap<String, String>,
}

/// The declaration-owned location of a source annotation. Unlike `SourceType`
/// syntax itself, this identity preserves the scope in which syntax such as a
/// bare `T` was resolved.
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
/// Literal value retained after semantic checking for declaration metadata.
pub enum CheckedConst {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    None,
}

impl CheckedConst {
    /// Convert an expression that is already a literal constant.
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
    // This is the single checker-to-checked-boundary assembly point. Keeping the
    // phase-owned fact tables explicit makes accidental omission visible.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        statements: Vec<Stmt>,
        overload_targets: HashMap<SourceSpan, String>,
        implicit_conversions: HashMap<SourceSpan, String>,
        checked_types: HashMap<AnnotationSite, Ty>,
        expression_types: HashMap<SourceSpan, Ty>,
        expression_bindings: HashMap<SourceSpan, crate::origin::OwnerId>,
        comprehension_bindings: HashMap<SourceSpan, Vec<CheckedComprehensionBinding>>,
        expression_place_types: HashMap<SourceSpan, Ty>,
        binding_types: HashMap<SourceSpan, Ty>,
        expression_effects: HashMap<SourceSpan, EffectFacts>,
        iteration_protocols: HashMap<SourceSpan, IterationProtocol>,
        simd_constructions: HashMap<SourceSpan, (crate::ast::Dtype, i64)>,
        variant_operations: HashMap<SourceSpan, SemanticAdjustment>,
        explicit_destroy_types: HashMap<String, ExplicitDestroyInfo>,
        explicit_destroy_calls: HashSet<SourceSpan>,
        reference_value_uses: HashMap<SourceSpan, bool>,
    ) -> Self {
        let (expressions, expression_index) = build_checked_expressions(
            &statements,
            &expression_types,
            &expression_bindings,
            &comprehension_bindings,
            &expression_place_types,
            &binding_types,
            &expression_effects,
            &iteration_protocols,
            &simd_constructions,
            &variant_operations,
            &overload_targets,
            &implicit_conversions,
            &explicit_destroy_calls,
            &reference_value_uses,
        );
        let declarations = build_checked_declarations(&statements, &checked_types);
        Self {
            statements,
            compatibility_overload_targets: overload_targets,
            compatibility_implicit_conversions: implicit_conversions,
            checked_types,
            expressions,
            expression_index,
            declarations,
            explicit_destroy_types,
        }
    }

    /// Elaborated statements accepted by semantic checking.
    pub fn statements(&self) -> &[Stmt] {
        &self.statements
    }

    /// Checker-selected lowered callable name at each resolved call site.
    pub fn overload_targets(&self) -> &HashMap<SourceSpan, String> {
        &self.compatibility_overload_targets
    }

    /// Checker-selected converting constructor for an expression used in a
    /// context that permits a user-defined implicit conversion.
    pub fn implicit_conversions(&self) -> &HashMap<SourceSpan, String> {
        &self.compatibility_implicit_conversions
    }

    pub(crate) fn checked_type_at(&self, site: &AnnotationSite) -> Option<&Ty> {
        self.checked_types.get(site)
    }

    pub fn expressions(&self) -> &[CheckedExpr] {
        &self.expressions
    }

    pub fn declarations(&self) -> &[CheckedDeclaration] {
        &self.declarations
    }

    pub fn expression(&self, id: CheckedNodeId) -> Option<&CheckedExpr> {
        self.expressions.get(id.0 as usize)
    }

    /// Compatibility lookup while HIR migrates from syntax to checked ids.
    /// Multiple nodes may share a source location, so callers must never use the
    /// span itself as semantic identity.
    pub fn expression_ids_at(&self, span: &SourceSpan) -> &[CheckedNodeId] {
        self.expression_index
            .get(span)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn explicit_destroy_types(&self) -> &HashMap<String, ExplicitDestroyInfo> {
        &self.explicit_destroy_types
    }

    /// Module identity attached to a declaration or expression location.
    pub fn declaration_module<'a>(&self, location: &'a SourceSpan) -> Option<&'a str> {
        location.source.as_deref()
    }
}

// Mirrors `CheckedProgram::new` while the fact tables are folded into stable
// checked nodes. A named input record can replace this once the old maps retire.
#[allow(clippy::too_many_arguments)]
fn build_checked_expressions(
    statements: &[Stmt],
    types: &HashMap<SourceSpan, Ty>,
    bindings: &HashMap<SourceSpan, crate::origin::OwnerId>,
    comprehension_bindings: &HashMap<SourceSpan, Vec<CheckedComprehensionBinding>>,
    place_types: &HashMap<SourceSpan, Ty>,
    binding_types: &HashMap<SourceSpan, Ty>,
    effects: &HashMap<SourceSpan, EffectFacts>,
    iteration_protocols: &HashMap<SourceSpan, IterationProtocol>,
    simd_constructions: &HashMap<SourceSpan, (crate::ast::Dtype, i64)>,
    variant_operations: &HashMap<SourceSpan, SemanticAdjustment>,
    calls: &HashMap<SourceSpan, String>,
    conversions: &HashMap<SourceSpan, String>,
    explicit_destroy: &HashSet<SourceSpan>,
    reference_value_uses: &HashMap<SourceSpan, bool>,
) -> (Vec<CheckedExpr>, HashMap<SourceSpan, Vec<CheckedNodeId>>) {
    struct Builder<'a> {
        nodes: Vec<CheckedExpr>,
        index: HashMap<SourceSpan, Vec<CheckedNodeId>>,
        types: &'a HashMap<SourceSpan, Ty>,
        bindings: &'a HashMap<SourceSpan, crate::origin::OwnerId>,
        comprehension_bindings: &'a HashMap<SourceSpan, Vec<CheckedComprehensionBinding>>,
        place_types: &'a HashMap<SourceSpan, Ty>,
        binding_types: &'a HashMap<SourceSpan, Ty>,
        effects: &'a HashMap<SourceSpan, EffectFacts>,
        iteration_protocols: &'a HashMap<SourceSpan, IterationProtocol>,
        simd_constructions: &'a HashMap<SourceSpan, (crate::ast::Dtype, i64)>,
        variant_operations: &'a HashMap<SourceSpan, SemanticAdjustment>,
        calls: &'a HashMap<SourceSpan, String>,
        conversions: &'a HashMap<SourceSpan, String>,
        explicit_destroy: &'a HashSet<SourceSpan>,
        reference_value_uses: &'a HashMap<SourceSpan, bool>,
    }
    impl Builder<'_> {
        fn expr(&mut self, expression: &Expr) -> CheckedNodeId {
            use ExprKind::*;
            let mut children = Vec::new();
            let mut add = |this: &mut Self, child: &Expr| children.push(this.expr(child));
            match &expression.kind {
                Prefix(_, value) | Transfer(value) | Spread(value) | Named { value, .. } => {
                    add(self, value)
                }
                Infix(_, left, right)
                | Index {
                    object: left,
                    index: right,
                } => {
                    add(self, left);
                    add(self, right);
                }
                Call { args, kwargs, .. } => {
                    for value in args {
                        add(self, value);
                    }
                    for value in kwargs {
                        add(self, &value.value);
                    }
                }
                Invoke {
                    callee,
                    args,
                    kwargs,
                    ..
                } => {
                    add(self, callee);
                    for value in args {
                        add(self, value);
                    }
                    for value in kwargs {
                        add(self, &value.value);
                    }
                }
                Member { object, .. } => add(self, object),
                MethodCall {
                    object,
                    args,
                    kwargs,
                    ..
                } => {
                    add(self, object);
                    for value in args {
                        add(self, value);
                    }
                    for value in kwargs {
                        add(self, &value.value);
                    }
                }
                ListLit(values) | TupleLit(values) => {
                    for value in values {
                        add(self, value);
                    }
                }
                BraceLit(values) => {
                    for (key, value) in values {
                        add(self, key);
                        if let Some(value) = value {
                            add(self, value);
                        }
                    }
                }
                Comprehension {
                    key,
                    value,
                    clauses,
                    ..
                } => {
                    for clause in clauses {
                        match clause {
                            crate::ast::ComprehensionClause::For { iter, .. } => add(self, iter),
                            crate::ast::ComprehensionClause::If(condition) => {
                                add(self, condition)
                            }
                        }
                    }
                    if let Some(key) = key {
                        add(self, key);
                    }
                    add(self, value);
                }
                IfExpr {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    add(self, cond);
                    add(self, then_branch);
                    add(self, else_branch);
                }
                Compare { first, rest } => {
                    add(self, first);
                    for (_, value) in rest {
                        add(self, value);
                    }
                }
                Slice {
                    object,
                    lower,
                    upper,
                    step,
                    ..
                } => {
                    add(self, object);
                    for value in [lower, upper, step].into_iter().flatten() {
                        add(self, value);
                    }
                }
                MultiIndex { object, args } => {
                    add(self, object);
                    for argument in args {
                        match argument {
                            crate::ast::SubscriptArg::Index(value) => add(self, value),
                            crate::ast::SubscriptArg::Slice {
                                lower, upper, step, ..
                            } => {
                                for value in [lower, upper, step].into_iter().flatten() {
                                    add(self, value);
                                }
                            }
                        }
                    }
                }
                TString { parts, .. } => {
                    for part in parts {
                        if let crate::ast::TStringPart::Expr(value) = part {
                            add(self, value);
                        }
                    }
                }
                Int(_)
                | Float(_)
                | Bool(_)
                | Str(_)
                | None
                | Uninitialized
                | Identifier(_)
                | TypeValue(_)
                | TypeApply { .. } => {}
            }
            let span = expression.source_span();
            let mut adjustments = Vec::new();
            if let Some(target) = self.calls.get(&span) {
                adjustments.push(SemanticAdjustment::ResolveCallable(target.clone()));
            }
            if let Some(target) = self.conversions.get(&span) {
                adjustments.push(SemanticAdjustment::ImplicitConversion(target.clone()));
            }
            if matches!(expression.kind, Transfer(_)) {
                adjustments.push(SemanticAdjustment::Move);
            }
            if self.explicit_destroy.contains(&span) {
                adjustments.push(SemanticAdjustment::ExplicitDestroy);
            }
            if let Some(protocol) = self.iteration_protocols.get(&span) {
                adjustments.push(SemanticAdjustment::Iterate(protocol.clone()));
            }
            if let Some(writable) = self.reference_value_uses.get(&span) {
                adjustments.push(if *writable {
                    SemanticAdjustment::BorrowMutable
                } else {
                    SemanticAdjustment::BorrowShared
                });
            }
            if let Some((dtype, width)) = self.simd_constructions.get(&span) {
                adjustments.push(SemanticAdjustment::ConstructSimd {
                    dtype: *dtype,
                    width: *width,
                });
            }
            if let Some(operation) = self.variant_operations.get(&span) {
                adjustments.push(operation.clone());
            }
            let variant_projection = self.variant_operations.get(&span).is_some_and(|operation| {
                matches!(operation, SemanticAdjustment::VariantProject { .. })
            });
            let category = match expression.kind {
                Identifier(_) | Member { .. } | Index { .. } => ValueCategory::Place,
                TypeApply { .. } if variant_projection => ValueCategory::Place,
                TypeApply { .. } | TypeValue(_) => ValueCategory::Type,
                _ => ValueCategory::Value,
            };
            let id = CheckedNodeId(self.nodes.len() as u32);
            self.nodes.push(CheckedExpr {
                id,
                syntax: expression.clone(),
                ty: self.types.get(&span).cloned(),
                place_ty: self.place_types.get(&span).cloned(),
                binding_ty: self.binding_types.get(&span).cloned(),
                category,
                binding: self.bindings.get(&span).copied(),
                effects: self.effects.get(&span).cloned().unwrap_or_default(),
                adjustments,
                children,
                comprehension_bindings: self
                    .comprehension_bindings
                    .get(&span)
                    .cloned()
                    .unwrap_or_default(),
            });
            self.index.entry(span).or_default().push(id);
            id
        }

        fn block(&mut self, statements: &[Stmt]) {
            use crate::ast::StmtKind::*;
            for statement in statements {
                match &statement.kind {
                    VarDecl { value, .. }
                    | RefDecl { value, .. }
                    | Assign { value, .. }
                    | Comptime { value, .. }
                    | Raise(value)
                    | Expr(value) => {
                        self.expr(value);
                    }
                    AugAssign { place, value, .. } | SetPlace { place, value } => {
                        self.expr(place);
                        self.expr(value);
                    }
                    Unpack { targets, value } => {
                        for target in targets {
                            self.expr(target);
                        }
                        self.expr(value);
                    }
                    Def {
                        params,
                        where_clause,
                        body,
                        ..
                    } => {
                        for param in params {
                            if let Some(value) = &param.default {
                                self.expr(value);
                            }
                        }
                        if let Some(value) = where_clause {
                            self.expr(value);
                        }
                        self.block(body);
                    }
                    Struct {
                        conformance_conditions,
                        associated,
                        methods,
                        ..
                    } => {
                        for (_, value) in conformance_conditions {
                            self.expr(value);
                        }
                        for value in associated {
                            self.expr(&value.value);
                        }
                        for method in methods {
                            for param in &method.params {
                                if let Some(value) = &param.default {
                                    self.expr(value);
                                }
                            }
                            if let Some(value) = &method.where_clause {
                                self.expr(value);
                            }
                            self.block(&method.body);
                        }
                    }
                    Trait { methods, .. } => {
                        for method in methods {
                            for param in &method.params {
                                if let Some(value) = &param.default {
                                    self.expr(value);
                                }
                            }
                            if let Some(value) = &method.where_clause {
                                self.expr(value);
                            }
                            if let Some(body) = &method.default_body {
                                self.block(body);
                            }
                        }
                    }
                    ComptimeIf { branches, orelse } | If { branches, orelse } => {
                        for (condition, body) in branches {
                            self.expr(condition);
                            self.block(body);
                        }
                        if let Some(body) = orelse {
                            self.block(body);
                        }
                    }
                    ComptimeFor { iter, body, .. } => {
                        self.expr(iter);
                        self.block(body);
                    }
                    While { cond, body, orelse } => {
                        self.expr(cond);
                        self.block(body);
                        if let Some(body) = orelse {
                            self.block(body);
                        }
                    }
                    For {
                        iter, body, orelse, ..
                    } => {
                        self.expr(iter);
                        self.block(body);
                        if let Some(body) = orelse {
                            self.block(body);
                        }
                    }
                    Return(value) => {
                        if let Some(value) = value {
                            self.expr(value);
                        }
                    }
                    With { items, body } => {
                        for item in items {
                            self.expr(&item.context);
                        }
                        self.block(body);
                    }
                    Try {
                        body,
                        except,
                        orelse,
                        finalbody,
                    } => {
                        self.block(body);
                        if let Some((_, body)) = except {
                            self.block(body);
                        }
                        if let Some(body) = orelse {
                            self.block(body);
                        }
                        if let Some(body) = finalbody {
                            self.block(body);
                        }
                    }
                    Import { .. } | FromImport { .. } | Pass | Break | Continue => {}
                }
            }
        }
    }
    let mut builder = Builder {
        nodes: Vec::new(),
        index: HashMap::new(),
        types,
        bindings,
        comprehension_bindings,
        place_types,
        binding_types,
        effects,
        iteration_protocols,
        simd_constructions,
        variant_operations,
        calls,
        conversions,
        explicit_destroy,
        reference_value_uses,
    };
    builder.block(statements);
    (builder.nodes, builder.index)
}

fn build_checked_declarations(
    statements: &[Stmt],
    _annotation_types: &HashMap<AnnotationSite, Ty>,
) -> Vec<CheckedDeclaration> {
    fn block(statements: &[Stmt], out: &mut Vec<CheckedDeclaration>) -> Vec<CheckedDeclId> {
        use crate::ast::StmtKind;
        let mut ids = Vec::new();
        for statement in statements {
            let (kind, name) = match &statement.kind {
                StmtKind::Def { name, .. } => (CheckedDeclKind::Function, name.clone()),
                StmtKind::Struct { name, .. } => (CheckedDeclKind::Struct, name.clone()),
                StmtKind::Trait { name, .. } => (CheckedDeclKind::Trait, name.clone()),
                StmtKind::VarDecl { name, .. } | StmtKind::RefDecl { name, .. } => {
                    (CheckedDeclKind::Binding, name.clone())
                }
                StmtKind::Comptime { name, .. } => (CheckedDeclKind::CompileTime, name.clone()),
                _ => {
                    // Control-flow declarations are discovered recursively below;
                    // no fake declaration node is introduced for the container.
                    let nested: Vec<&[Stmt]> = match &statement.kind {
                        StmtKind::If { branches, orelse }
                        | StmtKind::ComptimeIf { branches, orelse } => branches
                            .iter()
                            .map(|(_, body)| body.as_slice())
                            .chain(orelse.iter().map(Vec::as_slice))
                            .collect(),
                        StmtKind::While { body, orelse, .. }
                        | StmtKind::For { body, orelse, .. } => std::iter::once(body.as_slice())
                            .chain(orelse.iter().map(Vec::as_slice))
                            .collect(),
                        StmtKind::Try {
                            body,
                            except,
                            orelse,
                            finalbody,
                        } => std::iter::once(body.as_slice())
                            .chain(except.iter().map(|(_, body)| body.as_slice()))
                            .chain(orelse.iter().map(Vec::as_slice))
                            .chain(finalbody.iter().map(Vec::as_slice))
                            .collect(),
                        StmtKind::With { body, .. } | StmtKind::ComptimeFor { body, .. } => {
                            vec![body]
                        }
                        _ => Vec::new(),
                    };
                    for body in nested {
                        ids.extend(block(body, out));
                    }
                    continue;
                }
            };
            let id = CheckedDeclId(out.len() as u32);
            out.push(CheckedDeclaration {
                id,
                kind,
                name,
                location: statement.source_span(),
                ty: None,
                children: Vec::new(),
            });
            let children = match &statement.kind {
                StmtKind::Def { body, .. } => block(body, out),
                StmtKind::Struct { methods, .. } => methods
                    .iter()
                    .flat_map(|method| block(&method.body, out))
                    .collect(),
                _ => Vec::new(),
            };
            out[id.0 as usize].children = children;
            ids.push(id);
        }
        ids
    }
    let mut declarations = Vec::new();
    let _ = block(statements, &mut declarations);
    declarations
}
