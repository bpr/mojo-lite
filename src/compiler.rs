//! The authoritative whole-program compiler pipeline.

use crate::analysis::check_ownership_checked;
use crate::backend::BackendKind;
use crate::checked::CheckedProgram;
use crate::comptime::{ComptimeError, elaborate};
use crate::error::{OwnershipError, ParseError, RuntimeError, TypeError};
use crate::module::{LinkOptions, ModuleError, link_source_with_options, link_with_options};
use crate::runtime::Value;
use crate::{Stmt, check_program, parse};
use std::fmt;
use std::path::Path;

/// A program that has passed linking, comptime elaboration, semantic checking,
/// and ownership analysis and is therefore ready for any backend.
#[derive(Debug, Clone)]
pub struct CompiledProgram {
    checked: CheckedProgram,
}

impl CompiledProgram {
    pub fn checked(&self) -> &CheckedProgram {
        &self.checked
    }
}

#[derive(Debug, Clone)]
pub struct Execution {
    pub output: String,
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
}

impl Compiler {
    pub fn new(link_options: LinkOptions, backend: BackendKind) -> Self {
        Self {
            link_options,
            backend,
        }
    }

    pub fn compile_path(&self, entry: &Path) -> Result<CompiledProgram, CompilerError> {
        let linked =
            link_with_options(entry, self.link_options.clone()).map_err(CompilerError::Module)?;
        self.compile_linked(linked)
    }

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

    pub fn compile_linked(&self, linked: Vec<Stmt>) -> Result<CompiledProgram, CompilerError> {
        let elaborated = elaborate(linked).map_err(CompilerError::Comptime)?;
        let checked = check_program(&elaborated).map_err(CompilerError::Type)?;
        check_ownership_checked(&checked).map_err(CompilerError::Ownership)?;
        Ok(CompiledProgram { checked })
    }

    pub fn execute(&self, program: &CompiledProgram) -> Result<Execution, CompilerError> {
        let mut backend = self.backend.make();
        backend
            .run(program.checked())
            .map_err(CompilerError::Runtime)?;
        Ok(Execution {
            output: backend.output(),
            bindings: backend.bindings(),
        })
    }

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
