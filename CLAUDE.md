# CLAUDE.md

Repository guidance for Claude Code and other coding agents.

## Start Here

Mojito is a Rust compiler for a strict, executable subset of current Mojo. The
register VM is the sole runtime; there is no tree-walking execution path.

Read these documents before changing behavior:

- `docs/features.md` — authoritative support matrix.
- `docs/symbol-map.md` — symbol-level ownership and navigation map.
- `docs/architecture.md` — pipeline invariants and phase design.
- `grammar.md` and `docs/frontend.md` — accepted syntax and parser design.
- `roadmap.md` — current direction, pending work, and task lifecycle policy.

Do not copy the feature inventory into this file. Update `docs/features.md` when
support changes and update `docs/symbol-map.md` when ownership or entry points
move.

## Non-Negotiable Invariants

1. Mojito is a strict subset of current Mojo. Accepted programs must use valid
   Mojo syntax and semantics; Mojito may reject valid Mojo but must not invent a
   different language.
2. Unsupported semantics fail explicitly. Prefer an early, contextual checker
   error; use `MirInstr::Unsupported` or `RuntimeError::Unsupported` only for a
   genuine later-phase boundary.
3. `Compiler` owns the production pipeline:

   ```text
   source -> lex -> parse -> link -> comptime elaboration -> CheckedProgram
          -> HIR CFG -> MIR -> ownership/liveness -> drop elaboration -> VM
   ```

4. `CheckedProgram` is the semantic handoff. Later phases consume checked facts;
   they do not silently re-check or recover unchecked execution.
5. MIR is the stable waist. Backends consume verified MIR and checked declaration
   metadata rather than rediscovering language rules from AST syntax.
6. `src/call.rs` owns structural call binding and `src/symbol.rs` owns callable
   identity. Do not duplicate either policy in the checker, MIR, or VM.
7. Preserve source/module provenance on every AST and lowered location.

## Working Practices

- Inspect the worktree before editing. Existing staged and unstaged changes belong
  to the user; preserve unrelated work.
- Use `rg` for repository searches and `apply_patch` for edits.
- Add positive and negative tests at the phase that owns the rule.
- Parser support is not semantic support. Keep those states distinct in code,
  diagnostics, and `docs/features.md`.
- When syntax changes, update `grammar.md` first and parser tests with the code.
- When public pipeline or symbol ownership changes, update
  `docs/architecture.md` and `docs/symbol-map.md`.
- Keep comments about current invariants. Historical comparisons belong in design
  notes or commit history, not production-code commentary.

## Commands

- Required gate: `env RUSTC_WRAPPER= scripts/check`
- One integration target: `cargo test --test vm_test`
- One named test: `cargo test test_name`
- CLI: `cargo run -- <lex|parse|check|own|run> [FILE]`
- Module roots: repeat `--module-path PATH` / `-I PATH`; use `--stdlib PATH`
  to replace the bundled standard-library root.

Do not report a task complete until formatting, tests, Clippy with warnings denied,
and `git diff --check` pass.

## Test and Fixture Ownership

Integration tests are grouped by phase: lexer, parser, checker, comptime, HIR,
MIR, ownership, drops, VM, modules, symbols, compiler driver, self-hosted stdlib,
and file assets. `tests/evaluator_test.rs` is a historical filename; it exercises
the compiler-and-VM execution path.

Files under `assets/<outcome>/` run through the whole pipeline. The outcome
folders are `ok`, `parse_error`, `type_error`, `runtime_error`,
`ownership_ok`, and `ownership_error`. See `assets/README.md`.
