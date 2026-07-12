# Mojito VM Instruction Set

This document describes the instruction set executed by mojito's register VM in
the style of an assembly-language manual.

The VM does not currently decode a packed bytecode stream. It executes the
structured `MirInstr` and `MirTerm` values defined in `src/mir/mod.rs`. The
mnemonics below are a human-readable assembly spelling for those existing
operations; they do not define a separate implementation or binary encoding.

## Machine Model

A function owns two indexed storage spaces:

- **registers**, written `%r0`, `%r1`, and so on, hold expression temporaries
- **variable slots**, written `$v0`, `$v1`, and so on, hold parameters, source
  variables, and compiler-generated locals

Registers are function-local virtual registers. They are allocated densely but
are not physical machine registers. Variable slots have stable identities across
the function's control-flow graph.

A function is a list of basic blocks:

```text
fn example(%parameters...) {
bb0:
    const.i64 %r0, 1
    var.store $v0, %r0 : Int
    jump bb1

bb1:
    var.copy %r1, $v0
    return %r1
}
```

Every block contains zero or more ordinary instructions followed by exactly one
terminator. Terminators transfer control and are documented separately.

## Assembly Notation

| Notation | Meaning |
|---|---|
| `%rN` | virtual register `N` |
| `$vN` | variable slot `N` |
| `bbN` | basic block `N` |
| `@name` | function, constructor, builtin, or resolved method symbol |
| `: Type` | optional type annotation used for coercion |
| `[$v0.field]` | a place rooted at a variable slot |
| `[$v0.items[%r2].x]` | a place with field and index projections |
| `{...}` | structured metadata, not an evaluated operand |
| `[...]` | a list of operands or optional operands |

A **place** identifies mutable storage. Its root is a variable slot and its
projection chain contains named fields and register-valued indices:

```text
[$v0]
[$v0.field]
[$v0.items[%r3].value]
```

A place is different from the value currently stored there. Place instructions
can read, write, or move from that storage without reevaluating its index
expressions.

## Instruction Summary

### Constants and variable transfer

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `const.*` | `Const` | Load a literal into a register |
| `var.copy` | `UseVar(Copy)` | Copy a variable value into a register |
| `var.move` | `UseVar(Move)` | Move a variable value into a register |
| `var.borrow` | `UseVar(BorrowShared)` | Read through a shared borrow |
| `var.borrow_mut` | `UseVar(BorrowMut)` | Read through an exclusive borrow |
| `var.store` | `DefVar` | Define or redefine a variable slot |

### Scalar computation

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `neg` | `UnOp(Neg)` | Arithmetic negation |
| `not` | `UnOp(Not)` | Logical negation |
| `add` through `not_in` | `BinOp` | Binary arithmetic, comparison, logic, or membership |

### Calls

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `call` | `Call` | Call a free function, constructor, or builtin |
| `call.method` | `MethodCall` | Invoke a method on a receiver |

### Places and aggregate access

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `field.get` | `GetField` | Read a named field from a register value |
| `index.get` | `Index` | Read an indexed element |
| `slice.get` | `Slice` | Produce a list or string slice |
| `place.load` | `LoadPlace` | Read a place without reevaluating it |
| `place.store` | `Store` | Write a value through a place |
| `place.move` | `MovePlace` | Move a value out of a place |

### Aggregate construction

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `list.make` | `MakeList` | Construct a list |
| `tuple.make` | `MakeTuple` | Construct a tuple |
| `simd.make` | `MakeSimd` | Construct or splat a SIMD value |

### Iteration

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `iter.init` | `GetIter` | Normalize a value into an iterator |
| `iter.has_next` | `HasNext` | Test whether an iterator has another element |
| `iter.next` | `Next` | Produce and consume the next element |

### Exceptions and structured regions

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `raise` | `Raise` | Raise an error value |
| `try` | `Try` | Execute structured try/except/else/finally regions |
| `unsupported` | `Unsupported` | Report an explicitly unsupported operation |

### Lifetime operations

| Mnemonic | MIR operation | Purpose |
|---|---|---|
| `drop.var` | `DropVar` | Destroy the value in a variable slot |
| `drop.reg` | `Drop` | Reserved register-drop operation |

### Terminators

| Mnemonic | MIR terminator | Purpose |
|---|---|---|
| `jump` | `Jump` | Unconditional block transfer |
| `branch` | `Branch` | Conditional block transfer |
| `return` | `Return` | Return from a function or structured region |
| `falloff` | `FallOff` | Complete a try sub-region normally |
| `escape` | `EscapeJump` | Leave a try region for an enclosing loop target |

## Constants and Variable Transfer

### `const.*` — Load Constant

```text
const.i64  %dest, integer
const.f64  %dest, float
const.bool %dest, true|false
const.str  %dest, "text"
const.none %dest
```

Loads a compile-time literal into `%dest`.

The available constant classes are signed `Int`, `Float64`, `Bool`, `String`,
and `None`. More precise integer and SIMD materialization happens through typed
variable stores, parameter coercion, conversions, or `simd.make`.

Examples:

```text
const.i64  %r0, 42
const.f64  %r1, 3.5
const.bool %r2, true
const.str  %r3, "mojito"
const.none %r4
```

### `var.copy` — Copy Variable

```text
var.copy %dest, $source
```

Copies the value in `$source` into `%dest` without emptying the variable slot.
For lifecycle-aware structs this may invoke `__copyinit__`; aggregate copies
recursively copy their contents. Reading a moved slot is a runtime error and
should already have been rejected by ownership analysis.

### `var.move` — Move Variable

```text
var.move %dest, $source
```

Transfers the value from `$source` into `%dest`. The source slot becomes a moved
tombstone. If the value's type defines `__moveinit__`, the VM performs the custom
move initialization. A later read or second move from the source is an error.

### `var.borrow` — Shared-Borrow Read

```text
var.borrow %dest, $source
```

Reads `$source` under the MIR `BorrowShared` use mode. The current VM represents
this value similarly to a non-moving read; the mode exists so ownership and
borrow analysis can distinguish the operation before execution.

### `var.borrow_mut` — Mutable-Borrow Read

```text
var.borrow_mut %dest, $source
```

Reads `$source` under the MIR `BorrowMut` use mode. Static analysis enforces
exclusive access. Runtime mutation through `mut` and `ref` parameters is
implemented by caller-place write-back at the call boundary.

### `var.store` — Define Variable

```text
var.store $dest, %source
var.store $dest, %source : Type
```

Defines or redefines `$dest` from `%source`.

With a type annotation, the VM coerces the value to that declared type. Without
one, it coerces like the value already in the slot when applicable. In ownership
analysis this is a definition: storing into a moved variable reinitializes it.

## Scalar Computation

### Unary instructions

```text
neg %dest, %operand
not %dest, %operand
```

`neg` performs arithmetic negation. `not` performs logical negation according to
the runtime truth-value rules.

### Binary instructions

All binary operations have the form:

```text
opcode %dest, %left, %right
```

| Mnemonic | Source operation | Struct dispatch |
|---|---|---|
| `add` | `a + b` | `a.__add__(b)` |
| `sub` | `a - b` | `a.__sub__(b)` |
| `mul` | `a * b` | `a.__mul__(b)` |
| `div` | `a / b` | `a.__truediv__(b)` |
| `floor_div` | `a // b` | `a.__floordiv__(b)` |
| `mod` | `a % b` | `a.__mod__(b)` |
| `pow` | `a ** b` | `a.__pow__(b)` |
| `eq` | `a == b` | `a.__eq__(b)` |
| `ne` | `a != b` | `a.__ne__(b)` |
| `lt` | `a < b` | `a.__lt__(b)` |
| `gt` | `a > b` | `a.__gt__(b)` |
| `le` | `a <= b` | `a.__le__(b)` |
| `ge` | `a >= b` | `a.__ge__(b)` |
| `and` | `a and b` | no dunder dispatch |
| `or` | `a or b` | no dunder dispatch |
| `in` | `a in b` | `b.__contains__(a)` |
| `not_in` | `a not in b` | negated `b.__contains__(a)` |

Primitive values are handled by the shared runtime arithmetic implementation.
For most operators, a struct in the left operand dispatches to its corresponding
dunder method. Membership dispatches on the right operand.

Short-circuit evaluation of source `and` and `or` is normally expressed through
control flow before MIR execution. The binary opcode remains part of the
underlying operation set.

## Function and Method Calls

### `call` — Free Call or Construction

```text
call %dest, @function(%r0, %r1)
call %dest, @function(%r0, name=%r1)
call %dest, @Generic[value=%r2](%r0)
```

Invokes a free function, builtin, or struct constructor and stores its result in
`%dest`.

The encoded operation carries more information than the compact spelling shows:

- ordered positional argument registers
- named keyword argument registers
- an optional caller place corresponding to each positional argument
- optional compile-time value-parameter registers
- the checker-resolved lowered symbol for overloaded calls

Caller places allow `mut` and `ref` parameters to write their final values back
after the call. Type parameters are erased at runtime; value parameters can be
reified as function locals or struct metadata.

Calls may dispatch to builtins, user functions, fieldwise constructors,
hand-written `__init__`, or copy constructors. Argument binding handles required,
default, positional-only, keyword-only, and variadic parameters where supported.

### `call.method` — Method Call

```text
call.method %dest, %receiver, method(%r0, %r1)
call.method %dest, %receiver, @Resolved.method$ov$Type(%r0)
```

Invokes a method on `%receiver` and stores the result in `%dest`.

The instruction carries:

- the source method name
- an optional statically resolved overload symbol
- ordinary argument registers
- an optional writable receiver place
- optional writable places for ordinary arguments

For a `mut self` method, the final receiver is written back through the receiver
place. `mut` and `ref` ordinary parameters are likewise written back through
their argument places. Builtin list methods and user-struct methods share this
instruction but take different runtime dispatch paths.

## Places and Aggregate Access

### `field.get` — Read Field

```text
field.get %dest, %base, field_name
```

Reads the named field of the struct-like value in `%base` into `%dest`. This is
an rvalue read. Writes and read-modify-write operations use place instructions.

### `index.get` — Read Element

```text
index.get %dest, %base, %index
```

Reads an indexed element from `%base`.

Supported runtime paths include:

- list, tuple, string, and SIMD indexing
- pointer arena loads
- user structs through `__getitem__`

### `slice.get` — Slice Value

```text
slice.get %dest, %object, [%lower:%upper:%step]
```

Produces a slice of a list or string. Each bound is either a register or `_` for
an omitted, direction-aware default:

```text
slice.get %r4, %r0, [%r1:%r2:_]
slice.get %r5, %r0, [_:_:%r3]
```

A zero step or an invalid bound produces a runtime error.

### `place.load` — Read Place

```text
place.load %dest, [$v0.field[%r1]]
```

Reads a previously formed place into `%dest`. It is used for the read half of a
read-modify-write operation so index expressions are evaluated exactly once.

An indexed user struct dispatches through `__getitem__`; a pointer place reads
the heap arena; ordinary fields, lists, and SIMD lanes use direct place
navigation.

### `place.store` — Write Place

```text
place.store [$v0.field[%r1]], %source
```

Writes `%source` through the destination place.

An indexed user struct dispatches through `__setitem__` and writes the mutated
receiver back. An indexed pointer writes the VM heap arena. Ordinary variable,
field, list, and SIMD places are updated directly.

### `place.move` — Partial Move

```text
place.move %dest, [$v0.field]
```

Transfers a value out of a projected place into `%dest`. The source location is
replaced with a moved tombstone. This permits a field to be moved while leaving
sibling fields usable and ensures later destruction skips the moved field.

Ownership analysis must prove the place is initialized and not used again in an
invalid way. Moving an already moved place is a runtime error.

## Aggregate Construction

### `list.make` — Construct List

```text
list.make %dest, [%r0, %r1, %r2]
```

Constructs a list from the supplied values. Numeric literal elements are
promoted to a common runtime kind where required.

### `tuple.make` — Construct Tuple

```text
tuple.make %dest, [%r0, %r1, %r2]
```

Constructs a tuple from the supplied register values. Element types may differ.

### `simd.make` — Construct SIMD Value

```text
simd.make %dest, DType.Int32, 4, [%r0, %r1, %r2, %r3]
simd.make %dest, DType.Float64, 8, [%r0]
```

Constructs a SIMD value with the specified element type and width. Supplying one
element splats it across all lanes; otherwise the element count must match the
width. Scalar aliases can also lower through this operation with width one.

## Iteration Instructions

The iterator instructions mutate a variable slot because range, list, and user
iterators carry iteration state.

### `iter.init` — Normalize Iterator

```text
iter.init $iterator
```

Normalizes `$iterator` for a `for` loop. Ranges and lists require no conversion.
A user struct is repeatedly passed through `__iter__()` until it yields a struct
that provides `__next__`, with a defensive iteration-depth limit.

### `iter.has_next` — Test Iterator

```text
iter.has_next %dest, $iterator
```

Writes a boolean indicating whether `$iterator` can produce another element.

- ranges compare the current value with the stop value using the step direction
- lists test whether they are nonempty
- user iterators call `__len__()` and test whether the returned `Int` is positive

### `iter.next` — Advance Iterator

```text
iter.next %dest, $iterator
```

Writes the current element to `%dest` and advances `$iterator` in place.

- ranges return the current counter and add the step
- lists remove and return the first element
- user iterators call `__next__(mut self)` and write the final receiver back into
  the iterator slot

Calling this instruction when no element remains is invalid; the generated loop
tests `iter.has_next` first.

## Exceptions and Structured Regions

### `raise` — Raise Error

```text
raise %source
```

Raises the value in `%source`. An `Error` or `String` supplies its message;
another value reports its runtime type. The exceptional outcome propagates to
the nearest enclosing `try` handler or out of the current function.

### `try` — Structured Exception Region

```text
try {
    body      { ... }
    except $error { ... }
    else      { ... }
    finally   { ... }
    cleanup   [$v3, $v4]
}
```

Executes structured mini-CFG regions that share the enclosing function's
registers and variable slots.

Semantics:

1. Execute `body`.
2. If `body` raises and an `except` region exists, drop the body-local cleanup
   slots, optionally bind the error, and execute the handler.
3. Execute `else` only if the body completed normally.
4. Execute `finally` on normal completion, raise, return, break, or continue.
5. A non-normal result from `finally` overrides the pending result.

The body, handler, else, and finally components are each local basic-block
graphs whose entry is block zero.

### `unsupported` — Explicit Backend Failure

```text
unsupported "description"
```

Stops execution with a clean unsupported-operation error. Lowering emits this
instruction for recognized syntax whose runtime semantics are not implemented,
instead of panicking or silently executing the wrong behavior.

## Lifetime Instructions

### `drop.var` — Destroy Variable

```text
drop.var $variable
```

Removes the value from `$variable`, leaving `None`, and destroys the removed
value. For a struct this can invoke `__del__`; fields are then dropped in reverse
declaration order. Lists, tuples, and nested structs are destroyed recursively.
Moved fields are skipped, preventing double destruction after a partial move.

Drop elaboration inserts this instruction at the variable's last use or on an
appropriate control-flow edge. Values without observable destruction make it a
semantic no-op.

### `drop.reg` — Reserved Register Drop

```text
drop.reg %register
```

Represents destruction of a register temporary. It is reserved for a future
operation or assembler VM and is currently rejected by the register VM as
unsupported. Current lifetime elaboration uses `drop.var`.

## Block Terminators

Terminators appear only as the final operation of a basic block.

### `jump` — Unconditional Transfer

```text
jump bb_target
```

Continues execution at `bb_target`.

### `branch` — Conditional Transfer

```text
branch %condition, bb_true, bb_false
```

Tests `%condition` using runtime truth-value semantics. Control transfers to
`bb_true` when true and `bb_false` otherwise.

### `return` — Return Value

```text
return %value
return.none
```

Ends the current function and returns a register value or `None`.

Within a try sub-region, `return` becomes a non-normal flow value. It propagates
through enclosing regions so cleanup and `finally` execute before the function
actually returns.

### `falloff` — Complete Region

```text
falloff
```

Marks normal completion of a try sub-region. It is not a valid ordinary function
terminator. The region runner translates it to normal flow and allows the
surrounding `try` instruction to continue with `else` or `finally` as required.

### `escape` — Escape Structured Region

```text
escape bb_target cleanup [$v3, $v4]
```

Represents `break` or `continue` leaving a try sub-region for a loop block in the
enclosing function. Before propagating the jump, the VM destroys the listed
region-local variables. Every intervening `finally` executes before control
reaches `bb_target`.

## Call ABI and Function Metadata

The instruction stream is accompanied by function and declaration metadata.
This is part of the VM contract even though it is not an opcode.

Each function records:

- block list
- register count
- variable-slot count and diagnostic names
- number and types of leading parameter slots
- which parameters are owned
- which parameters are `mut` or `ref` write-back parameters
- source spans associated with generated registers

Program declaration metadata records:

- struct field layouts
- mutating method identities
- fieldwise-construction status
- function parameter names and types
- defaults and argument markers
- generic type and value parameter declarations

The VM constructs a frame by allocating the recorded register and variable-slot
counts, placing arguments in the leading variable slots, and reifying generic
value parameters into their named slots.

## Worked Example

For source shaped like:

```mojo
def add_one(x: Int) -> Int:
    return x + 1

def main():
    var n: Int = 4
    print(add_one(n))
```

a simplified assembly rendering is:

```text
fn @add_one($v0: Int) {
bb0:
    var.borrow %r0, $v0
    const.i64 %r1, 1
    add %r2, %r0, %r1
    return %r2
}

fn @main() {
bb0:
    const.i64 %r0, 4
    var.store $v0, %r0 : Int
    var.borrow %r1, $v0
    call %r2, @add_one(%r1)
    call %r3, @print(%r2)
    return.none
}
```

The exact use mode selected for an operand is determined by checking and
lowering. The example is explanatory rather than a golden dump format.

## Opcode Inventory

The complete current inventory is:

```text
const.i64       const.f64       const.bool      const.str
const.none

var.copy        var.move        var.borrow      var.borrow_mut
var.store

neg             not
add             sub             mul             div
floor_div       mod             pow
eq              ne              lt              gt
le              ge              and             or
in              not_in

call            call.method

field.get       index.get       slice.get
place.load      place.store     place.move

list.make       tuple.make      simd.make

iter.init       iter.has_next   iter.next

raise           try             unsupported

drop.var        drop.reg

jump            branch          return          return.none
falloff         escape
```

This inventory covers every `MirInstr` and `MirTerm` variant currently defined
by mojito.
