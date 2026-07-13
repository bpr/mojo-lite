# Runtime representation for `**kwargs`

## Decision context

Mojito parses and binds homogeneous free-function `**kwargs` parameters. The ordinary call
path already carries ordered keyword pairs and `match_call_slots` is the shared
checker binder. Unknown keywords are therefore the natural point at which a
keyword collector would take ownership of the remaining arguments; duplicate
keywords are already rejected.

The strengthened self-hosted collections establish that nested pointer-backed
containers can copy correctly, that `Dict` and `HashDict` preserve insertion
order, and that `HashDict[String, V]` can represent a homogeneous Mojo
`**kwargs: V` mapping. They also expose the cost: copying a hash dictionary walks
its dense entries, bucket-index lists, and inner pointer buffers.

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

## Recommendation

Use the existing ordered keyword-pair vector as the initial internal ABI and
delay choosing the public local type. Once implicit stdlib identity and direct
in-frame construction are available, expose or lower that storage as a
self-hosted `HashDict[String, T]`. Do not add a new builtin dictionary `Value`:
it would be cheap initially but would duplicate ordering, copying, hashing, and
iteration semantics now demonstrated by the library implementation.

## Implemented decision

The ordered pair vector remains the internal ABI. The linker injects the bundled
`HashDict` implementation for programs declaring a collector, the checker exposes
the local as `HashDict[String, T]`, and the VM constructs it directly in the callee
frame. Method and generic collectors remain separate language-expansion work.
