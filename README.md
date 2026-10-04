# Cogito

**A programming language designed and implemented by an AI.**

Cogito (*cogito, ergo sum*: "I think, therefore I am") is a small,
expression-oriented language whose design starts from one question:

> What would a language look like if it were designed so that both humans
> *and* AI models can write code in it correctly, read it unambiguously,
> and check that it does what it claims?

Everything in this repository was designed and written by an AI (Claude, by
Anthropic), from a single human prompt: the grammar, the interpreter, the
standard library, the error messages, the test suite, the examples, and these
docs. The reasoning behind each decision is recorded in [DESIGN.md](DESIGN.md).

```cogito
type Account = { owner: Str, balance: Int }

fn withdraw(acct: Account, amount: Int) -> Result[Account, Str]
  ensures match result {
    Ok(a) => a.balance == acct.balance - amount and a.balance >= 0
    Err(_) => true
  }
{
  if amount <= 0 { return Err("amount must be positive") }
  if amount > acct.balance { return Err("insufficient funds") }
  Ok({ ..acct, balance: acct.balance - amount })
}

test "withdrawing too much fails" {
  let a = Account(owner: "Ada", balance: 10)
  assert withdraw(a, 50) == Err("insufficient funds")
}

property "balances never go negative" (balance: Int, amount: Int) where balance >= 0 {
  match withdraw(Account(owner: "x", balance: balance), amount) {
    Ok(a) => assert a.balance >= 0
    Err(_) => assert true
  }
}
```

```console
$ cogito test bank.cog
bank.cog
  ✓ withdrawing too much fails
  ✓ balances never go negative (100 cases, 77 discarded)

$ cogito verify bank.cog
bank.cog
  ✓ withdraw  200 cases, contracts held
```

## Why Cogito?

AI models now write a large share of new code. Cogito is an experiment in
what a language optimized for that world looks like. Its core ideas:

- **Specs live next to code, and are executable.** Functions declare
  `requires`/`ensures` contracts. `test` and `property` blocks sit beside the
  code they test. `cogito verify` generates random inputs from type
  annotations, checks every contract, and *shrinks* failures to a minimal
  counterexample. An AI (or a person) that writes a function also writes down
  what it promises, and the toolchain checks the promise.
- **One obvious way, no ambiguity.** Capitalization separates types and
  constructors (`Point`, `Some`) from variables (`point`). Comparisons don't
  chain. There is no truthiness: conditions must be `Bool`. There are no
  implicit conversions (except Int to Float). `/` always means real division.
  Code generators of all kinds make fewer mistakes when the grammar never has
  to guess.
- **No spooky action at a distance.** Every value has value semantics.
  `let b = a` makes an independent copy (copy-on-write makes this cheap).
  Bindings are immutable unless declared with `var`. Functions that mutate
  their argument end in `!` (`xs.push!(4)`), so mutation is visible at every
  call site. Closures capture values, not variables.
- **Errors are values; bugs are loud.** Expected failures are `Result` and
  `Option`, with `?` for propagation. Bugs (overflow, division by zero, a
  broken contract, an out-of-bounds index) stop the program with a precise
  diagnostic. Integer overflow is never silent.
- **Errors explain themselves.** Every diagnostic has a code, a source
  excerpt, and usually a fix (`did you mean print?`). Contract violations say
  who is to blame: a failed `requires` is a bug in the caller, a failed
  `ensures` is a bug in the function. `cogito explain E0101` explains any error.
- **Small enough to fit in a context window.** The whole language is
  described in [one compact spec](docs/llm-spec.md) (`cogito spec` prints it),
  so another model can learn Cogito from the spec alone.

## A quick tour

```cogito
# Algebraic data types and exhaustive pattern matching
type Shape =
  | Circle(radius: Float)
  | Rect(w: Float, h: Float)

fn area(s: Shape) -> Float => match s {
  Circle(r) => pi * r * r
  Rect(w, h) => w * h
}

# Method syntax works with any function: `x.f(y)` is `f(x, y)`
let shapes = [Circle(1.0), Rect(2.0, 3.5)]
print(shapes.map(area).sum())

# Pipelines and comprehensions
let evens = (1..=20) |> filter(fn(n) => n % 2 == 0) |> map(fn(n) => n * n)
let pairs = [(x, y) for x in 1..4 for y in 1..4 if x < y]

# Value semantics: `ys` is unaffected by changes to `xs`
var xs = [3, 1, 2]
let ys = xs
xs.sort!()            # `!` marks mutation; only `var`s can be mutated
print(xs, ys)         # [1, 2, 3] [3, 1, 2]

# Errors as values, with `?`
fn parse_port(s: Str) -> Result[Int, Str] {
  let n = parse_int(s).ok_or("not a number: {s}")?
  if n < 1 or n > 65535 { return Err("out of range: {n}") }
  Ok(n)
}

# String interpolation with format specs
print("{"name":<10}|{3.14159:>8.2}|{255:x}")
```

Errors look like this:

```console
$ cogito divide.cog
error[E0301]: precondition of `divide` violated: `b != 0`
  --> divide.cog:8:9
   |
 8 |   print(divide(10, x))
   |         ^^^^^^^^^^^^^ this call breaks a precondition of `divide`
  = note: `divide` requires `b != 0` (at divide.cog:2:12)
  = note: where b = 0
  = help: this is a bug in the caller: make sure the arguments satisfy the precondition
```

And `cogito verify` turns contracts into tests. It generates inputs from the
type annotations, checks every `ensures`, and shrinks failures to a minimal
counterexample. Here it finds a floating-point bug in an "obviously correct"
function:

```cogito
fn average(xs: List[Float]) -> Float
  requires not xs.is_empty()
  ensures xs.min().unwrap() <= result and result <= xs.max().unwrap()
{
  xs.sum() / xs.len()
}
```

```console
$ cogito verify stats.cog
stats.cog
  ✗ average  postcondition violated
      counterexample (after 64 cases, shrunk 2 times):
        xs = [0.1, 0.1, 0.1]
      error[E0302]: postcondition of `average` violated: `xs.min().unwrap() <= result and result <= xs.max().unwrap()`
        = note: where xs = [0.1, 0.1, 0.1], result = 0.10000000000000002
```

## Getting started

Cogito is written in dependency-free Rust. To build it:

```console
$ git clone https://github.com/OPTIC7409/AI-Lang
$ cd AI-Lang
$ cargo build --release
$ ./target/release/cogito examples/hello.cog
Hello, world!
```

| Command | What it does |
|---|---|
| `cogito` | interactive REPL |
| `cogito FILE.cog [ARGS]` | run a program (top-level code, then `main()` if defined) |
| `cogito test [PATHS]` | run `test` and `property` blocks |
| `cogito verify FILE.cog` | check contracts against random inputs |
| `cogito check [PATHS]` | report errors and warnings without running |
| `cogito fmt [--check] [PATHS]` | format files in the one canonical layout |
| `cogito lsp` | language server for editors (errors as you type, format, hover, go to definition) |
| `cogito eval "CODE"` | run a snippet |
| `cogito explain E0101` | explain an error code |
| `cogito doc [NAME]` | built-in function reference |
| `cogito spec` | print the compact language specification |

Editor support: `cogito lsp` is a language server (errors and warnings as
you type, formatting, hover documentation, outline, go to definition) that
works with any editor that speaks LSP. [editors/vscode](editors/vscode) is a
VS Code extension that uses it, with setup notes for Neovim and Helix.

### In the browser

The interpreter also compiles to WebAssembly. [web/](web) holds a playground
page with an editor, example programs, and Run / Test / Verify / Check
buttons; the interpreter runs in a Web Worker, so long programs can be
stopped. Build it with:

```console
$ rustup target add wasm32-unknown-unknown
$ python3 web/build.py            # writes web/dist/index.html and cogito.wasm
$ python3 web/build.py --inline   # or one self-contained page
```

## Documentation

- [docs/tutorial.md](docs/tutorial.md): a guided introduction.
- [docs/llm-spec.md](docs/llm-spec.md): the complete, compact language specification.
- [DESIGN.md](DESIGN.md): why the language looks the way it does.
- [examples/](examples): example programs, from FizzBuzz to a calculator
  interpreter and a contract-checked bank.

## Project layout

```
src/
  lexer.rs       tokens; significant newlines; string interpolation
  parser.rs      recursive-descent parser producing the AST
  resolver.rs    static checks: names, mutability, exhaustiveness, arity, types
  interp.rs      tree-walking interpreter with value semantics and contracts
  builtins.rs    the standard library (~170 functions)
  proptest.rs    random generation and shrinking from type annotations
  testing.rs     `cogito test` and `cogito verify`
  diagnostic.rs  error rendering and the error-code catalog
  repl.rs        the interactive loop
  typecheck.rs   the gradual static type checker
  format.rs      `cogito fmt`
  lsp.rs         `cogito lsp`, the language server (with json.rs)
  platform.rs    clock and sleep, for native builds and WebAssembly
web/             the browser playground (a WebAssembly build of the interpreter)
tests/
  lang/          the language test suite, written in Cogito
  errors/        one program per diagnostic code
  integration.rs end-to-end tests of the binary
examples/        example programs with expected output
```

Run everything with `cargo test`.

## Status

Cogito is a young, experimental language (version 0.1). It is a
tree-walking interpreter: fast enough for scripts, puzzles, teaching and
experiments, not for performance-critical work. Typing is gradual: a static
checker rejects type errors it can prove before the program runs, and every
annotation is also checked at runtime. There is no
concurrency, no package manager, and the standard library is small.
Contributions and experiments are welcome.

## License

MIT. See [LICENSE](LICENSE).
