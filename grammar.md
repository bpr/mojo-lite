# mojo-lite grammar

Grammar for the implemented subset of mojo-lite, written as a **PEG** (parsing
expression grammar). It covers everything currently implemented; update it *before*
adding syntax. mojo-lite is a **strict subset of Mojo** — every construct here is
valid Mojo, restricted (the grammar may tighten Mojo but never invent syntax it
lacks). This grammar is pure syntax: it has no semantic actions or type annotations.

(Supersedes the earlier `mojo-lite.ebnf`.)

## Why PEG

PEG alternation `|` is **ordered**: the first alternative that matches wins, so the
grammar is unambiguous by construction and matches how the recursive-descent / Pratt
parser actually behaves. Left recursion is permitted and is used here for the
left-associative operators.

## Notation

```
rule: alt1 | alt2 | ...    a rule; alternatives are tried left-to-right (ordered)
rule:                      may also be written one alternative per line:
    | alt1
    | alt2
e1 e2        sequence
e1 | e2      ordered choice (first match wins)
( e )        grouping
[ e ]   e?   optional (zero or one)
e*           zero or more
e+           one or more
s.e+         one or more e separated by s     (e.g. ','.expression+ )
'text'       literal terminal (keyword or punctuation)
UPPER        a token produced by the lexer (NAME, INT, STRING, NEWLINE, ...)
&e  !e  ~    positive lookahead / negative lookahead / cut -- available, but no
             current rule needs them
```

## Starting rule

```
file: [statements] EOF
```

`EOF` is the lexer's end-of-input token (`Token::Eof`). Blank lines produce no tokens,
so they never appear in the grammar.

### Comments and keywords (lexer)

`#` begins a comment that runs to the end of the line; the lexer discards it and emits
no token (there is no `COMMENT` token). A **comment-only line** (optional spaces then
`#…`) is treated exactly like a blank line — it produces no `NEWLINE` and never affects
indentation (no `INDENT`/`DEDENT`), so you can comment freely at any indent level. An
**inline** comment (`x = 1  # note`) is dropped but the line's `NEWLINE` is still emitted,
and a `#` inside `( … )`/`[ … ]` is skipped like any other continuation whitespace.

Following the reference tree-sitter Mojo grammar, reserved words split into the ones Mojo
**shares with Python** (`def return pass None and or not if elif else while for in break
continue raise try except finally import from as`, plus the `True`/`False` literals) and
the **Mojo-only** ones (`var struct trait comptime raises`). `Token::keyword` is the single
lookup table. Soft/contextual words such as `mut` (in `mut self`) are **not** reserved — they
lex as identifiers, so they stay usable as ordinary names, matching Mojo.

**Line joining.** A logical line may span several physical lines two ways. *Implicit*:
inside `( … )` / `[ … ]` newlines and indentation are suppressed (the brackets carry the
continuation). *Explicit*: a **backslash immediately before a newline** (`\` then `LF` or
`CRLF`) joins the two physical lines — no `NEWLINE` is emitted and the continued line's
indentation is not significant (matching Mojo's `line_continuation` lexer token). A
backslash *not* immediately followed by a newline is a lex error.

## Statements

```
statements: statement+

statement: compound_stmt | simple_stmt NEWLINE

simple_stmt:
    | var_decl
    | assignment
    | unpack_assignment
    | augmented_assignment
    | comptime_stmt
    | return_stmt
    | raise_stmt
    | import_stmt
    | 'pass'
    | 'break'
    | 'continue'
    | expression

compound_stmt:
    | function_def
    | struct_def
    | trait_def
    | if_stmt
    | while_stmt
    | for_stmt
    | with_stmt
    | try_stmt
    | comptime_if_stmt
    | comptime_for_stmt
```

A simple statement is one logical line terminated by `NEWLINE` (no `;`, hence no
multiple-statements-per-line). `assignment` is listed before `expression` so `x = e`
is taken as an assignment rather than a bare expression that then chokes on `=` — PEG
ordering does the disambiguation. A compound statement carries its own block and is
not `NEWLINE`-terminated at this level.

### var_decl, assignment, return

```
var_decl: 'var' NAME [':' type] '=' expression   # annotation optional (inferred var)
assignment: target '=' expression
unpack_assignment: target (',' target)* ','? '=' expression   # a top-level comma ⇒ tuple unpacking
augmented_assignment: target aug_op expression
aug_op: '+=' | '-=' | '*=' | '/=' | '//=' | '%=' | '**='
target: NAME | place
place: primary ('.' NAME | '[' expression ']')      # a field/index chain (checker: rooted at a variable)
comptime_stmt: 'comptime' NAME '=' expression
return_stmt: 'return' [expression]
raise_stmt: 'raise' expression
```

`var x: T = e` **declares** a new (mutable) variable. The annotation is **optional**:
`var x = e` **infers** the type from `e` (a numeric literal materializes to its default
kind — `var x = 5` is `Int`, `var f = 3.5` is `Float64`); an inferred `var` of a value
that can't be stored in a binding — a closure or the non-first-class `range` — is a
checker error. `x = e` **re-assigns** an already-declared variable (the value must keep
the declared type — a checker rule, not expressible in the grammar). The target is a `NAME` or a **place** — a field
(`p.x = e`) or index (`xs[i] = e`) chain, nested freely (`p.items[i].x = e`). The
checker requires the place's root to be a mutable location: any variable, or
`self` inside a `mut self` method (writing a field of a read-only `self` is an
error). A place write mutates the root variable's binding **in place** (value
semantics: only that binding changes).

`x = e` on an **undeclared** name is a Mojo **`var`-less variable introduction**
(implicit declaration). mojo-lite parses and type-checks it (binding the implicit
variable), but the evaluator reports it as an **unsupported feature** — a
"parse now, run later" gap (write an explicit `var x: T = e`).

**Tuple unpacking** `a, b, … = e` binds each target to the corresponding element of
the tuple `e` (Mojo's `x, y = example_tuple`; a top-level comma in the target list is
what marks the statement as an unpack). Each target obeys the same rule as an
assignment target — a `NAME` or a place — and a trailing comma is allowed
(`a, = t`). This is the **bare form only**: the `var a, b = e` form is a still-open
Mojo feature request ([modular/modular#2105](https://github.com/modular/modular/issues/2105)),
so it is *not* accepted (strict-subset — mojo-lite never invents syntax Mojo lacks).
The value tuple is evaluated **once**; each element is then bound to its target (a
`NAME` follows the assignment rules — re-assign if in scope, else a var-less
introduction).

**Augmented assignment** `target OP= e` means `target = target OP e` for the seven
arithmetic operators (`+ - * / // % **`; the bitwise/`@` forms Mojo also has are not
in this subset). The target obeys the same place rules, and the operator/result must
type-check like the expansion (`i /= 2` is an error when `i: Int`, since `/` yields
`Float64`). The place — and any index inside it — is evaluated **once** (so
`xs[f()] += 1` calls `f` a single time). Still unsupported: chained assignment
(`a = b = c`). `return` outside a function is a checker error.

A place may also be a **SIMD lane** — `v[i] = e` (and `v[i] += e`) writes lane `i`,
where `e` is a same-dtype scalar or a splatting literal (`v[0] = 5`), wrapped to the
element width like construction. A lane may be reached through a field, e.g.
`vec.data[i] = e`.

### import_stmt

```
import_stmt:
    | 'import' dotted_name ['as' NAME]
    | 'from' from_module 'import' import_targets
dotted_name: '.'.NAME+                      # e.g. mypackage.mymodule
from_module: dots [dotted_name]             # leading dots = relative; may be dots only
dots: ('.' | '...')*                        # '.' is 1 level, '...' (Ellipsis) is 3
import_targets: '*' | ','.import_target+
import_target: NAME ['as' NAME]
```

`import mymodule [as alias]` imports a whole module (dotted paths allowed:
`import mypackage.mymodule`). `from module import name [as alias], ...` imports specific
names, `from module import *` imports all (discouraged in Mojo, but valid). The module
may be **relative** — leading dots give the level (`from .mymodule import X`, `from .
import X`, `from ..pkg import X`); because the lexer tokenizes `...` as a single
ellipsis, a 3-dot level (`from ...pkg import X`) is read as one `...` (plus any further
`.`). **`from module import …` is now resolved** by a link pass (`src/module.rs`): when
a file path is given (`mojo-lite run FILE`), the module is loaded from a `.mojo` file
relative to the importing file's directory (`collections` → `collections.mojo`, dotted
`a.b` → `a/b.mojo`, relative dots climb directories) and its **top-level declarations**
(`def`/`struct`/`trait`/`comptime`, excluding `main`) are hoisted into the program ahead
of the code that uses them. Imports are resolved transitively (a module may import
others), deduplicated by path (cycles are broken). Still parse-only / no-op: a plain
`import module` (qualified `module.Name` access), `as` aliases on imported names, and a
module's top-level executable code (only declarations are hoisted). From stdin (no base
directory) imports stay unresolved.

### function_def

```
function_def: decorators 'def' NAME [params_decl] '(' [params] ')' ['raises' [type]] ['->' type] ':' block
decorators: decorator*
decorator: '@' dotted_name ['(' [args] ')'] NEWLINE   # general — any name (only `@fieldwise_init` is acted on)
dotted_name: NAME ('.' NAME)*
params: ','.param_item+ [',']
param_item:
    | '/'                                  # positional-only marker
    | '*'                                  # keyword-only marker (bare)
    | '*' NAME ':' type                    # *args (positional variadic)
    | '**' NAME ':' type                   # **kwargs (keyword variadic)
    | [convention] NAME ':' type ['=' expression]   # regular, optional default
convention: 'read' | 'mut' | 'owned' | 'out' | 'ref' [origin_spec] | 'deinit'
origin_spec: '[' ','.expression+ ']'   # an expression, a named origin, origin_of(...), or '_'
reference_binding: 'ref' NAME '=' expression
```

Every parameter is typed; omitting `-> type` means the function returns `None`. The
full Mojo parameter grammar is **parsed**. **Default values** (`b: Int = 2`) and a
trailing **`*args` homogeneous variadic** (`*values: Int`, a `List[T]` in the body)
are *implemented* for non-generic free `def`s. Homogeneous `**kwargs: T` is also
implemented: unmatched ordered keyword pairs are transported by the call ABI and
materialized as an owned self-hosted `HashDict[String, T]` local. Generic and method
`**kwargs` remain deferred. The remaining unsupported form is the `out` convention.
A `convention` word is only a convention when a parameter name follows it, so `read`,
`mut`, `ref`, etc. remain usable as parameter names (`def f(read: Int)`, `def f(ref:
Int)`). Ordering is parsed leniently. The **`ref` convention** (parametric-mutability
reference) may carry an **origin specifier** — `ref[origin] x` — whose contents (an
arbitrary expression treated as `origin_of(...)`, a named origin, or `_` for an unbound
origin) are **parsed and discarded** (origins are not modeled; `origin_of(...)` itself
is just an ordinary call expression). Named `Origin[mut=...]` parameters and the
`//` infer-only marker are parsed but their origin-specific meaning is discarded.
An optional `params_decl` list (see **Parameterization** below) makes the
function generic: its type/value parameters are in scope as bare `NAME`s in the
signature and body (e.g. `def first[T: Copyable & Movable](p: Pair[T]) -> T`, or
`def repeat[count: Int](msg: String)` with `count` a value parameter).

The optional **`raises`** effect (before `->`) marks a function that may raise an
error (Mojo's `def` is non-raising by default). An error type may follow (`raises
ValidationError`) — it is *parsed but not modeled* (mojo-lite has a single `Error`
type). mojo-lite records `raises` but **does not enforce the effect** (it does not
require that a call to a raising function be inside a `try` or in a `raises` function);
that effect analysis is deferred. Methods take the same optional `raises`.

### Parameterization (generics)

```
params_decl: '[' ','.param_decl+ ']'
param_decl: NAME ':' ( bound | type )     # a type parameter, or a value parameter
bound: '&'.NAME+
```

A `params_decl` list may follow the name in a `struct` or `def` header, declaring
compile-time **parameters**. Each `NAME: X` is either a **type parameter** — when `X`
is one or more trait names joined by `&` (`T: Copyable & Movable`) — or a **value
parameter** — when `X` is a concrete type (`n: Int`). The parser accepts both forms
uniformly; the checker classifies each by whether the annotation names a trait or a
type. Every type parameter must carry a bound (Mojo requires this — no unconstrained
parameters; least restrictive is `AnyType`); a bound names a **built-in** trait
(`AnyType`, `Copyable`, `Movable`, …) or a user `trait`. A type parameter is *opaque*
apart from its bound traits' methods. **Value parameters** are restricted to type
`Int` and are usable as `Int` values in the body (`Self.n` in a struct, bare `n` in a
function) and as the struct's type-identity arguments; they may **not** appear in field,
parameter, or return **type** annotations (no dependent types yet).

**Supplying parameters.** Parameters are supplied with a bracketed argument list before
the call/construction parentheses, or as type arguments in an annotation:

```
param_args: '[' ','.param_arg+ ']'
param_arg: type | expression            # a type argument, or a comptime value expression
```

A type parameter receives a `type`; a value parameter receives a **comptime value
expression** — an `Int` expression over literals, `comptime` constants, and the
arithmetic operators `+ - * // % **` (and unary `-`) — evaluated at compile time
(`Pair[2 + 3]` is `Pair[5]`). Explicit `param_args` supply *all* parameters positionally
(`Pair[Int]`, `FixedBuffer[8]`, `Foo[Int, 5]`). If the bracket list is **omitted**, the
checker **infers** the type parameters from the argument types (as before); a generic
with any value parameter must be supplied explicitly (a value cannot be inferred).

### struct_def

```
struct_def: decorators 'struct' NAME [params_decl] [conformance] ':' struct_block
conformance: '(' ','.NAME+ ')'
struct_block: NEWLINE INDENT struct_member+ DEDENT
struct_member: field | method
field: 'var' NAME ':' type NEWLINE
method: decorators 'def' NAME '(' [receiver [',' params] | params] ')' ['raises' [type]] ['->' type] ':' block
receiver: [convention] 'self'    # instance method; absent ⇒ a @staticmethod (no self)
```

An optional `params_decl` list after the name makes the struct generic
(`struct Pair[T: Copyable & Movable]:`, or `struct FixedBuffer[size: Int]:` with a value
parameter). Inside the struct body, refer to the struct's own type parameters as
`Self.T` and value parameters as `Self.n` (an `Int` value), and to the struct type
itself as `Self` (see **Types**); methods do not take their own parameters.

An optional `conformance` list — a parenthesized, comma-separated list of trait names
after the name (and after any `params_decl`) — declares that the struct **conforms** to
those traits (`struct Duck(Copyable, Quackable):`). The checker verifies the struct
implements every method required by each *user* trait (with a matching signature);
built-in traits (`Copyable`, …) impose no checked requirements. Conformance is
**explicit/nominal** (matches Mojo): a struct satisfies a `[T: Trait]` bound only if it
declares that trait here.

### trait_def

```
trait_def: 'trait' NAME [conformance] ':' trait_block         # conformance here = super-traits (refinement)
trait_block: NEWLINE INDENT trait_member+ DEDENT
trait_member: trait_method | trait_comptime
trait_method: 'def' NAME '(' 'self' [',' params] ')' ['->' type] ':' NEWLINE INDENT (trait_req | block) DEDENT
trait_req: '...' NEWLINE                                       # a pure requirement
trait_comptime: 'comptime' NAME ':' type NEWLINE              # a compile-time member requirement
```

A `trait` declares **method requirements**: each is a `def` header (with the implicit
`self` first parameter, like a struct method) whose body is exactly `...` (the ellipsis
token) — a requirement, not an implementation. A struct that conforms must supply a
method of matching signature. **Fully modeled:** a *pure* trait (only `...`-bodied
methods, no super-traits, no `comptime` members) and struct conformance to it.

Three further, valid-Mojo trait forms are **parsed and grammar-documented, but the
checker flags each as an unsupported feature** (semantics deferred — "parse now, run
later"): **trait inheritance / refinement** (`trait Bird(Animal, Named):` — a
parenthesized super-trait list, reusing the struct `conformance` grammar), **default
method implementations** (a real block body instead of `...`), and **`comptime`
members** (`comptime count: Int`, `comptime EltType: Copyable` — a compile-time
constant/associated-alias requirement). **Generic traits** `trait T[U]:` are **not**
valid current Mojo (verified — only conforming *structs* are parameterized), so per the
strict-subset rule no trait `[type_params]` is parsed. Also deferred: trait runtime
fields (Mojo has none — members are `comptime`).

A struct is a value type with typed `var` fields and `def` methods. The body has at
least one member; fields and methods may interleave.

- **`self`** is the implicit, untyped first parameter of every method (its type is the
  enclosing struct). By default `self` is **read-only** (a method may read `self.field`
  and call `self.method(...)` but not assign a field). Declaring the receiver **`mut self`**
  makes `self` writable: the method may do `self.field = e` (and call other `mut self`
  methods), and those mutations are **written back** to the receiver — so `c.increment()`
  persists. A `mut self` method must be called on a mutable place (a variable or a
  field/index chain), never a temporary. Other argument conventions (`mut`/`out`/`read`/`ref`
  on ordinary parameters, and `out self` / `owned self` / `ref self` receivers) are still
  deferred (parsed, flagged by the checker).
- **Decorators** use a **general grammar** — any dotted name with optional call
  arguments, one or more, stacked before a `def` or `struct` (or a struct method).
  They are parsed into the AST but **only `@fieldwise_init` is acted on**: it generates
  a constructor taking the fields in declaration order, so `NAME(v1, v2, ...)` builds an
  instance (an ordinary call — see `primary`). A struct without it has no constructor (a
  checker error to construct). Every other decorator is recorded and ignored.
- **Dunder methods** (`__init__`, `__eq__`, `__add__`, …) are ordinary methods by name and
  parse as such. **Receiver conventions** `read`/`mut`/`owned`/`out`/`ref self` parse; only
  `mut self` is modeled (writable, written back). A hand-written `__init__` (`out self`),
  an `owned self` or `ref self` method, and a `@staticmethod` (no `self`) parse but the
  checker flags them as unsupported.
- Type and value **parameters** and trait **conformance** are supported (see
  **Parameterization** and `conformance`), but not inheritance, operator overloading,
  `@value`, or value parameters of non-`Int` type. There is no member **write**
  (`p.x = e`) yet.

### Control flow

```
if_stmt:
    | 'if' expression ':' block elif_stmt
    | 'if' expression ':' block [else_block]
elif_stmt:
    | 'elif' expression ':' block elif_stmt
    | 'elif' expression ':' block [else_block]
else_block: 'else' ':' block

while_stmt: 'while' expression ':' block

for_stmt: 'for' NAME 'in' expression ':' block

with_stmt: 'with' ','.with_item+ ':' block
with_item: expression ['as' NAME]
```

Conditions are any `expression` (the checker requires `Bool`). `while` / `for` have no
loop-`else` clause. A `for` target is a single `NAME` (no tuple unpacking); its
iterable is any `expression` — in practice a `range(...)` call or a `List` (see
**Collections** and Built-ins). `break` / `continue` outside a loop are checker errors.

### with_stmt

```
with_stmt: 'with' ','.with_item+ ':' block
with_item: expression ['as' NAME]
```

A `with` statement enters one or more **context managers** for the duration of its
block (`with open(p) as f:`, `with lock():`, or several comma-separated
`with a() as x, b() as y:`). Each `with_item` is a context `expression` with an
**optional** `as NAME` binding (Mojo's protocol: `__enter__` runs on entry — its
result bound to `NAME` if present — and `__exit__` on exit). The `as` target is a
plain `NAME`; the **parenthesized** and **tuple-target** forms are not in the Mojo
docs, so (strict-subset) they are not parsed. The statement is **parsed and
grammar-documented, but the checker flags it as an unsupported feature** (the
`__enter__`/`__exit__` protocol is deferred) — a "parse now, run later" gap.

### try_stmt

```
try_stmt: 'try' ':' block [except_clause] [else_block] [finally_clause]
except_clause: 'except' [NAME] ':' block
finally_clause: 'finally' ':' block
```

`try` runs its block; if it raises, control jumps to the `except` clause (which may
bind the error to a `NAME` — there is no `except Type as e:` form, matching Mojo);
`else` runs only when the block did *not* raise; `finally` always runs. At least one of
`except` / `finally` is required (a checker rule). Errors are raised with `raise` (see
`raise_stmt`), whose operand is an `Error` (from the built-in `Error(msg)` constructor)
or a `String` (auto-wrapped). Re-raise a caught error with the transfer sigil:
`raise e^`. An **uncaught** raise surfaces as a runtime error (a deliberate signal —
the checker does not prevent it, just as it can't prevent a `range(...)` zero `step`).

### block

```
block: NEWLINE INDENT statements DEDENT
```

Only the indented form — there is no inline `if x: y` body. `INDENT` / `DEDENT` come
from the lexer's offside rule. An empty body is written `pass`.

## Expressions

Lowest precedence first; each level falls through to the next. The left-recursive
rules (`comparison`, `sum`, `term`, `primary`) encode left associativity.

```
expression:
    | NAME ':=' expression        # named expression (walrus) — parsed, deferred
    | conditional
conditional:
    | disjunction 'if' disjunction 'else' expression   # ternary `a if c else b` (implemented)
    | disjunction

disjunction:
    | conjunction ('or' conjunction)+
    | conjunction
conjunction:
    | inversion ('and' inversion)+
    | inversion
inversion:
    | 'not' inversion
    | comparison
comparison:
    | sum (compare_op sum)+       # chained: `a < b < c` (implemented — each operand once, short-circuits)
    | sum compare_op sum          # a single comparison stays an `Infix`
    | sum
compare_op: '==' | '!=' | '<=' | '<' | '>=' | '>' | 'in' | 'not' 'in'
sum:
    | sum '+' term
    | sum '-' term
    | term
term:
    | term '*' factor
    | term '/' factor
    | term '//' factor
    | term '%' factor
    | factor
factor:
    | '-' factor
    | power
power:
    | primary '**' factor
    | primary
primary:
    | primary '.' NAME '(' [args] ')'    # method call
    | primary '.' NAME                   # field access / value-parameter read (Self.n)
    | atom [param_args] '(' [args] ')'   # call/construction, optionally with explicit params
    | NAME param_args                    # parameterized type in expr position (TypeApply), e.g. UnsafePointer[Int]
    | primary '[' expression ']'         # subscript: index, v[i], ptr[i]
    | primary '[' [expression] ':' [expression] [':' [expression]] ']'  # slice `a[i:j:k]` on List/String (implemented)
    | primary '^'                        # transfer sigil (ownership move); parsed, not modeled
    | atom
atom:
    | NAME
    | INT
    | FLOAT
    | STRING
    | TSTRING                     # t-string — parsed (sub-exprs), semantics deferred
    | 'True'
    | 'False'
    | 'None'
    | list_literal
    | tuple_or_group
list_literal: '[' ','.expression+ ']'      # a non-empty List literal, e.g. [1, 2, 3]
# A parenthesized form is a Tuple when it has a comma (or is empty), else grouping.
tuple_or_group:
    | '(' ')'                                       # empty tuple ()
    | '(' expression ',' [','.expression+] [','] ')' # tuple (a,) (a, b) (a, b,)
    | '(' expression ')'                            # grouping (e)

args: ','.arg+ [',']              # positional args, then keyword args
arg: NAME '=' expression | expression   # `name=value` is a keyword argument
```

Keyword arguments (`f(x=1, y=2)`; a positional argument may not follow a keyword one)
are **implemented** for free-function and ordinary user-method calls — matched to
parameters by name, mixed with positional args, defaults, and homogeneous
`*args`. Keyword args to builtins, general struct construction, and generic
functions are still deferred. List literals use only positional elements.

Notes:

- **A single comparison is an `Infix`; a chain of ≥ 2 (`a < b < c`, `0 <= i < n`) is a
  `Compare` node** — implemented as `(a < b) and (b < c)` with each operand evaluated
  **once**, left to right, short-circuiting (a false link stops the rest). Result `Bool`.
- **Membership `x in c` / `x not in c`** share the comparison level and return `Bool`
  (`not in` is two words). `c` is a `List[T]` (is `x` an element? — `x` must coerce to an
  **equatable** `T`) or a `String` (is `x` a substring? — `x` a `String`). In infix
  position `not` can only begin `not in`; note that the prefix form `not x in c` instead
  parses as `not (x in c)` (the same truth value).
- **The walrus / named expression `NAME := e`** binds looser than every operator (so
  `(n := a + b)` is `n := (a + b)`); the target must be a bare `NAME`. It is **parsed**
  (AST `Expr::Named`) and type-checks as `e`'s type, but the evaluator reports it as an
  **unsupported feature** (a "parse now, run later" gap); since it isn't run, the checker
  does not bind the name, so a program that *uses* the walrus-bound name later won't
  type-check.
- **`primary` is left-recursive over `.NAME` (field access) and `.NAME(args)` (method
  call)**, which chain (`a.b.c`, `p.m().n`). The plain call form `atom '(' [args] ')'`
  is a *free* call whose callee must be a `NAME` — a free function, a built-in, or a
  struct constructor — so `(f)(x)` works but `f(x)(y)` does not (the result of a call is
  not a `NAME`; chain through a method instead).
- **`[` after a `primary` is disambiguated by what follows**: `NAME '[' param_args ']'
  '(' args ')'` is a call/construction with explicit compile-time parameters (Phase 2);
  `primary '[' expression ']'` *not* followed by `(` is a **subscript** — a SIMD lane
  read `v[i]` (the only subscriptable value; there is no list/dict indexing, and no lane
  *write* `v[i] = x`).
- **`^` (transfer sigil)** is a postfix that marks an ownership *move* of its operand
  (`x^`, `raise e^`). mojo-lite **parses** it anywhere a `primary` can appear but does
  **not model** ownership/move — the value semantics are unchanged, so `x^` evaluates to
  `x`. (Real move semantics — argument conventions `owned`/`mut`, `__moveinit__` — are
  deferred; the sigil is parsed for completeness.)
- **`factor`** has only unary `-` (no unary `+`, no `~`). Unary `-` applies to `Int`
  and `Float64` (not `UInt`).
- **`power` (`**`) is right-associative and binds tighter than unary `-`** (so
  `-2 ** 2` is `-(2 ** 2)` and `2 ** 3 ** 2` is `2 ** (3 ** 2)`); its right operand is a
  `factor`, so `2 ** -1` is `2 ** (-1)`.
- **Arithmetic operators and their result types** (numeric, no mixing — see Numbers):
  `+ - * // % **` take same-type operands and return that type (`+` also concatenates
  `String`); `/` (true division) returns `Float64` for any numeric operands
  (`Int / Int → Float64`). Integer `//` / `%` by zero is a runtime error.
- **`args`** is a non-empty comma-separated list: no trailing comma, no
  `*` / `**` / keyword arguments.

## Types

```
type:
    | 'Int' | 'UInt' | 'Bool' | 'String' | 'Float64' | 'None'
    | 'Self' '.' NAME              # a struct's own type parameter, inside its body
    | 'Self'                       # the enclosing struct/trait type
    | function_type               # a function/closure type — parsed, deferred
    | 'ref' [origin_spec] type    # a reference type `ref[origin] T` — parsed, deferred (origin discarded)
    | NAME [param_args]            # struct type, optionally with type/value arguments
function_type: 'def' '(' [','.type+] ')' fn_effect* '->' type
fn_effect: 'thin' | 'raises' | 'abi' '(' ... ')'    # `abi(...)` is parsed and discarded
```

`None` is a reserved keyword; `Int`, `UInt`, `Bool`, `String`, `Float64` are ordinary `NAME`s recognized
here by spelling (they are **not** reserved). Any other `NAME` is a **struct type**
(`Point`), a **type parameter** in scope (a bare `T` inside a generic `def`), or a
**parameterized struct type** with arguments (`Pair[Int]`, `Pair[T]`, or the value form
`FixedBuffer[8]` / `Pair[2 + 3]`); the checker resolves which. A `param_args` entry is a
`type` or a comptime value `expression` (see **Parameterization**). `Self.T` names one
of the enclosing struct's type parameters (Mojo spelling — a bare `T` is not in scope
inside a struct body); bare `Self` names the enclosing struct type (in a struct method)
or the conforming type (in a trait method). Inside a value-parameterized struct, bare
`Self` as a *type* is not supported (a value parameter can't appear in a type).
A **function/closure type** `def(T1, …) [thin] [raises] -> R` (parameters are types
only; `thin` marks a non-capturing function pointer, default capturing) **parses**
into `Type::Func` but the checker flags any function-typed binding as unsupported
(semantics deferred). Capture lists `{…}` on a closure are **not** parsed yet
(their current-Mojo grammar is unsettled — the old `unified` keyword was removed).
A **reference type** `ref[origin] T` (used in a `ref` return, `def f(…) -> ref[o] T:`)
likewise **parses** into `Type::Ref` (origin discarded) but the checker flags it as
unsupported. `ref` is contextual here — only the reference form when a type-starting
token follows.

A `NAME` is also a **SIMD type** when it is `SIMD[DType.<dt>, <width>]` or one of the
fixed-width **scalar aliases** (`Int8` / `Int16` / `Int32` / `Int64`, `UInt8` / `UInt16`
/ `UInt32` / `UInt64`, `Float32`) — see **SIMD** — or a **`List[T]`** (see
**Collections**). These are recognized by the checker, not reserved words.

`ref NAME = expression` parses as a distinct reference binding and is rejected by
the checker until origin-carrying reference values are modeled. Origin unions in
arguments and returns, `Origin[mut=...]` parameters, and the `//` infer-only
parameter marker are also accepted by the parser. Origin clauses are currently
discarded after parsing, so this is syntax support rather than lifetime semantics.

`comptime NAME = expression` declares a **compile-time constant** (`comptime N = 8`).
The right-hand side must be a comptime `Int` expression (literals, other `comptime`
constants, and `+ - * // % **` / unary `-`); the constant is usable both as a value
parameter argument (`FixedBuffer[N]`) and as an ordinary `Int` at runtime. (Mojo
removed `alias`; `comptime` replaces it.)

```
comptime_if_stmt:  'comptime' if_stmt      # 'comptime' 'if' … 'elif' … 'else' …
comptime_for_stmt: 'comptime' for_stmt     # 'comptime' 'for' NAME 'in' expression ':' block
```

`comptime` also prefixes an `if` or `for` to give a **compile-time conditional**
(`comptime if cond:` — with `elif`/`else` like a normal `if`) or a **compile-time,
unrolled loop** (`comptime for i in range(1, 5):`). These are Mojo's *modern*
spellings — the older `@parameter if` / `@parameter for` are **deprecated** (mojo-lite
follows current Mojo, per the strict-subset rule, and does not accept the `@parameter`
forms). Both are **implemented** by a compile-time **elaboration pass** (`src/comptime.rs`,
run between parsing and checking): `comptime if` evaluates its conditions at compile
time and keeps only the taken branch — the others are **dropped before type-checking**,
so an unselected branch may contain code valid only for other specializations; `comptime
for` unrolls over a compile-time **`range(...)` or a compile-time tuple/list**
(`comptime for s in ("a", "b"):`), substituting the loop variable with its literal value
in each body copy, bounded by an iteration **quota**. The evaluator's compile-time values
are `Int`/`Bool`/`String`/`Tuple`/`List` (integer arithmetic & comparisons, `and`/`or`/
`not`, `String` `+`/`==`, indexing a comptime tuple/list) and it reads `comptime NAME`
constants. **CTFE:** a `comptime` context may call a **pure top-level function** — run by a
small, fuel-bounded AST interpreter (`comptime CAP = next_power_of_two(17)`; supports
`if`/`while`/`for`/recursion over compile-time values, no I/O or runtime state).
**Materialization:** module-level `comptime` constants are inlined as literals into runtime
code (values, value-parameter arguments), so a top-level comptime value is usable inside
functions. Deferred: compile-time *type* values, CTFE of methods/generic functions, and
comptime constants of non-`Int`/`Bool` kind as value parameters.

## Built-ins

Built-ins are not grammar — they are ordinary `NAME`s used in a call, resolved as
built-ins only when not shadowed by a binding.

- `range(stop)` / `range(start, stop)` / `range(start, stop, step)` — the only
  iterable (the value a `for` consumes). The checker types it (1–3 `Int` arguments,
  result `range`); the evaluator implements it (half-open `[start, stop)`, zero `step`
  is a runtime error).
- `Int(x)` / `UInt(x)` / `Float64(x)` — numeric conversions: one argument of type
  `Int`, `UInt`, `Float64`, or `Bool`, producing the named type. (`Float64`→integer
  truncates toward zero; `Bool` is 0/1.)
- `print(...)` — writes its arguments (any number of *printable* values — everything
  except functions, ranges, and opaque type parameters) separated by a space, followed
  by a newline; returns `None`. Keyword arguments (`sep=` / `end=`) are not supported
  (mojo-lite has no keyword arguments). Unlike Mojo it does not require `Writable`
  conformance — any displayable value prints.
- `Error(msg)` — constructs an `Error` from a `String` (see **try_stmt**).
- `String(x)` — stringify a numeric, `Bool`, or `String` value → `String` (uses the
  value's display, so `Bool` is `true`/`false` and `Float64` keeps a decimal point — a
  mojo-lite display convention). Lets you build messages: `"n = " + String(n)`.
- `abs(x)` — absolute value of a numeric, preserving its type.
- `min(a, b)` / `max(a, b)` — two numeric arguments unified like an operator (no
  concrete-type mixing), returning their common type.
- `round(x)` — round a `Float64` to the nearest whole `Float64` (ties round half away
  from zero).
- `len(x)` — the length of a `String` (in bytes) or a `List` → `Int`.
- `List[T]()` / `List[T](a, b, …)` / `List(a, b, …)` — construct a `List` (see
  **Collections**).

## Numbers

`Int` is a 64-bit signed integer, `UInt` a 64-bit unsigned integer, `Float64` a
double. Two *concrete* numeric types never mix in one operator — `var i: Int = 1` then
`i + f` (with `f: Float64`) is a type error; convert first.

**Literal coercion.** Numeric literals are flexible: an `INT` literal coerces to `Int`,
`UInt`, or `Float64`, and a `FLOAT` literal to `Float64`, materializing to whatever the
context wants. So `var u: UInt = 0`, `var f: Float64 = 3`, `u + 1`, and `1 / 2` (=
`0.5`) all work without an explicit conversion. A literal combined only with other
literals stays a literal (`var u: UInt = 1 + 2`); combined with a concrete numeric it
takes that type (`u + 1 : UInt`). The explicit conversions `Int(x)` / `UInt(x)` /
`Float64(x)` are still needed to convert *between concrete* numeric types.

## SIMD

`SIMD[DType.<dt>, <width>]` is a fixed-width vector of `<width>` lanes of element type
`<dt>` — a built-in **parameterized** type (built on Phase 2 value parameters). `<width>`
is a comptime `Int` and must be a power of two; `<dt>` is written `DType.<dt>` where
`DType` is a built-in namespace (`DType.<dt>` is valid only inside a `SIMD[...]`
argument, never as a value).

- **Element types (`<dt>`)**: `int8` / `int16` / `int32` / `int64`, `uint8` / `uint16` /
  `uint32` / `uint64`, `float32`, `float64`, `bool`. (The wider integers and the
  low-precision floats of real Mojo are not included.) Integer arithmetic is
  **bit-accurate** — it wraps at the element width; `float32` rounds each result to single
  precision, `float64` keeps full double precision.
- **Scalar aliases**: `Int8`…`Int64`, `UInt8`…`UInt64`, `Float32` mean `SIMD[DType.<dt>,
  1]`. **`Float64` is unified with `SIMD[DType.float64, 1]`** — they are the same type
  (as in real Mojo), so a `Float64` splats into a `float64` vector, a `float64` vector's
  lane reads/writes as a `Float64`, and either annotation names the same type; internally
  a width-1 `float64` keeps `Float64`'s native representation. (`Int` / `UInt` / `Bool`
  remain separate non-SIMD types, matching Mojo.)
- **Construction**: `SIMD[DType.int32, 4](1, 2, 3, 4)` takes exactly `<width>` element
  arguments; `SIMD[DType.int32, 4](7)` **splats** one value across all lanes. A scalar
  alias constructs a width-1 vector: `Int32(5)`.
- **Operators** (elementwise, both operands the same SIMD type; a numeric *literal*
  splats to the other operand's type): `+ - *` on any numeric element type, `/` on
  `float32`. Comparisons `== != < > <= >=` return a `SIMD[DType.bool, <width>]` mask.
  (`// % **` on SIMD, mixed-width broadcast, and a value-parameter width are deferred.)
- **Lane read**: `v[i]` (`i` an `Int`) returns lane `i` as the width-1 scalar
  `SIMD[DType.<dt>, 1]`.
- **Lane write**: `v[i] = e` sets lane `i` (see **assignment**); `e` is a same-dtype
  scalar or a splatting literal, wrapped to the element width. `v[i] += e` works too.

## Collections

`List[T]` is a growable homogeneous sequence — a built-in generic type. Like Mojo, it
is a **value type**: assigning or passing a `List` **copies** it (no aliasing).

- **Construction**: `List[T]()` (empty), `List[T](a, b, …)` (explicit element type), or
  `List(a, b, …)` / the literal `[a, b, …]` (element type **inferred** from the
  arguments — numeric literals unify, so `[1, 2.0]` is `List[Float64]`). An empty
  literal `[]` and empty `List()` can't infer `T`, so use `List[T]()`.
- **`len(xs)`** → the number of elements.
- **Index read**: `xs[i]` (`i` an `Int`, in `0..len`) → the element (type `T`). Negative
  indices are not supported yet; a runtime out-of-range index is an error.
- **Iteration**: `for x in xs:` binds `x` to each element (type `T`).
- **Mutation**: `xs[i] = e` (index assignment) and the mutating methods `append(e)`,
  `insert(i, e)`, `remove(e)` (removes the first equal element), `pop([i])` (removes and
  returns the last element, or the one at index `i`), `clear()`, `reverse()`, and
  `extend(other)` (append all of another `List[T]`). A `List` is a value type, so these
  mutate the list in place at its **place** — a variable (`xs.append(e)`) or a field/index
  chain rooted at one (`bag.items.append(e)`, `p.rows[i].clear()`) — never a temporary or
  call result (a checker error).
- **Queries** (read-only, allowed on any list): `count(e)` (number of equal elements) and
  `index(e)` (index of the first equal element; a runtime error if absent). `remove`,
  `count`, and `index` compare elements, so they require an **equatable** element type
  (`Int`/`UInt`/`Float64`/`Bool`/`String`/`None`). Deferred: negative indices, `insert`/
  `pop` at a negative index, `sort`, and `Dict`/`Set`. (Membership `in` / `not in` on a
  `List` is implemented — see **Expressions**.)

`Tuple[T1, …, Tn]` is a built-in **fixed-size, heterogeneous** value type (also
`Clone`-copies).

- **Construction**: the literal `(a, b, …)` (`()` empty, `(a,)` a 1-tuple; a plain `(e)`
  is grouping). Element types are inferred; element-wise coercion means `(1, 2)` fits
  `Tuple[Float64, Float64]`.
- **Index read**: `t[i]` where `i` is a **compile-time constant** `Int` (a literal or a
  `comptime` value) — required because the elements are heterogeneous, so the result type
  depends on the (statically known) index. A runtime index, or one out of range, is a
  checker error.
- **Immutable**: no element write (`t[0] = e` is rejected), though the whole `var` can be
  re-assigned. Deferred: tuple unpacking (`var (a, b) = t`) and `for` over a tuple.

`UnsafePointer[T]` is the built-in low-level pointer — a handle to contiguous heap
storage of element type `T`. Unlike the value-type collections, a pointer **aliases**:
copying it copies the offset, so two copies refer to the *same* storage (this is what lets
a value-type struct own mutable heap storage, e.g. a self-hosted `List`).

- **Allocation**: `UnsafePointer[T].alloc(n)` (a static method on the parameterized type —
  the `Name[T]` receiver is a `TypeApply` expression) reserves `n` uninitialized slots and
  returns a pointer to the base.
- **Load / store**: `ptr[i]` reads, `ptr[i] = e` (and `ptr[i] += e`) writes the pointee at
  offset `i` (an `Int`); `e` must be a `T`.
- **`free()`**: releases the allocation (a no-op in the model's arena, which never reclaims).
- Deferred: pointer arithmetic (`ptr + i`), `.load()`/`.store()`, and the pointee-lifecycle
  methods (`init_pointee_*`, `destroy_pointee`, `take_pointee`). `UnsafePointer` is
  **unchecked** — an out-of-allocation (but in-arena) access is permitted, matching Mojo.

## Tokens (lexical structure)

Terminals above are produced by the lexer. Character classes: `letter` = ASCII
`A`–`Z` / `a`–`z`, `digit` = ASCII `0`–`9`.

```
NAME:  (letter | '_') (letter | digit | '_')*       # minus the reserved words below
INT:                                                 # 64-bit signed; '-' is the operator
    | dec_digits                                     #   decimal
    | '0' ('x'|'X') hex_digits                        #   hex 0xFF
    | '0' ('o'|'O') oct_digits                        #   octal 0o77
    | '0' ('b'|'B') bin_digits                        #   binary 0b0111
dec_digits: digit ('_'? digit)*                       # '_' digit separators (between digits)
FLOAT: dec_digits ('.' dec_digits)? exponent?        # is a FLOAT iff it has a '.' or exponent
        | dec_digits exponent                        #   (otherwise the same text is an INT)
exponent: ('e' | 'E') ('+' | '-')? digit+
STRING:                                              # single- or double-quoted, escapes below
    | ('"' | "'") (string_char | escape)*  ('"' | "'")        # may not span a newline
    | ('"""' | "'''") (any_char | escape)* ('"""' | "'''")    # triple-quoted, multi-line
escape:                                              # fully decoded to real chars
    | '\a' | '\b' | '\f' | '\n' | '\r' | '\t' | '\v' | '\\' | '\"' | "\'"   # simple
    | '\' octal_lead octal octal          # octal byte: leading digit 0–3, then 2 octal digits
    | '\x' hex hex                        # 2-hex-digit code point (0–255)
    | '\u' hex hex hex hex                # 4-hex-digit Unicode scalar
    | '\U' hex hex hex hex hex hex hex hex # 8-hex-digit Unicode scalar
    # each numeric escape names a Unicode scalar, encoded UTF-8 into the string;
    # a non-scalar value (e.g. a surrogate) or a missing/bad digit is a lex error.
    # NB: `\u`/`\U` are the exact-hex-digit forms — Mojo has NO `\u{…}` brace form.
TSTRING:                                             # interpolation; parsed into sub-exprs
    | ('t' | 'rt') STRING-body-with '{' expression '}'  and  '{{' '}}'  literal braces
    # `t"…{x+1}…"` / raw `rt"…"`; each `{…}` is a parsed expression (semantics deferred)
```

Reserved keywords (cannot be an unquoted `NAME`):

```
var  def  struct  trait  comptime  return  pass  None  and  or  not
if  elif  else  while  for  in  break  continue
raise  try  except  finally  raises
import  from  as
```

`True` / `False` lex directly to boolean literals, so they are reserved too.
Any name may be **stropped** with backticks: `` `var` ``, `` `with space` ``, and
`` `with#symbol` `` each lex as one ordinary `NAME` whose value is the text between
the delimiters. A stropped name cannot span a newline.
The type names `Int` / `UInt` / `Bool` / `String` / `Float64`, the decorator name
`fieldwise_init`, the built-in trait names (`AnyType`, `Copyable`, `Movable`, …), the
type-alias `Self`, and `self` are **not** reserved (they are ordinary `NAME`s that are
special only by position). Punctuation tokens include `@` (decorator), `.` (member
access), `&` (trait-bound conjunction), `...` (the ellipsis marking an unimplemented
trait-method requirement), `^` (the transfer sigil), and `[` / `]` (type-parameter and
type-argument lists).

Structural tokens (significant-indentation / offside rule):

- `NEWLINE` — ends a logical line; suppressed inside parentheses or brackets `[ ]`;
  blank lines emit nothing; one is synthesized at end-of-input if the last line lacks a
  trailing newline.
- `INDENT` / `DEDENT` — emitted when leading-space width increases / decreases (one
  `DEDENT` per level unwound); an unmatched dedent is an error. Indentation is ignored
  inside parentheses or brackets `[ ]`.
- `EOF` — end of input, after unwinding open `INDENT`s.

Whitespace (spaces, tabs, `\r`) between tokens is insignificant; there are no
comments.

Operators intentionally absent (strict subset; see CLAUDE.md): bitwise, augmented
assignment, and chained-comparison semantics. Do not add syntax that is not valid in
current Mojo.

## Example derivations

(1) `-a + 10 * 2`  →  `(-a) + (10 * 2)`

```
expression → disjunction → conjunction → inversion → comparison → sum
sum:
    | sum:                                  (left operand)
    |     term: factor: '-' factor
    |                   → '-' primary: atom(NAME a)
    | '+'
    | term:                                 (right operand)
          term:   factor: primary: atom(INT 10)
          '*'
          factor: primary: atom(INT 2)
```

`term` (`*`) binds tighter than `sum` (`+`); `factor` (unary `-`) binds tighter than
`term`, so `-` attaches to `a` alone.

(2)
```
if x < 0:
    pass
elif x == 0:
    pass
else:
    pass
```
```
if_stmt:
    'if'  comparison(x < 0)  ':'  block
    elif_stmt:
        'elif'  comparison(x == 0)  ':'  block
        else_block: 'else' ':' block
```

Each `block` is `NEWLINE INDENT pass NEWLINE DEDENT`; the `elif` / `else` keywords sit
at the `if`'s indentation, so the `DEDENT` closing one arm's block lands the parser
back where it picks up the next.
