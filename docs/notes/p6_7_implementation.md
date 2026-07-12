# Comptime Roadmap — Phase 6 & 7 Implementation Notes

This document records the implementation choices, behavior, limitations, and tests
for Phases 6 and 7 of `docs/notes/comptime.md` (delayed generic body checking and type
predicates). Both phases are implemented entirely within `src/comptime.rs`.

---

## Phase 6: Delayed Generic Body Checking for `comptime if`

**The problem:** A `comptime if`/`comptime for` inside a generic value-parameter
`def` depends on the parameter value, which is unknown when the elaborator runs
globally. Previously `def f[n: Int](): comptime if n == 0: ...` errored (`'n' is
not a compile-time value`). And because both branches must type-check when checked
once with a symbolic `n`, a type error in an unselected branch would sink the whole
function.

**The solution — monomorphization**, contained entirely in `src/comptime.rs`:

1. **Detect templates** (`collect_specializable` + `block_has_comptime`): a
   top-level `def` whose compile-time params are *all* value parameters (`[n: Int]`)
   and whose body contains a `comptime if`/`comptime for`.

2. **Defer** them: the elaborator's `Def` arm keeps such a template verbatim instead
   of trying (and failing) to elaborate its body.

3. **`monomorphize()`** runs after materialization:
   - `mono_stmt`/`mono_expr` walk the program mutably. For each call `f[k](...)` to a
     template, they evaluate the value arguments, mangle a name (`f$0`, `f$1` — `$`
     can't appear in a source identifier, so no collision), rewrite the call
     (dropping `[...]`), and enqueue a specialization job.
   - The worklist drains, `generate_spec` producing each specialization: it binds the
     value parameters in the comptime env so `self.block` **resolves the `comptime
     if`/`for`** (dropping unselected branches), folds the parameters to literals, and
     clears `type_params`. Each generated body is re-scanned so recursion
     (`sumto[n-1]`) discovers further instantiations.
   - Specializations replace their template **at its original source position**, in
     **reverse generation order** (callee before caller), respecting the checker's
     sequential, no-forward-reference binding.

**Result:** `f[0]` and `f[1]` take different branches; a type error in a *dropped*
branch is never checked, while an *instantiated* bad branch is caught. Recursion and
value-parameter `comptime for` unrolling both work.

**Limitation** (matches the doc's stop point): only pure-value-parameter generics;
type-parameter predicates (`T is Int`) are deferred, and a mixed type+value generic
containing comptime stays on the old error path.

**Tests:** 4 unit tests in `comptime_test.rs` (per-instantiation selection,
dropped-branch-not-checked, instantiated-branch-checked, recursion + unroll) plus 2
file fixtures (`assets/ok/`, `assets/type_error/`). **607 tests pass**;
`comptime.rs` is clippy-clean. (The lone `check_struct` "too many arguments" clippy
warning is in pre-existing working-tree changes to `checker.rs`, not this work.)

---

## Phase 7: Type Predicates and Type Pattern Matching

Following the doc's recommendation ("start with builtin CTFE predicates rather than
new syntax"), I implemented `is_same_type[T, U]()` — no parser changes, since it's an
ordinary call/type-application.

**What I added (all in `src/comptime.rs`):**

1. **The `is_same_type[T, U]()` builtin predicate.** In the comptime `eval` Call arm,
   `is_same_type` is intercepted before CTFE dispatch. `eval_is_same_type` resolves
   both `[...]` arguments to `Ty` values (via a new `param_arg_type` helper that
   handles `Type`, bare identifier, and `TypeApply` argument forms) and returns
   `CtValue::Bool` type equality — usable directly in a `comptime if` condition.

2. **Extended Phase 6 monomorphization to type-parameter (and mixed) generics.** The
   acceptance test's `def name[T: AnyType]()` is a *type*-parameter generic, which
   Phase 6 excluded. I:
   - Dropped `collect_specializable`'s all-value-param restriction — any generic `def`
     with a `comptime if`/`for` in its body is now a template.
   - `resolve_spec_args` classifies each call argument by its declared parameter kind
     (value → `CtValue`, type → `CtValue::Type`), evaluating types through the
     comptime environment.
   - **Design choice (Option B): type parameters stay symbolic on the specialized
     def.** The specialization keeps its type params in the signature and the call
     re-supplies the type argument (`name[Int]()` → `name$Int[Int]()`), so no
     `Ty`→`ast::Type` substitution machinery is needed. Only value parameters are
     baked into literals. `generate_spec` binds *all* parameters in the comptime env
     (so predicates resolve) while keeping type params in the signature — the branch
     bodies are then checked the usual type-erased way, with only the selected branch
     surviving.

**Behavior verified:**
- **Positive:** `name[Int]()` → `"int"`, `name[String]()` → `"other"` (each a
  distinct specialization selecting a different branch).
- **Negative:** a type predicate in a runtime `if` (not `comptime if`) is rejected —
  the checker reports `Undefined variable 'is_same_type'` (it has no runtime `Bool`
  form), exactly the doc's negative case.
- **Mixed:** `tag[Int, 0]`/`tag[Int, 5]`/`tag[String, 0]` compose a type predicate
  with a value predicate → `int-zero`/`int-n`/`other`.

**Documented limitations** (past the stop point): type-dependent generics must be
called with explicit `[...]` arguments (the elaborator can't infer types); a branch
that does a `T`-specific operation like `x + 1` is a clean `BadOperator` rejection
rather than a wrong answer (full type substitution — Option A — is deferred); and
only `is_same_type` is provided (`conforms_to` would need trait-conformance data the
elaborator doesn't carry).

**Tests:** 3 unit tests in `comptime_test.rs` + 2 file fixtures (`assets/ok/`,
`assets/type_error/`). **610 tests pass**; `comptime.rs` is clippy-clean. (The single
`check_struct` clippy warning is pre-existing in `checker.rs`, unrelated to this
work.)
