# mojito

mojito is a small Rust implementation of a strict, experimental subset of
[Mojo](https://www.modular.com/mojo). It is not Mojo, and it is not trying to
compete with Mojo's production compiler. It is a compact compiler playground for
studying the shape of a modern systems programming language: Python-like syntax,
value semantics, ownership transfer, borrowing, ASAP destruction, generics, and a
register-VM execution model.

Formerly named `mojo-lite`; the rename is only a project-name change, not a
change in language goals.

Think of it as a tiny cousin of Rust, C++, and of course, Mojo, striving for at
least syntactic compatibility with Mojo. High performance is not a current goal.
The current goal is making ownership, moves, destructors, borrowing, and
control-flow lowering visible in a codebase small enough to hold in your head.

## Status

mojito currently has:

- a lexer and Pratt parser for a useful slice of Mojo-like syntax
- a type checker with structs, functions, methods, overload sets, traits,
  generics, value parameters, builtin scalar types, lists, tuples, strings, and
  SIMD-like values
- a simple module linker for `import` / `from ... import ...` across `.mojo`
  files
- compile-time elaboration for `comptime if`, `comptime for`, richer compile-time
  values, and fuel-bounded pure-function CTFE through the MIR/VM path
- a HIR control-flow graph lowering pass
- a MIR/A-normal lowering pass with explicit registers, variables, places, moves,
  drops, calls, method calls, exceptions, and loop control
- ownership analysis for `^` moves, including use-after-move, conditional moves,
  double moves, partial field moves, and reinitialization
- borrow checking for ordinary call arguments, including mutable/shared aliasing
  checks and place-sensitive field borrowing
- liveness-driven ASAP destruction via `__del__(deinit self)`
- a register VM backend used as the runtime implementation
- self-hosted standard-library proofs in `stdlib/`, including generic
  `Optional`, `List`, `Set`, and `Dict` implementations
- basic collection/protocol traits such as `Iterable`, `Iterator`, `Sized`,
  `Equatable`, and `Comparable` where the current self-hosted library needs
  them
- fixture-based tests for accepted programs, parse errors, type errors, runtime
  errors, ownership errors, and ownership-ok cases

This project is intentionally small and direct. Rust plus `petgraph` is the core
tooling; the compiler is not wrapped in a large framework.

## Not Mojo Proper

mojito is much smaller than real Mojo. Its gaps fall into two different
categories: language subset work that belongs on the near-term roadmap, and
larger infrastructure work that may or may not ever be part of this project.

Language deficiencies:

- no complete Mojo standard library; a small self-hosted `stdlib/` exists, but it
  is a proof of direction, not a compatible replacement
- no full trait system; structural conformance, common bounds, receiver
  conventions, and associated compile-time facts exist, but default methods,
  refinement, and the complete Mojo trait model remain incomplete
- no full parametric polymorphism story comparable to Mojo; generics and value
  parameters cover the current library, but not the full language
- overload resolution is useful but intentionally conservative; same-name
  functions, methods, and constructors can overload by arity and by clearly best
  argument type, but the full Mojo ranking/coercion model is not implemented
- no complete effect system; `raises` is only partially modeled
- no full exception/unwind model beyond the VM-supported subset
- no complete model of Mojo's ownership, origins, and lifetime semantics
- limited nested-function/capture support; Mojo does not support general escaping
  closures, so this is about matching Mojo's supported non-escaping patterns, not
  adding a Python-style closure system
- no self-hosted `String`; string literals and runtime strings still rely on VM
  support while storage, Unicode, slicing, and literal interop are designed
- `Tuple` remains mostly compiler/runtime-shaped; fully self-hosting arbitrary
  heterogeneous tuples would need deeper type-level/variadic machinery
- no complete support for every Mojo expression form; some advanced forms still
  parse before they are fully checked or executed

Infrastructure and backend deficiencies:

- no deep MLIR integration
- no real GPU backend, GPU programming model, kernels, device memory model, or
  accelerator codegen
- no production optimizer
- no general MLIR dialect lowering, ABI integration, or native object generation
- no Python interop
- no real SIMD lowering to machine vector instructions; SIMD values are modeled
  at the VM value level
- no performance claim beyond "useful as a reference implementation"

The language deficiencies are the ones most likely to shrink as mojito grows.
The infrastructure deficiencies are larger bets: interesting, but not necessary
for mojito to be useful as a model implementation of ownership, borrowing,
ASAP destruction, and a register-VM compiler.

The goal is honest subset semantics. A feature is usually parsed before it is
fully supported, and unsupported semantics should fail cleanly instead of
producing a wrong answer.

## Pipeline

The compiler pipeline is:

```text
source
  -> lex
  -> parse
  -> module link
  -> comptime elaboration
  -> check
  -> HIR CFG
  -> MIR
  -> ownership / borrow / liveness analysis
  -> drop elaboration
  -> register VM
```

The major source directories are:

- `src/lexer.rs`, `src/parser.rs`, `src/ast.rs`: tokens, AST, Pratt parser, and
  statement parsing
- `src/module.rs`: filesystem-backed module loading/linking
- `src/comptime.rs`: compile-time elaboration and MIR/VM-backed CTFE support
- `src/checker.rs`: type checking, trait checks, overload resolution, call
  matching, value-parameter checks, and borrow checks
- `src/hir/mod.rs`: control-flow graph lowering
- `src/mir/mod.rs`: flattened register/place MIR
- `src/analysis/mod.rs`: move analysis, liveness, and drop insertion
- `src/backend/vm.rs`: register VM execution
- `src/runtime/mod.rs`: shared runtime values and builtin operations
- `stdlib/`: self-hosted mojito library types
- `assets/`: executable and negative test fixtures
- `tests/`: parser, checker, HIR, MIR, VM, ownership, and drop tests

The surface syntax is documented in [`grammar.md`](grammar.md). The VM transition
history and design notes live in [`vm-compiler-plan.md`](vm-compiler-plan.md).

## Build And Test

```sh
cargo build
cargo test
cargo clippy --all-targets
```

If your local environment sets `RUSTC_WRAPPER` to a tool that cannot write its
cache, this form is useful:

```sh
env RUSTC_WRAPPER= cargo test
env RUSTC_WRAPPER= cargo clippy --all-targets
```

## CLI Usage

Run a compiler stage over a file:

```sh
cargo run -- <command> [FILE]
```

Commands:

| Command | What it does |
| ------- | ------------ |
| `lex` | print the token stream, one token per line |
| `parse` | print the parsed AST |
| `check` | parse and type-check |
| `own` | parse, type-check, and run ownership analysis |
| `run` | compile and execute on the register VM |

`FILE` is optional. Use a path, `-`, or omit it to read from standard input:

```sh
cargo run -- parse assets/ok/arithmetic.mojo
cargo run -- check -
echo 'var x: Int = 1' | cargo run -- lex
cargo run -- run assets/ok/list_and_struct.mojo
```

Stage errors are written to standard error with a non-zero exit code, so the CLI
is usable in scripts.

## Writing Programs

mojito executes top-level statements. If a file defines a zero-argument
`main()`, `main()` is called after top-level evaluation.

Example:

```mojo
@fieldwise_init
struct Counter:
    var n: Int

    def bump(mut self, by: Int):
        self.n += by

def main():
    var c: Counter = Counter(10)
    c.bump(5)
    print(c.n)
```

Run it:

```sh
cargo run -- run path/to/file.mojo
```

## Fixture Workflow

The easiest way to add coverage is to place `.mojo` files under `assets/`.

The test harness walks these folders:

| Folder | Meaning |
| ------ | ------- |
| `assets/ok/` | program should lex, parse, check, pass ownership analysis, and run |
| `assets/parse_error/` | lexer or parser should reject it |
| `assets/type_error/` | parser accepts it, checker rejects it |
| `assets/runtime_error/` | checker accepts it, VM reports a runtime error |
| `assets/ownership_ok/` | ownership analysis should accept it |
| `assets/ownership_error/` | ownership analysis should reject it |

So adding an accepted language example is usually just:

```sh
$EDITOR assets/ok/my_feature.mojo
cargo test
cargo run -- run assets/ok/my_feature.mojo
```

Negative fixtures can pin part of the expected error with a top comment:

```mojo
# expect: use after move
@fieldwise_init
struct Box:
    var n: Int

def main():
    var a: Box = Box(1)
    var b: Box = a^
    print(a.n)
```

See [`assets/README.md`](assets/README.md) for the fixture rules.

## Ownership, Borrowing, And Destruction

mojito treats `^` as an ownership transfer. Moving a value leaves the source
uninitialized. Later use is rejected by analysis before the VM runs.

Examples of modeled behavior:

- `var b = a^` transfers ownership from `a` to `b`
- moving the same value twice is rejected
- moving a value on one branch and using it after the merge is rejected
- partial field moves are tracked separately from sibling fields
- assigning back to a moved field reinitializes it
- `owned` parameters consume their argument
- `mut` and `ref` parameters borrow and can write back through caller places
- conflicting borrows in the same call are rejected
- values with `__del__(deinit self)` are destroyed at last use, not scope end
- moved values are dropped once, at their new owner
- structs drop their own destructor first, then fields in reverse declaration
  order

This is the part of the project that makes it useful as a model implementation:
it shows how systems-language semantics can be enforced as compiler analyses
over a small MIR instead of being scattered through an interpreter.

## Comptime

`comptime` is implemented as an elaboration phase before runtime checking and
MIR lowering.

Supported pieces include:

- `comptime NAME = expr` constants
- `comptime if` branch selection
- `comptime for` over `range` and compile-time tuple/list values
- richer compile-time values such as integers, booleans, strings, tuples, and
  lists, plus compile-time-only type facts
- small pure-function CTFE, implemented by cloning/restricting helper bodies,
  folding compile-time-only facts, and executing the result through MIR/VM with
  fuel
- materialization of compile-time values into ordinary runtime code where the
  subset supports it

This is intentionally still a small model of Mojo's comptime system. It is
powerful enough to support the self-hosted library experiments, but not a full
replacement for Mojo's compile-time evaluation and specialization machinery.

## Overloading And Dispatch

mojito supports same-name top-level functions, methods, and constructors as
overload sets. The checker chooses a single best candidate at each call site:

- distinct arities work directly
- same-arity overloads work when argument types make one candidate uniquely best
- exact type matches beat candidates that require coercion
- ambiguous coercion cases are rejected at type-check time

The selected callee is recorded as a checker fact and preserved through MIR
lowering. Overloaded definitions lower to stable signature-based names such as
`choose$ov$Int` or `Box.__init__$ov$String`; the source still says
`choose(x)` or `Box(x)`.

This same mechanism underpins ordinary function calls, method calls, dunder
operator dispatch, subscript dispatch, and constructor selection. It is not yet a
complete implementation of Mojo's overload ranking, but it is enough for the
numeric and container patterns that need ordinary type-directed overloads.

## Self-Hosted Standard Library

mojito includes a small `stdlib/` written in mojito itself. These are
ordinary `.mojo` modules imported by programs and executed by the VM.

Current self-hosted proof types include:

- `Optional[T]`
- `List[T]`
- `Set[T]`
- `Dict[K, V]`

The point is not that these are production collections. The point is that user
structs now have enough language hooks to behave like real value types:

- dunder operator and builtin dispatch
- subscript read/write
- type-directed function, method, and constructor overloading
- `__len__`
- user iteration
- `__init__(out self)`
- `__copyinit__`
- `__moveinit__`
- `__del__(deinit self)`
- `UnsafePointer[T]`
- modules
- comptime helpers

## Development Direction

Near-term work:

- continue tightening Mojo compatibility within the chosen subset
- deepen self-hosted `stdlib/` coverage while deleting or shrinking Rust
  intrinsics where practical
- design self-hosted `String`
- clarify what remains compiler-primitive about `Tuple`
- improve diagnostics and source spans
- expand trait and generic support
- document the architecture in `docs/architecture.md`
- add more VM disassembly/introspection tools
- keep growing fixture coverage from real Mojo examples

Longer-term possible directions:

- an assembler/disassembler for the register VM
- bytecode serialization
- a second backend once MIR semantics are solid
- richer borrow checking and lifetime diagnostics
- better specialization of value-parameterized code
- deeper comptime specialization and generated declarations
- an explicitly documented unsafe/unsupported boundary

MLIR and GPU support remain north-star topics, not current implementation
features.

## Library API

The frontend stages are also available as library functions:

```rust
let tokens = mojito::lex(source)?;
let ast = mojito::parse(source)?;
mojito::check(&ast)?;
mojito::check_ownership(&ast)?;
```

For execution, use the backend trait:

```rust
use mojito::{BackendKind, Backend};

let program = mojito::parse(source)?;
mojito::check(&program)?;
mojito::check_ownership(&program)?;

let mut backend = BackendKind::Vm.make();
backend.run(&program)?;
println!("{}", backend.output());
```

## Philosophy

mojito is deliberately modest. It should be small enough to read, strict
enough to be meaningful, and honest enough to say "unsupported" when a feature
has not earned its semantics yet.

The aspiration is a clear reference implementation of a Mojo-like systems
language core: lexing, parsing, checking, MIR lowering, ownership, borrowing,
ASAP destruction, and execution on a transparent register VM.
