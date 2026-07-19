//! Execution-backend contract below the verified-MIR waist.
//!
//! The register VM is the executable semantic oracle. Future Cranelift, eBPF,
//! LLVM, and MLIR implementations should consume the same checked program/MIR
//! facts instead of reconstructing language semantics from source declarations.
//!
//! Dispatch is a static enum, not a trait object: each implemented backend is
//! one [`Backend`] variant, so adding a backend extends the enum and every
//! `match` below rather than introducing dynamic dispatch.

use crate::checked::CheckedProgram;
use crate::error::RuntimeError;
use crate::runtime::Value;

mod vm;
pub use vm::VmBackend;

/// A statically dispatched execution backend. The frontend hands it a program
/// (the checked AST, lowered to verified MIR) and it executes, capturing
/// output.
pub enum Backend {
    Vm(VmBackend),
}

impl Backend {
    /// Run a checked program, entering through `main()` when present. Production
    /// compilation rejects executable module-scope statements; the top-level MIR
    /// block remains for declarations and explicit legacy snippet tests.
    pub fn run(&mut self, program: &CheckedProgram) -> Result<(), RuntimeError> {
        match self {
            Backend::Vm(vm) => vm.run(program),
        }
    }

    /// Captured standard output.
    pub fn output(&self) -> String {
        match self {
            Backend::Vm(vm) => vm.output(),
        }
    }

    /// Final top-level bindings, for the CLI `run` dump. Empty for backends with
    /// no global environment — a debugging nicety, not core semantics.
    pub fn bindings(&self) -> Vec<(String, Value)> {
        match self {
            Backend::Vm(vm) => vm.bindings(),
        }
    }
}

/// Which backend to execute with (`--backend=…`). The register VM is the sole
/// executor today; the other names are recognized seams for future backends
/// behind the verified-MIR waist and refuse construction until implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Vm,
    Cranelift,
    Ebpf,
    Llvm,
    Mlir,
}

impl BackendKind {
    /// The `--backend=…` spelling of this backend.
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Vm => "vm",
            BackendKind::Cranelift => "cranelift",
            BackendKind::Ebpf => "ebpf",
            BackendKind::Llvm => "llvm",
            BackendKind::Mlir => "mlir",
        }
    }

    pub fn parse(s: &str) -> Result<BackendKind, String> {
        match s {
            "vm" => Ok(BackendKind::Vm),
            "cranelift" => Ok(BackendKind::Cranelift),
            "ebpf" => Ok(BackendKind::Ebpf),
            "llvm" => Ok(BackendKind::Llvm),
            "mlir" => Ok(BackendKind::Mlir),
            other => Err(format!(
                "unknown backend '{other}' (expected: vm, cranelift, ebpf, llvm, mlir)"
            )),
        }
    }

    /// Parse a backend name and construct the selected backend, like
    /// [`Self::parse`] followed by [`Self::instantiate`].
    pub fn make(s: &str) -> Result<Backend, String> {
        Self::parse(s)?.instantiate()
    }

    /// Construct the selected backend. Recognized-but-unimplemented backends
    /// refuse here rather than pretending to execute.
    pub fn instantiate(self) -> Result<Backend, String> {
        match self {
            BackendKind::Vm => Ok(Backend::Vm(VmBackend::new())),
            BackendKind::Cranelift | BackendKind::Ebpf | BackendKind::Llvm | BackendKind::Mlir => {
                Err(format!("backend '{}' is not implemented yet", self.name()))
            }
        }
    }
}
