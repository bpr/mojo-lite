# Runtime representation for `**kwargs`

## Decision context

Mojito parses and binds homogeneous `**kwargs` parameters on free, generic,
instance, static, and trait-bounded calls. The ordinary call path carries ordered
keyword pairs and `match_call_slots` is the shared checker binder. Unknown
keywords are therefore the natural point at which a keyword collector takes
ownership of the remaining arguments; duplicate keywords are rejected by the
same structural contract.

The strengthened self-hosted collections establish that nested pointer-backed
containers can copy correctly and preserve insertion order. Earlier Mojito used
`HashDict[String, V]` as a provisional public collector type. The tracked Mojo
nightly subsequently standardized the purpose-built owning `StringDict[V]`, so
the generic hash-table choice is now historical rather than an open decision.

## Options

1. **Self-hosted `HashDict[String, V]`.** This gives a normal library value and
   insertion-ordered iteration. It couples every kwargs-using program to implicit
   stdlib linking, requires the checker to recognize the library declaration, and
   pays several pointer-backed copies unless the parameter is constructed directly
   in its final frame slot.
2. **A new builtin map `Value`.** This is straightforward in the VM but creates a
   second dictionary semantics and representation that the self-hosted work is
   intended to replace.
3. **Keep ordered `Vec<(String, Value)>` storage in the call ABI.** The VM already
   transports this shape. It can be collected cheaply into an owned local behind
   a restricted keyword-map type, without committing to a public builtin map.

## Required semantics

- Values are homogeneous according to the declared `**kwargs: T` element type.
- Keys are copied strings and iteration follows call-site insertion order.
- The collector is an owned callee local and never writes values back to callers.
- Explicit parameters bind first; unmatched keywords enter the collector;
  duplicates remain errors.
- `FnSig` needs keyword-collector metadata parallel to its variadic positional
  metadata, and the checker must type the local consistently on every backend.
- `callee(**kwargs^)` consumes exactly one `StringDict[T]`; entries retain source
  order, flow back through ordinary binding, and keep duplicate-key diagnostics.

## Recommendation

Keep the ordered keyword-pair vector as the internal ABI and expose it through
the nightly's self-hosted `StringDict[T]`. Do not add a second builtin dictionary
`Value`: that would duplicate ordering, ownership, iteration, and destruction
semantics already expressed by the library implementation.

## Implemented decision

The ordered pair vector remains the internal ABI. The linker injects the bundled
`StringDict` implementation for programs declaring a collector, the checker
exposes the local as `StringDict[T]`, and the VM constructs it directly in the
callee frame. For `**kwargs^`, the parser requires the transfer sigil and the VM
moves each entry out of the consumed dictionary before applying the shared call
binder. Generic specialization infers from collected values, and instance,
static, and bounded-trait methods use the same element coercion, ownership,
origin, duplicate, and selected-effect checks as ordinary calls. A forwarded
dictionary is a move and cannot be reused by the caller.
