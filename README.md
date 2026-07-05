# mojo-lite

A Rust (edition 2024, no external dependencies) front-end for a growing **strict
subset of the [Mojo](https://www.modular.com/mojo) language**. It aims to accept only
valid Mojo syntax — it may *restrict* the language (e.g. requiring some annotations
Mojo would infer) but never invents syntax Mojo lacks.

The pipeline runs in stages — **lex → parse → check** — and many constructs are parsed
and grammar-documented well before they are fully modeled: the parser accepts the broad
Mojo surface, while the checker flags the parts whose semantics are still deferred. The
surface syntax is documented in [`grammar.md`](grammar.md).

## Build & test

```sh
cargo build
cargo test
cargo clippy --all-targets
```

## Usage

The CLI runs a single stage of the pipeline over your source:

```sh
cargo run -- <mode> [FILE]
```

| Mode    | What it does                                             |
| ------- | -------------------------------------------------------- |
| `lex`   | print the token stream, one token per line               |
| `parse` | print the parsed AST (pretty-printed)                    |
| `check` | lex + parse + type-check; report `ok` or the first error |
| `run`   | run the program and print its output                     |

Running with no arguments (`cargo run`) executes a built-in demo.

### Reading files

`FILE` is optional. Pass a path to read a source file, or use `-` (or omit it) to read
from standard input:

```sh
cargo run -- parse assets/ok/arithmetic.mojo   # read a file
cargo run -- check -                            # read from stdin
echo 'var x: Int = 1' | cargo run -- lex        # pipe source in
```

A stage error is written to standard error with a non-zero exit code, so the modes
compose in scripts.

## Library

The stages are also available as library functions:

```rust
let tokens = mojo_lite::lex(source)?;   // Vec<Token>
let ast    = mojo_lite::parse(source)?; // Vec<Stmt>
```
