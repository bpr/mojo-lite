use crate::checked::CheckedProgram;
use crate::error::RuntimeError;
use crate::runtime::Value;

mod vm;
pub use vm::VmBackend;

/// A pluggable execution backend. The frontend hands it a program (the checked
/// AST, lowered to verified MIR) and it executes, capturing output.
pub trait Backend {
    /// Run the whole program (top-level statements, then `main()` if present).
    fn run(&mut self, program: &CheckedProgram) -> Result<(), RuntimeError>;
    /// Captured standard output.
    fn output(&self) -> String;
    /// Final top-level bindings, for the CLI `run` dump. Empty for backends with no
    /// global environment — a debugging nicety, not core semantics.
    fn bindings(&self) -> Vec<(String, Value)> {
        Vec::new()
    }
}

/// Which backend to execute with (`--backend=…`). The register VM is the sole
/// executor today; the enum is retained as the seam for future backends (e.g.
/// Cranelift) behind the verified-MIR waist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Vm,
}

impl BackendKind {
    pub fn parse(s: &str) -> Result<BackendKind, String> {
        match s {
            "vm" => Ok(BackendKind::Vm),
            other => Err(format!("unknown backend '{other}' (expected: vm)")),
        }
    }
    /// Construct the selected backend as a trait object.
    pub fn make(self) -> Box<dyn Backend> {
        match self {
            BackendKind::Vm => Box::new(VmBackend::new()),
        }
    }
}
