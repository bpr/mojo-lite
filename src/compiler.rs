//! The authoritative whole-program compiler pipeline.

use crate::backend::BackendKind;
use crate::checked::CheckedProgram;
use crate::comptime::{ComptimeError, elaborate};
use crate::error::{OwnershipError, ParseError, RuntimeError, TypeError};
use crate::module::{LinkOptions, ModuleError, link_source_with_options, link_with_options};
use crate::runtime::Value;
use crate::{Stmt, ast::StmtKind, check_program, parse};
use std::fmt;
use std::path::Path;
/// A program that has passed linking, comptime elaboration, semantic checking,
/// and ownership analysis and is therefore ready for any backend.
#[derive(Debug, Clone)]
pub struct CompiledProgram {
    checked: CheckedProgram,
}
impl CompiledProgram {
    /// The semantically checked program carried by this ownership-verified
    /// pipeline result.
    pub fn checked(&self) -> &CheckedProgram {
        &self.checked
    }
}
#[derive(Debug, Clone)]
/// Observable result of executing a compiled program.
pub struct Execution {
    /// Captured standard output.
    pub output: String,
    /// Final named module-scope values exposed by the backend for inspection.
    pub bindings: Vec<(String, Value)>,
}
/// The stage at which the authoritative pipeline stopped.
#[derive(Debug)]
pub enum CompilerError {
    Module(ModuleError),
    Parse(ParseError),
    Comptime(ComptimeError),
    Type(TypeError),
    Ownership(OwnershipError),
    /// Typed-MIR semantic verification findings — compiler invariant
    /// violations, never user errors: the checker accepted the program, so an
    /// entry here means lowering produced metadata the backend must refuse.
    Verify(Vec<String>),
    Runtime(RuntimeError),
}
impl fmt::Display for CompilerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Module(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
            Self::Comptime(error) => error.fmt(f),
            Self::Type(error) => error.fmt(f),
            Self::Ownership(error) => error.fmt(f),
            Self::Verify(findings) => {
                write!(f, "invalid checked program: {}", findings.join("; "))
            }
            Self::Runtime(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for CompilerError {}
/// Owns stage ordering and backend selection for normal whole-program use.
#[derive(Debug, Clone)]
pub struct Compiler {
    link_options: LinkOptions,
    backend: BackendKind,
    allow_executable_module_scope: bool,
}
/// Reject runtime statements at module scope, matching Mojo's source rules.
/// Declarations, imports, compile-time constants, and `pass` are permitted.
pub fn validate_module_scope(stmts: &[Stmt]) -> Result<(), TypeError> {
    for stmt in stmts {
        let statement = match &stmt.kind {
            StmtKind::Def { .. }
            | StmtKind::Struct { .. }
            | StmtKind::Trait { .. }
            | StmtKind::Comptime { .. }
            | StmtKind::Import { .. }
            | StmtKind::FromImport { .. }
            | StmtKind::Pass => continue,
            StmtKind::VarDecl { .. } => "variable declaration",
            StmtKind::RefDecl { .. } => "reference declaration",
            StmtKind::Assign { .. } | StmtKind::SetPlace { .. } => "assignment",
            StmtKind::AugAssign { .. } => "augmented assignment",
            StmtKind::Unpack { .. } => "unpacking assignment",
            StmtKind::ComptimeIf { .. } | StmtKind::ComptimeFor { .. } => {
                "unelaborated compile-time statement"
            }
            StmtKind::If { .. } => "if statement",
            StmtKind::While { .. } => "while statement",
            StmtKind::For { .. } => "for statement",
            StmtKind::Return(_) => "return statement",
            StmtKind::Raise(_) => "raise statement",
            StmtKind::With { .. } => "with statement",
            StmtKind::Try { .. } => "try statement",
            StmtKind::Break => "break statement",
            StmtKind::Continue => "continue statement",
            StmtKind::Expr(_) => "expression statement",
        };
        return Err(TypeError::InvalidModuleScope(statement.to_string()));
    }
    Ok(())
}
impl Compiler {
    /// Construct a compiler with explicit module-link and backend policy.
    pub fn new(link_options: LinkOptions, backend: BackendKind) -> Self {
        Self {
            link_options,
            backend,
            allow_executable_module_scope: false,
        }
    }
    /// Permit executable module-scope statements for isolated compiler tests.
    /// This accepts a non-Mojo snippet dialect and must not be used by the CLI or
    /// by conformance tests.
    pub fn with_snippet_module_scope(mut self) -> Self {
        self.allow_executable_module_scope = true;
        self
    }
    /// Link and compile a source entry path through ownership verification.
    pub fn compile_path(&self, entry: &Path) -> Result<CompiledProgram, CompilerError> {
        let linked =
            link_with_options(entry, self.link_options.clone()).map_err(CompilerError::Module)?;
        self.compile_linked(linked)
    }
    /// Link in-memory source as `entry` and compile it through ownership
    /// verification.
    pub fn compile_source(
        &self,
        source: &str,
        entry: &Path,
    ) -> Result<CompiledProgram, CompilerError> {
        let linked = link_source_with_options(source, entry, self.link_options.clone())
            .map_err(CompilerError::Module)?;
        self.compile_linked(linked)
    }
    /// Compile source without a module base, as used for standard input.
    pub fn compile_unlinked(&self, source: &str) -> Result<CompiledProgram, CompilerError> {
        let parsed = parse(source).map_err(CompilerError::Parse)?;
        self.compile_linked(parsed)
    }
    /// Elaborate, check, verify, and ownership-verify an already linked
    /// statement set. Verification and ownership run over one lowered
    /// `MirProgram` — the production MIR contract the backend re-derives
    /// deterministically.
    // follow-up: cache the verified `MirProgram` in `CompiledProgram` so the
    // backend does not lower a second time (touches the `Backend` contract).
    pub fn compile_linked(&self, linked: Vec<Stmt>) -> Result<CompiledProgram, CompilerError> {
        let elaborated = elaborate(linked).map_err(CompilerError::Comptime)?;
        if !self.allow_executable_module_scope {
            validate_module_scope(&elaborated).map_err(CompilerError::Type)?;
        }
        let checked = check_program(&elaborated).map_err(CompilerError::Type)?;
        let mir = crate::mir::lower_checked_program(&checked);
        if !mir.invariant_errors.is_empty() {
            return Err(CompilerError::Verify(mir.invariant_errors));
        }
        crate::analysis::check_ownership_program(&mir).map_err(CompilerError::Ownership)?;
        Ok(CompiledProgram { checked })
    }
    /// Execute an ownership-verified program using the configured backend.
    pub fn execute(&self, program: &CompiledProgram) -> Result<Execution, CompilerError> {
        let mut backend = self.backend.instantiate().map_err(|unimplemented| {
            CompilerError::Runtime(RuntimeError::Unsupported(unimplemented))
        })?;
        backend
            .run(program.checked())
            .map_err(CompilerError::Runtime)?;
        Ok(Execution {
            output: backend.output(),
            bindings: backend.bindings(),
        })
    }
    /// Compile and execute an entry path.
    pub fn run_path(&self, entry: &Path) -> Result<Execution, CompilerError> {
        let program = self.compile_path(entry)?;
        self.execute(&program)
    }
}
impl Default for Compiler {
    fn default() -> Self {
        Self::new(LinkOptions::default(), BackendKind::Vm)
    }
}
