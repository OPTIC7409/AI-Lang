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
input is reported instead of hanging.

*Why:* this closes the loop between specification and implementation without
any extra test code. It is not a proof. Random testing can miss bugs, but it
finds a surprising number of them for almost no effort.

### Type annotations are checked at runtime

Annotations (`x: List[Int]`, `-> Result[Int, Str]`) are optional. When
present, they are checked whenever a value crosses a function boundary,
including the elements of containers, and Int values are converted where a
Float is expected. Generic parameters (`fn first[T](xs: List[T])`) are
accepted but not checked.

*Why:* a full static type system (with inference, generics and variance)
would multiply the size of the language and its specification. Runtime
checking at boundaries catches most of what matters in practice, gives
precise messages ("element 3 of the list is Str"), and the annotations also
drive test generation. A static checker can be added later without changing
the meaning of correct programs.

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

The language was then tested by writing its own test suite in Cogito, and by
giving fresh AI agents nothing but the compact specification and asking them
to write programs and try to break the interpreter. Their bug reports were
used to fix the implementation and sharpen the specification.

## Future directions

- A static type checker that uses the existing annotations.
- A bytecode compiler for speed.
- A formatter (`cogito fmt`) to make the "one obvious way" principle
  extend to layout.
- Contract-guided generation: using `requires` clauses to generate valid
  inputs directly instead of filtering random ones.
- A package format and a larger standard library.
