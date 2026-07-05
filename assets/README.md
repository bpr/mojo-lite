# Mojo asset fixtures

Drop `.mojo` files here to get them exercised by the pipeline (lex → parse → check
→ eval). `tests/assets_test.rs` walks each subdirectory and asserts every file lands
at the outcome the folder names — so **adding coverage is just putting a file in the
right folder**; no code changes.

## Folders (by where the pipeline first stops)

| folder            | meaning                                                        |
| ----------------- | ------------------------------------------------------------- |
| `ok/`             | lex + parse + check + eval all succeed                        |
| `parse_error/`    | rejected by the lexer or parser (a syntax gap/error)          |
| `type_error/`     | parses, but the checker rejects it                            |
| `runtime_error/`  | type-checks, but fails at eval — includes the `Unsupported` "parse now, run later" gaps |

Grab a Mojo file off the net, decide where mojo-lite should currently land on it,
and drop it in that folder. When mojo-lite gains a feature, a file "graduates" to an
earlier-passing folder (e.g. `parse_error/ → ok/`) — a nice, greppable diff.

## Optional: pin the exact error

A file may pin the reported message with a top comment (valid Mojo — the lexer skips
it):

```mojo
# expect: operator '+'
var x: Int = 1 + True
```

The harness then also asserts the error contains that substring.

## Note

mojo-lite runs **top-level statements**, then — like Mojo — calls a zero-argument
`main()` if the file defines one (see `ok/defines_main.mojo`). So a fixture can put
its work in `main()` and it will actually execute.
