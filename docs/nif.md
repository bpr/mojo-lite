# Investigating NIF as an Intermediate Format

This document investigates whether the Nim language's NIF format could be useful
as an intermediate format in mojito. It is a feasibility analysis, not a decision
to adopt NIF or change mojito's compiler direction.

Primary references:

- [NIF repository and overview](https://github.com/nim-lang/nifspec)
- [NIF data format specification](https://github.com/nim-lang/nifspec/blob/master/doc/nif-spec.md)

## Summary

NIF is technically usable in mojito, but it should be understood as an
extensible serialization and container format, not as a ready-made semantic IR
comparable to mojito MIR, LLVM IR, or MLIR.

The most promising investigation path is a small, optional NIF exporter/importer
around mojito's source-shaped AST or a future checked-declaration representation.
Replacing mojito MIR with NIF would provide much less immediate benefit and would
require defining a mojito-specific NIF language almost as large as the existing
MIR.

## What NIF Provides

NIF is a textual, S-expression-like tree format designed for communication
between compiler stages and between programming languages. Its useful facilities
include:

- arbitrarily extensible tagged trees
- distinct identifiers, unambiguous symbol definitions, and symbol references
- source filename, line, and column annotations on any node
- file-per-module organization and globally qualified symbols
- optional symbol-to-byte-offset indexes
- pipeline-step suffixes such as parsed versus semantically checked files
- language or dialect identification through `.lang`
- a binary BIF encoding for faster loading and smaller caches

The specification deliberately defines syntax and basic conventions rather than
a fixed compiler IR. A producer and consumer still need to agree on tag meanings,
child ordering, typing rules, ownership rules, and validation requirements.

## Possible Positions in Mojito

| Boundary | Fit | Potential value | Main problem |
|---|---:|---|---|
| Parsed AST interchange | Good | Debug dumps, external tooling, alternative frontends | Mojito-specific tags are still required |
| Linked or elaborated AST | Good | Cache module linking or comptime results | The current linker flattens module identity |
| Checked declarations | Potentially best | Stable symbol and type metadata for tools or incremental compilation | Mojito does not yet produce a complete checked-program object |
| HIR CFG | Moderate | Structured control-flow inspection | NIF's common vocabulary does not define CFG semantics |
| MIR | Technically possible | Portable MIR dumps and backend experiments | Almost every meaningful node would be mojito-specific |
| VM bytecode or runtime format | Poor | Possible artifact serialization | NIF is tree-oriented, while the VM wants indexed blocks, registers, and compact metadata |

## Parsed or Elaborated AST

This is the easiest mapping because NIF naturally represents an AST. A
mojito-specific tree could look approximately like this:

```text
(.nif27)
(.vendor "mojito")
(.lang "mojito-ast")
(stmts
  (def :add.0.example
    (params
      (param a.0 (i 64))
      (param b.0 (i 64)))
    (i 64)
    (stmts
      (ret (add (i 64) a.0 b.0)))))
```

This could support:

- inspectable compiler snapshots
- third-party source-analysis tools
- test fixtures that do not depend on Rust debug formatting
- an experimental alternate frontend
- lossless or nearly lossless source tooling if comments and enough syntax detail
  are preserved

It would not automatically allow mojito to consume Nim ASTs. Nim and mojito
would still use different tags and semantics under their respective `.lang`
contexts.

## Checked Declarations

This is the most architecturally interesting position.

Mojito carries normalized runtime declaration metadata in `MirDeclarations`, but
that representation still contains AST types and expressions. The architecture
already identifies a future checked-declaration table as the cleaner source of
truth.

A checked NIF module could represent:

- stable declaration symbols
- resolved field and parameter types
- overload identities
- trait conformances and evidence
- generic type and value parameters
- function conventions and effects
- source locations
- optional function bodies in a source-shaped or lowered form

NIF's distinction between identifiers and globally unique symbols aligns
reasonably well with mojito's `SignatureKey` and lowered overload names. NIF
global symbols also permit an instantiation key, which could eventually
correspond to specialized generic declarations.

The obstacle is that mojito's checker currently validates an AST and returns side
products rather than a complete checked program. Introducing a proper
`CheckedProgram` would be valuable independently of NIF. NIF should serialize
that representation, not become its in-memory Rust data structure.

## HIR or MIR

Mojito MIR has semantics that NIF does not standardize:

- explicit basic blocks and terminators
- virtual registers
- variable slots
- places and projection chains
- copy, move, shared-borrow, and mutable-borrow modes
- partial moves
- caller places for `mut` and `ref` write-back
- mini-CFG exception regions
- cleanup-bearing escape edges
- explicit drop instructions
- declaration metadata consumed by the VM

These could all be encoded:

```text
(fn :example.0.module
  (vars x.0)
  (blocks
    (block 0
      (const r.0 1)
      (defvar x.0 r.0)
      (use r.1 x.0 move)
      (jump 1))
    (block 1
      (dropvar x.0)
      (return r.1))))
```

However, tags such as `block`, `defvar`, `use`, `move`, `dropvar`, place
projections, and try cleanup would be mojito-specific. The project would still
need:

- a formal mojito-NIF schema
- a verifier
- a conversion layer to and from Rust MIR
- versioning rules for every MIR change
- tests ensuring round-trip preservation

At that point, NIF supplies parsing, printing, symbols, locations, and indexing,
but not the difficult semantic definition. This makes it reasonable as an
optional MIR interchange or debug format, but not obviously advantageous as
mojito's in-memory stable waist.

## Interoperability Expectations

NIF can provide syntactic interoperability without semantic interoperability.

A generic NIF tool could:

- parse and traverse mojito trees
- preserve unknown tags
- locate symbol definitions
- inspect source locations
- build indexes
- transform trees mechanically

A Nim or Nimony backend could not compile mojito NIF merely because both use
NIF. It would need to understand:

- mojito types and generics
- ownership and borrowing
- copy, move, and drop behavior
- trait dispatch
- exception cleanup
- runtime object layouts
- mojito-specific builtins

Likewise, importing Nim NIF into mojito would require a genuine Nim-to-mojito
semantic translation layer. NIF removes format friction; it does not remove
language differences.

## Architectural Tensions

### Module identity

NIF is module-oriented: one file represents one module, and global symbol names
encode module identity. Mojito's linker currently flattens imported declarations
into one `Vec<Stmt>`, deliberately erasing most module provenance. Using NIF
effectively for module caches would require preserving module identity longer in
the pipeline.

### Source spans

NIF uses line, column, and filename annotations, including compact relative
encodings. Mojito internally uses byte spans. Conversion is possible, but
accurate round trips require retaining source files and line indexes. Byte
offsets may remain preferable inside the compiler.

### Version maturity

The current specification describes Version 2027, including changes from 2026,
while the repository has no published releases. That suggests active evolution
rather than a frozen interchange standard. Mojito would need to pin a specific
NIF version and isolate compatibility code.

### Validation

NIF's extensibility means a syntactically valid file need not be valid mojito
IR. Every imported stage needs a stage-specific verifier. Loading serialized MIR
directly into the VM without validation would violate the existing verified-MIR
boundary.

### Human-readable does not mean stable

Textual NIF would be useful for snapshots and debugging, but golden tests could
become coupled to incidental register numbering, block ordering, or symbol
disambiguation choices.

### Implementation cost

A basic generic NIF parser is small. A production-quality implementation also
needs escaping, source suffixes, symbols, directives, indexes, version handling,
diagnostics, and possibly BIF. Mojito should not implement the whole
specification merely to print MIR trees.

## Benefits Worth Testing

A limited experiment could answer several useful questions:

- Is NIF significantly more readable and stable than Rust `Debug` output for
  AST or MIR tests?
- Can declaration symbols map cleanly onto `SignatureKey` without leaking
  `$ov$` implementation spellings?
- Does retaining module identity improve mojito's module architecture?
- Can external tools consume mojito compiler snapshots with minimal knowledge?
- Is serialization fast and compact enough to be useful for module or comptime
  caches?
- Can unknown tags be preserved through round trips, supporting forward
  compatibility?

## Suggested Reversible Experiment

A useful spike would be deliberately narrow.

### 1. Implement a small NIF subset

Support only the generic tree model needed for:

- compound nodes
- empty nodes
- identifiers
- symbols and symbol definitions
- integer, float, and string atoms
- `.nif27`, `.vendor`, and `.lang`
- absolute source annotations

### 2. Define a provisional language

Use:

```text
(.lang "mojito-mir-v0")
```

The `v0` name should make it explicit that the schema is experimental and not a
compatibility promise.

### 3. Export without importing

Export a small MIR subset:

- functions
- blocks and terminators
- constants
- variable definitions and uses
- calls
- places
- declaration metadata

An exporter tests the representational fit without introducing an untrusted IR
loading path or making NIF part of normal compilation.

### 4. Exercise representative fixtures

Use three cases:

- overloaded generic calls
- a partial move followed by drop elaboration
- `try`/`finally` with an escaping return or loop jump

### 5. Evaluate the result

Ask:

- Can the output be reconstructed without ambiguity?
- Does the schema expose missing MIR invariants?
- How much code is generic NIF machinery versus mojito mapping?
- Is the resulting format useful to anything outside mojito?
- Does it remain readable after real-world lowering?

### 6. Keep the first experiment bounded

Do not initially implement:

- BIF
- symbol indexes
- NIF module loading
- Nim or Nimony compatibility
- replacement of Rust MIR
- persistent compiler caches

An AST exporter would be an even cheaper preliminary spike, but a MIR exporter
tests the stronger claim that NIF could serve as an intermediate-language
interchange format.

## Assessment

The investigation supports three distinct conclusions:

1. **NIF as a serialization syntax:** clearly feasible.
2. **NIF as an external interchange or debug form for mojito AST, checked
   declarations, or MIR:** plausible and worth a bounded exporter experiment.
3. **NIF as a replacement for mojito's semantic MIR:** technically possible,
   but currently weakly justified. It would mostly re-encode the existing MIR
   under mojito-specific tags while adding schema, conversion, and verification
   obligations.

The strongest longer-term fit is likely:

```text
mojito source
    -> internal AST and checking
    -> internal CheckedProgram
    <-> optional NIF interchange or cache
    -> internal HIR and MIR
    -> VM or future backends
```

That shape preserves mojito's typed Rust representations and explicit MIR
invariants while gaining NIF's tooling-friendly textual format where
serialization actually matters.

No direction needs to be chosen based on this investigation. A bounded exporter
spike would provide concrete evidence at relatively low architectural risk.
