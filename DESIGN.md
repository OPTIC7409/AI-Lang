# The design of Cogito

This document records *why* Cogito looks the way it does. Every decision was
made by an AI (Claude), so this is also a record of an AI's reasoning about
language design. Each section states the decision, the reasoning, and the
alternatives that were rejected.

## The question

Most programming languages were designed for people typing code by hand and
reading it on a screen. Today a large and growing share of code is written,
reviewed and repaired by language models. That changes what matters:

1. **Generation errors.** A model writes code by predicting it. Every place
   where a language has two plausible readings is a place where the model
   (and the human reviewer) can be wrong. Ambiguity costs more than verbosity.
2. **Verification.** Models write plausible code quickly. The bottleneck is
   knowing whether it is *correct*. A language can make correctness claims
   explicit and checkable.
3. **Feedback loops.** Models fix code by reading error messages. Errors
   that name the cause, point at the location, and suggest a fix are part of
   the language's interface.
4. **Learnability from text.** A model may meet a new language only through
   its documentation. The language should be small enough that a complete
   specification fits in a context window.

Cogito is one answer to the question: *what does a language look like if it
is designed for that world, while remaining pleasant for humans?*

## Principles

1. **Explicit over implicit.** No truthiness, no implicit conversions, no
   hidden mutation.
2. **One obvious way.** When two forms are equivalent, keep one.
3. **Claims are code.** Contracts and tests are part of the language, not a
   library convention.
4. **Fail loudly, explain clearly.** Errors stop at the first sign of a bug
   and explain themselves.
5. **Small.** The whole language fits in one document.

## Decisions

### Capitalization is semantic

Types and constructors start with an uppercase letter; variables and
functions start with a lowercase letter. This is enforced.

*Why:* in a pattern, `Red` must mean "the constructor Red" and `red` must mean
"bind a new variable". Where the language does not enforce this, a misspelled
or unimported constant in a pattern can quietly become a catch-all binding
(in Rust, for example, only a lint warning catches it). With the rule, a
reader can classify every name at a glance, and the parser never needs type
information.

### Newlines end statements; brackets suspend that

Statements end at a newline or `;`. Inside `(...)` and `[...]`, newlines are
ignored; inside `{...}` they are significant again. A line beginning with
`.`, `|>`, `and` or `or` continues the previous line.

*Why:* semicolons are noise that models and people both forget. Python-style
indentation is fragile under copy-paste and generation (one wrong space
changes meaning). The bracket rule is simple and context-free: the lexer
alone decides, by looking at the innermost open bracket.
*Rejected:* JavaScript-style automatic semicolon insertion, whose rules are
famously hard to predict.

### No truthiness

`if`, `while`, `and`, `or` and `not` require `Bool`. `if xs { ... }` is an
error that suggests `if not xs.is_empty()`.

*Why:* whether `0`, `""`, `[]` or `None` count as false differs between
languages, so a model trained on all of them will guess wrong some of the
time. Being explicit costs a few characters and removes the question.

### Division means division

`/` always produces a Float (`7 / 2 == 3.5`). `//` is floor division and `%`
is floor modulo (the sign follows the divisor). Integer overflow is an error.
Division by zero is an error for Floats too.

*Why:* C-style truncating integer division and silent wraparound are among
the most common sources of subtle bugs, and they are invisible in review.
Producing `inf` or `NaN` from `1.0 / 0.0` usually just moves the bug further
away from its cause.

### Comparisons don't chain

`a < b < c` is a syntax error with a suggested rewrite.

*Why:* in Python it means `a < b and b < c`; in C it means `(a < b) < c`.
Either reading is plausible to someone (or something) that knows both.

### Value semantics everywhere

Lists, maps, records and strings behave like values: `let b = a` creates an
independent copy, and changing `a` later does not affect `b`. Closures
capture the values of variables at creation time.

*Why:* aliasing ("I changed this list, and some other part of the program
saw it change") is a large class of bugs that requires global reasoning to
avoid. With value semantics, a function's effect on the world is visible in
its signature and its return value.

*Cost:* naive copying would be slow. The interpreter uses reference counting
with copy-on-write: copies share storage until one of them is modified, and
a value with only one owner is modified in place. `xs.push!(x)` in a loop is
amortized O(1).

*Rejected:* reference semantics with optional immutability (as in most
mainstream languages). It is familiar, but it makes local reasoning
impossible without knowing who else holds a reference.

### Module globals versus local variables

Variables declared at the top level of a file are module globals: every
function and closure sees their current value, and functions may assign to
top-level `var`s. Variables declared inside functions and blocks are local,
and closures capture their values.

*Why:* this was the most-reported surprise during dogfooding, because a
top-level closure that reads a changing global behaves differently from the
same closure inside a function. The alternatives were both worse. Making
globals immutable from functions rules out caches and counters in small
scripts, which is exactly where top-level state is useful. Making top-level
closures snapshot globals would make named functions and anonymous functions
see different values of the same variable. The rule chosen is the one most
programmers already know from Python and JavaScript modules, and it is
stated in one sentence in the spec.

### Immutable by default; mutation is marked

`let` bindings never change. `var` bindings can be reassigned and mutated.
Functions that mutate their first argument end in `!` (`xs.sort!()` versus
`xs.sort()`), and the argument must be a `var`. User-defined `!` functions
receive their first argument "in-out": changes are written back to the
caller's variable.

*Why:* mutation is the operation that most needs to be visible. The `!`
convention exists informally in Ruby and Julia; Cogito enforces it, so
`xs.sort()` can never quietly mutate `xs`.

### Errors are values; bugs are not

Expected failures (parsing input, reading a file) return `Result` or
`Option`, and the `?` operator propagates them. Programmer errors (index out
of bounds, broken contracts, overflow) stop the program with a diagnostic.
There are no exceptions to catch, except via `catch(f)`, which exists for
tests.

*Why:* exceptions make control flow invisible. A model reading a function
cannot tell which calls may throw. With `Result` and `?`, every early exit is
marked in the source.

### Contracts are syntax

```
fn sqrt_floor(n: Int) -> Int
  requires n >= 0
  ensures result * result <= n and (result + 1) * (result + 1) > n
```

`requires` is checked on entry, `ensures` on exit (with `result` bound to
the return value). A failed `requires` is reported as the caller's bug; a
failed `ensures` as the function's.

*Why:* a contract is the most compact statement of what a function is for.
For AI-written code it is especially valuable: the model states its
intent, and the runtime holds it to that statement. Contracts also turn into
tests for free (see below), and they document assumptions that would
otherwise live in comments that drift out of date.

### Types carry invariants

```
type Graph = { n: Int, adj: List[List[Int]] }
  where n == adj.len()
  where adj.all(fn(es) => es.all(fn(e) => e >= 0 and e < n))
```

A record type's `where` clauses hold for every value of the type: they are
checked when a value is built and after every change to one of its fields.
A `!` function may break the invariant of its first argument while it runs
and must restore it before it returns.

*Why:* in the third dogfooding round, most of the noise from `verify` was
malformed generated input (a graph whose edges point past the end, a stack
whose `size` disagreed with its items), and every function on the type had
to repeat the same `requires`. Stating the condition once, on the type,
fixes both: functions may assume it, and the generator produces only values
that satisfy it. The `!` exception is the class-invariant rule of Eiffel,
for the same reason: a mutation that moves two fields (`lo` and `hi`)
passes through states that break the invariant. A failed check is not
undone (keeping a copy to restore would make every write through such a
value cost as much as the value), because breaking an invariant is a bug,
not a condition to handle. Only record types have invariants: per-variant
conditions on enums would need their own syntax.

### Tests and properties are syntax

`test "name" { ... }` and `property "name" (x: Int, xs: List[Str]) { ... }`
are top-level declarations. `cogito test` runs them. Properties receive
random inputs generated from their type annotations; failures are shrunk to
a minimal counterexample (`x = -11` rather than `x = -48213`).

*Why:* tests that live in another file, behind a framework, are tests that
don't get written. Property-based testing finds the edge cases that example
tests miss, and it is a natural fit for generated code: a model is often
better at stating a general law ("reverse twice is the identity") than at
enumerating the tricky inputs.

### `verify`: contracts as tests

`cogito verify` treats every function with contracts as a property:
generate inputs from the parameter types, discard those that fail
`requires`, call the function, and report any `ensures` violation or crash,
with shrinking. It also enforces a per-case step budget, so that a runaway
input is reported instead of hanging. Values of types with invariants are
generated to satisfy them: literal bounds in the clauses steer the
generator, a field that a clause defines (`n == adj.len()`) is computed,
retries shrink the size, and shrinking repairs the defined fields.

*Why:* this closes the loop between specification and implementation without
any extra test code. It is not a proof. Random testing can miss bugs, but it
finds a surprising number of them for almost no effort.

### Type annotations are checked at runtime

Annotations (`x: List[Int]`, `-> Result[Int, Str]`) are optional. When
present, they are checked whenever a value crosses a function boundary,
including the elements of containers, and Int values are converted where a
Float is expected. Generic parameters (`fn first[T](xs: List[T])`) are
accepted but not checked.

Annotations also stick: a `var` declared with a type, the first parameter
of a `!` function, and every field of a declared record type are re-checked
on each write (assignment, index or field assignment, mutating call), with
Int values converted to Float where needed.

*Why:* a full static type system (with inference, generics and variance)
would multiply the size of the language and its specification. Runtime
checking at boundaries catches most of what matters in practice, gives
precise messages ("element 3 of the list is Str"), and the annotations also
drive test generation. A static checker can be added later without changing
the meaning of correct programs.

*Cost, and how it is paid:* checking `xs: List[Int]` naively costs O(n) per
call, which turns a recursive function over a list into O(n²). Each list and
map therefore remembers the last annotation it was verified against. A write
that is checked against the declared type keeps that memo valid, so
`push!`, `+=`, `xs[i] = v` and `swap!` only check what they change. In
practice annotation checks are close to free.

### ...and before running, wherever the types are known

A gradual static checker runs before the program does. It infers types from
literals, annotations, and the signatures of functions, constructors and
common built-ins, and it rejects a program only when a value's type is known
and can never fit what is expected: `area("3", 4.0)` where `area` takes
Floats, `let total: Int = 1.5`, `if count { ... }`, `"a" + 1`, a field a
record type does not have. Anything it cannot work out is `Any`, which is
never an error.

*Why gradual, and why only "can never fit":* the runtime checks already give
every annotation a precise meaning, so the static checker must not change
which programs are correct; it only moves failures earlier. Requiring that a
type be *known* to be wrong means it never rejects a working program. A
`var` without an annotation may hold different types over time, so its type
is unknown, except when it holds a value of a declared record or enum type
and is never assigned as a whole: writes to its fields are checked against
the declared field types at runtime, so they can be checked early too.
Tests that exercise the runtime checks on purpose pass values through an
unannotated function (`fn opaque(x) => x`) to hide their types.

### Static checks that need no types

Before running anything, the resolver reports: undefined names (with
suggestions), assignments to immutable bindings, assignments to captured
variables, wrong argument counts and unknown named arguments for known
functions and constructors, non-exhaustive `match` over enums, misplaced
`break`/`return`/`?`, unknown types, duplicate definitions, and unused
variables.

*Why:* these are the mistakes models and people make most often, and none of
them need a type checker to detect.

### Method syntax is just function call syntax

`x.f(a)` means `f(x, a)` (unless `x` is a record with a field `f`). There
are no classes, no `self`, and no method declarations. `x |> f(a)` also
means `f(x, a)`.

*Why:* one mechanism instead of three (functions, methods, extension
methods). Any function, including built-ins, can be chained. Methods
"belong" to a type only in the sense that functions defined in a type's
module are found when calling methods on that type.

### Overloading by annotated type

Several top-level functions may share a name if their parameter annotations
differ; the first definition whose annotations accept the arguments is
called. User functions that share a built-in's name extend it.

*Why:* this gives polymorphism (`area(c: Circle)`, `area(r: Rect)`) without
interfaces or classes, using a mechanism the reader can resolve by reading
annotations.

### Strings

Strings are immutable UTF-8; `len` and indexing count characters, not
bytes. Interpolation (`"{x}"`) is the only formatting syntax, and supports
format specs (`{x:.2}`, `{s:>8}`). Raw strings `r"..."` have no escapes and
no interpolation, for JSON and similar text.

*Why:* one formatting mechanism instead of several (concatenation,
`format()`, `%`, f-strings). Character-based indexing is slower than byte
indexing but never cuts a character in half.

### Diagnostics are an interface

Every error has a stable code (`E0101`), a message, a source excerpt, and
usually a `help:` line with a concrete fix. `cogito explain CODE` prints a
longer explanation. Runtime errors carry a stack trace. Assertion failures
show the values of both sides of a comparison; contract failures show the
values of the variables involved.

*Why:* for a model fixing its own code, the error message is the only
feedback channel. A good message turns a guess into a correction.

### One layout: `cogito fmt`

`cogito fmt` rewrites files in the canonical layout: two-space indentation
(one level per open bracket, plus one for continuation lines such as
`|> map(...)`, `requires ...` and `| Variant(...)`), one space around binary
operators and after commas and colons, none inside brackets, and at most one
blank line in a row. It changes only whitespace: line breaks stay where the
author put them, comments are kept (trailing comments keep their column, so
aligned comments stay aligned), and the result must lex to exactly the same
tokens as the input, or the file is left alone. CI runs `cogito fmt --check`.

*Why:* "one obvious way" should extend to layout, so that diffs show only
changes in meaning, and so that code written by models and by people looks
the same. Keeping the author's line breaks avoids the hardest part of a
pretty-printer (deciding where to break long lines) and keeps the formatter
small enough to trust.

### The spec fits in a context window

[docs/llm-spec.md](docs/llm-spec.md) is the complete language: syntax,
semantics, and the standard library, in about four pages. `cogito spec`
prints it.

*Why:* a model can learn a new language from its documentation only if the
documentation is short enough to read in full, and precise enough to be
unambiguous. This constraint also keeps the language small, which helps
humans just as much.

## What was deliberately left out

- **Classes and inheritance.** Records, enums, functions and overloading
  cover the same ground with fewer concepts.
- **Null.** `Option` makes absence explicit.
- **Exceptions.** `Result` and `?` make failure explicit.
- **Operator overloading and user-defined operators.** They make the meaning
  of `+` depend on context.
- **Implicit conversions** (other than Int to Float).
- **Macros.** Code generators are already writing the code.
- **Shared mutable state between closures.** Closures capture snapshots.

## How it was built

The implementation is a single dependency-free Rust crate:

- a hand-written lexer with significant newlines and nested string
  interpolation;
- a recursive-descent parser with precedence climbing;
- a resolver that turns names into frame slots, captured-value indexes and
  global slots, and performs the static checks above;
- a tree-walking interpreter whose values use `Rc` with copy-on-write;
- a standard library of about 170 functions;
- a property-testing engine (generation from types, shrinking) shared by
  `property` blocks and `cogito verify`.

## How it was tested: dogfooding by AI agents

The language was tested in three ways:

1. **Its own test suite, written in Cogito** (`tests/lang`), plus one program
   per diagnostic code (`tests/errors`) and example programs with exact
   expected output (`examples`).
2. **Fresh AI agents that knew nothing but the compact specification.** Each
   was asked to write about ten realistic programs in an area (algorithms,
   text processing, types and contracts) and to report every bug, gap in the
   spec, and confusing message, with a minimal reproduction.
3. **Adversarial AI agents** asked to make the interpreter crash, hang or
   give wrong answers.

The first round produced about a hundred findings. Some were bugs only real
programs would hit: type ids clashed when two modules declared types, and
recursive functions over annotated lists were quadratic. Others were gaps in
the design: no way to destructure a tuple parameter, no `old()` in
postconditions, `?` silently mixing Option and Result, assignments not
allowed as match-arm bodies. The adversarial agents found a dozen ways to
crash the process, mostly integer edge cases (`gcd(min_int, -1)`,
`0..=max_int`) and unbounded sizes (`"{x:.65536}"`, a million nested
parentheses). All of these were fixed, and the specification was rewritten
to answer every question the agents had to guess at.

A second round, against the improved implementation and specification,
measured how often a fresh agent's program worked on the first run:

| Area | Round 1 | Round 2 |
|---|---|---|
| Algorithms and data structures | 7 of 11 | 7 of 12 |
| Text processing, formatting, JSON, I/O | 2 of 10 | 7 of 10 |
| Types, contracts and tests | 6 of 10 | 6 of 10 |

In round 2, eleven of the twelve programs that did not work on the first
run failed because of the agent's own mistake (a wrong expected value in a
test, syntax from another language such as `if let`), and the error message
pointed at the problem; one also hit a real bug (a `where` clause on a
destructured parameter).
`verify` found real contract violations in the agents' code, including
integer overflows and missing preconditions.

Round 2 still found problems, which were fixed:

- **Performance cliffs** that only realistic programs reach: mutating a
  collection with a declared type re-checked the whole collection on every
  change (a typed binary heap took 15 s instead of 0.4 s), and indexing a
  string containing one non-ASCII character took linear time per access.
  Both are now constant time.
- **Silent wrong answers** at the edges of the value-semantics model: a write
  to a global variable during a `!` call on that same variable was lost; a
  failed `push!` on a typed list left the wrong element behind; map keys and
  destructured `var`s escaped their declared types. All are now errors, or
  are undone.
- **Mistakes the checker could catch**: `xs.sort()` whose result is thrown
  away, `?` inside an anonymous function (which returns from that function
  only), side effects in contracts, and patterns of the wrong type. `check`
  now reports each, and `cogito FILE` shows the likely-bug warnings too.
- **Generator blind spots**: properties filtered on `xs.len() >= 3` always
  gave up, `where` clauses could not see names bound by parameter patterns,
  generated strings rarely contained newlines, and shrinking large integers
  took a thousand steps. Generation now reads length and Float bounds from
  `where`/`requires`, and integer shrinking is logarithmic.
- **Resource exhaustion** found by adversarial fuzzing (about 50,000
  generated programs, with no crashes): comparing or hashing values built by
  repeated doubling took exponential time, and top-level code under
  `cogito test` had no step budget.

Some findings changed the language's direction rather than its bugs. When a
test of `verify` produced no counterexample for an "obviously correct"
`average` function, the input generator was changed to favour duplicates and
values like `0.1`, after which `verify` found the floating-point bug
(`[0.1, 0.1, 0.1]`) on its own. That bug is now the demonstration in the
README.

A third round added a new area, applications split across modules (a bank
ledger, a vending machine, a to-do manager with undo, a spreadsheet), in
place of types and contracts:

| Area | Round 1 | Round 2 | Round 3 |
|---|---|---|---|
| Algorithms and data structures | 7 of 11 | 7 of 12 | 7 of 10 |
| Text processing, formatting, JSON, I/O | 2 of 10 | 7 of 10 | 4 of 10 |
| Types, contracts and tests | 6 of 10 | 6 of 10 | |
| Applications with modules | | | 7 of 10 |

The numbers did not keep rising, and the failures say why. Of the twelve
programs that failed on the first run, nine were the agents' own mistakes
(a `{` in a string that started an interpolation, a function from another
language, a wrong expected value, an algorithm bug), each with an error
pointing at it. Three hit real bugs, all now fixed: zero-padded hex printed
`" a"` instead of `"0a"`; a module's `push!` with more parameters than the
built-in `push!` was rejected by the arity check; and a local function named
like a built-in (`let get = fn(k) => m.get(k)`) called itself instead of
the built-in. The adversarial agent found
three ways to make `check` accept a match that then failed (list patterns
whose prefix and suffix overlap, record patterns without `..`, and a search
that gave up silently), and fourteen kinds of correct program that the type
checker rejected; the checker now treats as unknown everything the runtime
does not enforce (inferred type arguments, the result type in a
`fn(A) -> B` annotation, fields beyond a structural annotation).

Two changes came from watching `verify` on application code. Generated
inputs rarely fitted together (a key was almost never in the generated
map), so 5 of 14 contracts on state-machine functions went unchecked; the
generator now reuses values it has already produced across parameters,
which checked one such function 200 times where it had found 13 valid
inputs in 4,000. And four of five `verify` failures were overflows from
inputs such as `max_int`, which is right but tedious, so those reports now
say how to rule such inputs out.

## Future directions

- Deeper static checking: arguments of built-ins, the parameter types of
  passed-in functions, and flow-sensitive types for reassigned `var`s.
- A bytecode compiler for speed.
- Richer contract-guided generation: today simple numeric bounds in `requires`
  steer the generator; more general constraints could be solved instead of
  filtered.
- A package format and a larger standard library.
