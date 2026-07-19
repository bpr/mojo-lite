# Symbol-Level Architecture Map

This map answers “where does this rule live?” It names the production entry
points and the symbols that own cross-phase contracts. Keep it synchronized with
refactors; implementation details belong in `docs/architecture.md`.

## Production Path

| Stage | Owning symbols | Output / invariant |
|---|---|---|
| Driver | `compiler::Compiler::{compile_path, compile_source, execute}` | The only whole-program stage ordering. |
| Lex | `lexer::Lexer`, crate-level `lex` | Spanned token stream. |
| Parse | `parser::Parser::{parse_program, parse_program_diagnostic}`, crate-level `parse` | Spanned AST; diagnostic partial AST is quarantined. |
| Link | `module::{link_with_options, link_source_with_options, LinkOptions}` | Dependency-first flat program with `SourceSpan` module identity. |
| Comptime | `comptime::elaborate`, `ct::CtValue` | Ordinary AST with compile-time control resolved. |
| Check | `checker::{check_program, Checker}`, `checked::CheckedProgram` | Authoritative semantic handoff and side tables. |
| HIR | `hir::Cfg::build_checked_fn` (unchecked `build`/`build_fn` are phase-test compatibility) | Statement CFG with nested expressions. |
| MIR | `mir::lower_checked_program`, `mir::MirProgram` | Fully register-typed A-normal IR, places, declaration metadata, source table. |
| Verify | `mir::verify::verify` | Semantic verification of typed MIR: register/place types, call contracts, CFG edges, effects, references. |
| Ownership | `analysis::check_ownership_program` (checked wrapper `check_ownership_checked`) | Move/init and loan validation over lowered MIR. |
| Drops | `analysis::elaborate_drops_program` | MIR with explicit `DropVar` operations; re-verified before execution. |
| Execute | `backend::Backend::run`, `backend::vm::VmBackend` | Output and bindings from verified MIR. |

## Cross-Phase Contracts

| Concern | Sole owner | Consumers |
|---|---|---|
| Structural call binding | `call::{match_call_slots, ArgSlot, CallSlots}` | Checker and VM call adapters. |
| Parser-to-call marker normalization | `call::{regular_marker_index, effective_keyword_only_index}` | Checker and MIR declaration lowering. |
| Callable identity and overload names | `symbol::{OverloadSets, lowered_def_name, lowered_method_name, function_symbol, method_symbol}` | Checker, MIR, VM registries, symbol tests. |
| Checked semantic facts | `checked::{CheckedProgram, CheckedConst, AnnotationSite}` | MIR, ownership driver, backends. |
| Source annotation syntax | `ast::SourceType` (alias of the AST `Type` node) | Parser, checker input, HIR/MIR source metadata. |
| Source location/provenance | `token::{Span, SourceSpan}` | AST, checker side tables, MIR diagnostics. |
| Compile-time values | `ct::CtValue` | Elaborator, specialization, checked constants. |
| Semantic types | `types::{Ty, TyArg, ParamDecl}` | Checker, checked data, MIR declarations, VM coercion. |
| Runtime values/operations | `runtime::{Value, coerce_checked, apply_infix, apply_prefix}` | VM and VM-backed CTFE. |
| Backend contract | `backend::{Backend, BackendKind}` | Compiler driver and CLI. |

## Source Versus Checked Naming

Names crossing a phase boundary must say which representation they contain:

- `SourceType`, `source_annotation`, and `param_annotations` preserve syntax
  written in the source program. They are not proof that a type is valid.
- `Ty`, `checked_type`, and `param_types` are checker-produced semantic facts.
- `AnnotationSite` identifies a source annotation; `CheckedProgram::checked_type_at`
  retrieves the semantic `Ty` resolved for that site.

Do not use an unqualified `type` field for source syntax in HIR or MIR. Compiler
invariant failures—such as checked metadata missing at a required annotation
site—must be returned as diagnostics, never encoded with `expect`, `unwrap`, or
`unreachable!` at a phase boundary.

## Internal Responsibility Boundaries

### Checker

- `checker::Checker` coordinates scopes, declarations, expression inference,
  trait conformance, and overload selection.
- `checker/calls.rs` adapts neutral call matching to `TypeError` and validates
  checker-only signature rules.
- `checker/places.rs` owns call-site place classification and alias rejection.
- `checker/generics.rs` owns unification and substitution.
- `checker/annotations.rs` converts AST annotations into checked `Ty` values.
- `checker/builtins.rs` owns built-in typing/coercion rules.

### MIR

- `mir/ir.rs` defines `MirInstr`, `MirTerm`, `MirPlace`, `MirFunction`, and
  `MirProgram`.
- `mir/mod.rs` owns ordinary AST/HIR-to-MIR lowering.
- `mir/nested.rs` owns capture analysis and nested-function lifting.

### VM and Comptime

- `backend/vm.rs` owns frame execution and instruction dispatch.
- `backend/vm/calls.rs` turns `CallSlots` into runtime values and frame slots.
- `backend/vm/places.rs` navigates projected runtime storage.
- `comptime.rs` owns evaluation, elaboration, and specialization orchestration.
- `comptime/rewrite.rs` owns AST substitution and value materialization.

## Change Routing

| If you change… | Start at… | Also inspect… |
|---|---|---|
| Syntax or AST shape | [`grammar.md`](../grammar.md), `parser.rs`, `ast.rs` | Parser tests, `frontend.md`, feature matrix. |
| Argument binding | `call.rs` | Checker/VM adapters and call-parity tests. |
| Overload identity | `symbol.rs` | Checker selection, MIR declarations, symbol/rejection tests. |
| Type rules | `checker.rs` or focused checker child | `CheckedProgram`, negative checker tests. |
| Ownership/destruction | `analysis/mod.rs` | MIR place/use forms, ownership and drop tests. |
| Runtime behavior | `backend/vm.rs` or `runtime/mod.rs` | VM tests and file fixtures. |
| Pipeline ordering | `compiler.rs` | CLI, architecture doc, compiler tests. |
| Support status | `docs/features.md` | Roadmap/todo only if future work changes. |
