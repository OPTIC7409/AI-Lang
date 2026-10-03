# Cogito language specification (compact)

Cogito is a small, expression-oriented, dynamically typed language with
optional type annotations that are checked at runtime, value semantics,
contracts, and built-in tests. This document is complete enough to write
correct programs; it is written to fit in a language model's context window.

Run: `cogito FILE.cog` · Test: `cogito test FILE.cog` · Check contracts:
`cogito verify FILE.cog` · Static check: `cogito check FILE.cog` · Snippet:
`cogito eval "CODE"` (prints the last expression's value) · REPL: `cogito`.

## Lexical rules

- Comments start with `#` and run to the end of the line.
- Statements end at a newline (or `;`). Newlines inside `(...)` and `[...]`
  are ignored. A line starting with `.`, `|>`, `and` or `or` continues the
  previous line. A binary operator at the end of a line also continues it.
- **Names**: variables and functions start with a lowercase letter or `_`
  (snake_case by convention); types and constructors start with an uppercase
  letter. This is enforced. A function whose name ends in `!` mutates its
  first argument (see Mutation).
- Keywords (not usable as names or field names): `let var fn return if else
  while for in loop break continue match type test property requires ensures
  and or not true false import as assert where`.
- Numbers: `42`, `1_000`, `0xff`, `0b1010`, `0o17`, `3.14`, `1e-9`. Int is
  64-bit signed; overflow is an error (never wraps).
- Strings: `"..."` with escapes `\n \t \r \\ \" \0 \{ \} \u{1F600}`.
  `{` starts an interpolation: `"x = {x + 1}"`. Write `\{` for a literal `{`
  (`{{` is an error, not an escape); a lone `}` is literal. A format spec may
  follow `:` — `{x:.2}` (2 decimals), `{s:>8}` `{s:<8}` `{s:^8}` (align in
  width 8), `{n:05}` (zero pad), `{n:+}`, `{n:,}` (thousands separators),
  `{n:x}` `{n:b}` `{n:o}` (hex/binary/octal), `{f:e}` (`1.5e+03`), `{f:.1%}`,
  `{s:*>6}` (fill char). Width ≤ 1000, precision ≤ 100; for a computed width
  use `pad_left`/`pad_right`.
- `"""..."""` strings span lines: a newline right after the opening quotes is
  dropped, a final line holding only whitespace is dropped (so there is no
  trailing newline), and the common indentation of the lines is removed.
- Raw strings `r"..."` and `r"""..."""` have no escapes and no interpolation
  (use them for JSON, regexes, Windows paths). Raw triple strings are dedented
  the same way.

## Values and types

| Type | Literal / constructor | Notes |
|---|---|---|
| `Int` | `42` | `/` always gives Float; `//` floor division; `%` floor modulo |
| `Float` | `3.0` | Int is accepted (converted) where Float is annotated |
| `Bool` | `true` `false` | no truthiness: conditions must be Bool |
| `Str` | `"hi"` | indexing/len count characters (O(1) for ASCII); immutable |
| `Unit` | `()` | value of statements, `if` without `else`, etc. |
| `List[T]` | `[1, 2, 3]`, `[..xs, 4]` | `xs[0]`, `xs[-1]`, `xs[1..3]`, `xs[2..]`, `xs[..2]` |
| `Map[K, V]` | `["a": 1, "b": 2]`, empty `[:]` | insertion-ordered; `m[k]`, `m.get(k)` |
| tuples | `(1, "a")`, `(x,)` | `t.0`, `t[1]` |
| records | `{ x: 1, y: 2 }`, `{ ..r, y: 5 }` | see Records below |
| `Range` | `0..10`, `0..=10`, `0..` | Int only; end exclusive (`..=` inclusive) |
| `Option[T]` | `Some(x)`, `None` | built-in enum |
| `Result[T, E]` | `Ok(x)`, `Err(e)` | built-in enum |
| `Ordering` | `Less`, `Equal`, `Greater` | returned by `compare(a, b)` |
| functions | `fn(x) => x + 1` | type: `fn(Int) -> Int` (or just `Fn`) |
| `Any` | | annotation that accepts anything |

Indexing out of range is an error (use `get` for an Option); slices clamp
silently (`[1, 2, 3][1..10] == [2, 3]`).

**Value semantics**: every value behaves like an independent copy.
`let b = a` then changing `a` never changes `b`. (Implemented with
copy-on-write, so copies are cheap.)

**Equality** `==` is structural (deep). `1 == 1.0` is true (Int/Float
comparisons are exact). Comparison `< <= > >=` works on numbers, strings,
lists/tuples (lexicographic), and values of the same enum/record type.
Comparing different kinds is an error.

**Records**: anonymous records (`{ x: 1 }`) are structural: field order does
not matter for `==`. A declared record type (`type P = { x: Int }`) is
nominal: `P(x: 1) != { x: 1 }`. Where an annotation expects `P`, an
anonymous record with exactly P's fields is converted to a `P`
(`fn mk() -> P => { x: 1 }` works).

## Declarations

```
let x = 1                 # immutable binding
var count = 0             # mutable binding
let (a, b) = (1, 2)       # destructuring (any pattern)
var [first, ..rest] = xs  # `var` patterns make every bound name mutable
var n: Int = 5            # annotations are checked now and on every later write

fn add(a: Int, b: Int) -> Int {    # block body: the last expression is the result
  a + b
}
fn square(x: Int) -> Int => x * x  # expression body
fn greet(name: Str, greeting: Str = "Hello") -> Str => "{greeting}, {name}!"
greet("Ada")  greet("Ada", "Hi")  greet(name: "Ada", greeting: "Yo")   # named args
fn dist((x1, y1): (Float, Float), (x2, y2): (Float, Float)) -> Float => hypot(x2 - x1, y2 - y1)
pairs.map(fn((k, v)) => "{k}={v}")  # parameters may be destructuring patterns

fn first[T](xs: List[T]) -> Option[T] => xs.get(0)   # generic parameters (unchecked)

type Point = { x: Float, y: Float }          # record type
type Shape =                                  # enum (sum type)
  | Circle(center: Point, radius: Float)      # named fields: c.radius
  | Rect(Point, Point)                        # positional fields: r.0, r.1
  | Empty                                     # no fields
type Tree[T] = Leaf | Node(Tree[T], T, Tree[T])
type Grid = List[List[Int]]                   # alias

let p = Point(x: 1.0, y: 2.0)   # or Point(1.0, 2.0); fields are type-checked
let c = Circle(center: p, radius: 3.0)
```

Variant names must be unique within a module. Top-level functions and types
may be used before their definition; `let`/`var` may not. Functions may
have at most 64 parameters. Functions with the same name but different
parameter type annotations are **overloads**; the first whose annotations
accept the arguments is called. A user function named like a built-in (e.g.
`len`) adds an overload and falls back to the built-in. If a top-level
`fn main()` exists, it runs after the top-level statements; if `main` returns
`Err(e)`, the program prints the error and exits with status 1.

## Scope and closures

- Variables declared at the top level of a file are **module globals**:
  every function and closure sees their *current* value, and functions may
  assign to top-level `var`s.
- Variables declared inside functions and blocks are **local**. Closures
  capture the *values* of the local variables they use, at the moment the
  closure is created, and cannot assign to them (error E0110).
- A `let`-bound lambda can call itself (`let fact = fn(n) => ... fact(n - 1)`);
  a local `fn` can call itself; local functions cannot call each other before
  they are defined (use top-level functions for mutual recursion).
- Recursion is limited to 100,000 nested calls (`cogito --max-depth N` to change).

## Expressions and control flow

Everything is an expression. Blocks `{ ... }` evaluate to their last expression.

```
let size = if n > 10 { "big" } else if n > 5 { "medium" } else { "small" }
while cond { ... }
for x in xs { ... }             # lists, ranges, strings (chars), maps ((k, v) tuples)
for (i, x) in xs.enumerate() { ... }
let found = loop { if done() { break value } }   # only `loop` can break with a value
break  continue  return value
[x * x for x in 1..=10 if x % 2 == 0]           # comprehension (several `for`/`if` allowed)
```

Operators by precedence (loosest first): `or`; `and`; `not`;
`== != < <= > >= in` (`not in`) — **cannot be chained** (`a < b < c` is an
error); `|>`; `..` `..=`; `+ -`; `* / // %`; unary `-`; `**` (right-assoc).
`+` concatenates strings and lists; `str * n` and `list * n` repeat.
No implicit conversions: `"a" + 1` is an error; use `"a{1}"` or `str(1)`.
There are no bitwise operators; use `bit_and`, `bit_or`, `bit_xor`, `shl`, `shr`.

**Method syntax**: `x.f(a, b)` calls `f(x, a, b)` — any function, including
built-ins (`xs.len()`, `"a,b".split(",")`). If `x` is a record with a field
`f`, the field is called instead. A variable named like a function (say
`let lines = ...`) does not hide the function from method syntax. For values
whose type comes from an imported module, that module's functions are also
found (including `!` functions).
**Pipelines**: `x |> f(a)` is `f(x, a)`; `x |> f` is `f(x)`.

## Pattern matching

```
match value {
  0 => "zero"
  1 | 2 => "one or two"                    # or-patterns
  n if n < 0 => "negative"                 # guard (tried for each alternative of an or-pattern)
  3..=9 => "small"                         # range (also "a"..="z")
  "hello" => "greeting"
  [] => "empty list"
  [x] => "one element"
  [first, ..rest] => "has a head"          # also [..init, last], [a, .., z]
  (a, b) => "pair"
  { name, age: years, .. } => "record"     # `..` allows extra fields
  Circle(center: c, radius: r) => "named"  # or positional: Circle(c, r)
  geo.Circle(r) => "from a module"         # qualified constructor
  Rect(..) => "ignore fields"
  Some(x) => x
  n @ 10..=19 => "bind and test: {n}"
  _ => total += 1                          # an arm body may be an assignment
}
```

A `match` over enums, Bools and tuples of them must be exhaustive (or have
`_`); this is checked before the program runs, including nested patterns,
and the error names a missing case. Matches on numbers, strings and lists are
checked at runtime: no matching arm is an error.

## Mutation

Only `var` bindings change. Assignment forms: `x = v`, `x += v` (also `-= *= /= %=`),
`xs[i] = v`, `m[k] = v` (inserts), `p.field = v`, `t[0] = v`, nested
`grid[r][c] = v`. Mutating functions end in `!` and require a `var` (or
field/index of one) as first argument: `xs.push!(4)`, `xs.sort!()`,
`m.insert!(k, v)`. Non-mutating twins return new values: `ys = xs.push(4)`,
`xs.sort()`. User-defined `fn grow!(xs: List[Int], n: Int) { xs.push!(n) }`
mutates the caller's variable through its first parameter: `v.grow!(3)` or
`grow!(v, 3)`. Declared types (of `var`s, `!` parameters and record fields)
are enforced on every write. While a `!` call runs, the variable being
changed cannot be read; if the call fails, the variable keeps its old value
(built-ins) or the changes made so far (user functions).

## Errors as values

Functions that may fail return `Result` (or `Option`). The postfix `?`
operator unwraps `Ok(x)`/`Some(x)` or returns the `Err`/`None` from the
current function. Use `?` on a Result only in a function returning a
Result, and on an Option only in one returning an Option; convert with
`opt.ok_or(err)?` and `res.ok()?` (mixing them is error E0117).

```
fn parse_age(s: Str) -> Result[Int, Str] {
  let n = parse_int(s).ok_or("not a number: {s}")?
  if n < 0 { return Err("negative") }
  Ok(n)
}
```

Bugs (index out of bounds, division by zero, overflow, failed contracts,
`panic(msg)`) stop the program with a diagnostic and stack trace.
`catch(fn() => expr)` converts such an error into `Err(message)` (for tests).

## Contracts

```
fn withdraw(balance: Int, amount: Int) -> Int
  requires amount > 0                  # checked on entry: a violation is the caller's bug
  requires amount <= balance
  ensures result == balance - amount   # checked on exit; `result` is the return value
  ensures result >= 0
{
  balance - amount
}
fn push_twice!(xs: List[Int], x: Int)
  ensures xs.len() == old(xs.len()) + 2  # old(e): e evaluated on entry
{ xs.push!(x); xs.push!(x) }
```

`cogito verify FILE` treats each function with contracts as a property: it
generates arguments from the parameter types (including extreme Ints such as
`max_int`), discards those that fail `requires`, and reports shrunk
counterexamples that break `ensures`, crash, or exceed the step budget.

## Tests

```
test "addition" {
  assert add(2, 2) == 4                # failures show both sides
  assert xs.len() > 0, "message"
}
property "reverse is an involution" (xs: List[Int]) {   # inputs generated from types
  assert xs.reverse().reverse() == xs
}
property "division" (a: Int, b: Int) where b != 0 {      # `where` filters inputs
  assert (a // b) * b + a % b == a
}
```

- A test fails if an assertion fails, an error occurs, it returns `Err`, or a
  `?` returns early from it. Each test and each generated case has a step
  budget (10 million calls plus loop iterations; `--budget N`).
- `cogito test` runs a file's top-level statements first (with their output
  hidden) but not `main`; `print` inside `test` blocks is shown, inside
  `property` blocks hidden. `exit()` is an error during tests.
- Generated inputs: Int mostly small (|n| up to ~40, sometimes up to 100000);
  Float finite (no NaN/inf), including values like 0.1; Str up to ~40 chars,
  some non-ASCII; List/Map up to ~40 elements, often with duplicates; records,
  tuples, Option, Result and user enums (recursive ones stay finite); generic
  `T` is Int. Simple bounds in `where`/`requires` (comparisons of a parameter
  with a literal joined by `and`, or `n in 1..=10`) steer generation.
- Failing inputs are shrunk to a minimal counterexample. A property where too
  few inputs pass `where` "gives up", which counts as a failure.

## Modules

```
import "lib/geometry.cog"            # binds `geometry`; path relative to this file
import "lib/geometry.cog" as geo
geo.area(c)   geo.Point(1.0, 2.0)    # qualified access
fn f(p: geo.Point) -> Float => ...   # qualified types
```

Importing runs the module's top-level statements once.

## Built-in functions (all callable as methods: `xs.map(f)`)

Built-ins accept named arguments using the names shown (`to_json(x, indent: 2)`).

- **I/O**: `print(..)` `write(..)` (no newline) `eprint(..)` `input(prompt)`
  `read_line() -> Option[Str]` `read_stdin() -> Str` `read_file(path) -> Result[Str, Str]`
  `write_file(path, text) -> Result[Unit, Str]` `append_file(path, text) -> Result[Unit, Str]`
  `file_exists(path) -> Bool` `list_dir(path) -> Result[List[Str], Str]`
  `args() -> List[Str]` `env(name) -> Option[Str]` `exit(code)` (0–255)
  `time()` `clock()` `sleep(seconds)`
- **Core**: `type_of(x)` `str(x)` `repr(x)` `int(x)` `float(x)`
  `parse_int(s) -> Option` `parse_float(s) -> Option` `ord(c)` `chr(n)`
  `panic(msg)` `todo()` `dbg(x)` (prints and returns x) `catch(f)` `compare(a, b)`
  `min(xs) -> Option` / `min(a, b, ...)`, `max` likewise
- **Math**: `abs sqrt pow exp ln log(x, base) log2 log10 sin cos tan asin acos
  atan atan2 hypot floor ceil trunc` (floor/ceil/trunc/round return Int),
  `round(x, digits) -> Float`, `sign clamp(x, lo, hi) gcd lcm is_nan fixed(x, digits) -> Str`,
  `bit_and bit_or bit_xor bit_not shl shr`, constants `pi tau e inf max_int min_int`,
  `seed(n) random() random_int(lo, hi) shuffle(xs) choice(xs) -> Option`
- **Collections** (lists; most also accept ranges, strings, tuples, maps):
  `len is_empty range(end) range(start, end, step) first last get(i) -> Option
  get_or(k, default) push insert(i, x) remove(i) set(i, x) map filter
  reduce(f) fold(init, f) sum product min_by(key) max_by(key) sort sort_by(key)
  sort_with(cmp) reverse contains index_of -> Option find -> Option
  find_index -> Option any(pred) all(pred) count(pred_or_value) take drop
  take_while drop_while slice(a, b) zip enumerate flat_map flatten join(sep)
  unique group_by(key) -> Map tally -> Map[T, Int] partition(pred) -> (List, List)
  chunks(n) windows(n) repeat(x, n) each(f) to_list to_map(pairs)`
- **Mutating**: `push! pop! -> Option insert! remove! extend! clear! swap!(i, j)
  sort! sort_by! reverse!`
- **Maps**: `keys values entries -> List[(K, V)] has(k) merge(other)
  map_values(f) get(k) get_or(k, d) insert(k, v) remove(k)`; `filter` and
  `each` on maps pass `(k, v)` to a two-parameter function.
- **Strings**: `split(sep, limit)` (no sep: whitespace) `split_once(sep) -> Option[(Str, Str)]`
  `lines words chars trim trim_start trim_end upper lower capitalize` (uppercases
  only the first character) `starts_with ends_with strip_prefix(p) -> Option
  strip_suffix(s) -> Option replace(a, b) pad_left(width, fill) pad_right(width, fill)
  is_digit is_alpha is_alnum is_space is_upper is_lower reverse repeat
  count(sub) index_of(sub)`, slicing `s[1..3]`.
- **Option/Result**: `unwrap expect(msg) unwrap_or(d) unwrap_or_else(f)
  is_some is_none is_ok is_err map(f) and_then(f) map_err(f) ok_or(e) ok err
  unwrap_err collect_ok(list of Results) -> Result[List] collect_some(list of Options) -> Option[List]`
- **JSON**: `to_json(x, indent = 0)`, `parse_json(s) -> Result` (objects become
  Maps with Str keys, `null` becomes `None`).

## What `cogito check` checks

Without running anything: syntax; undefined names (with suggestions);
assignments to immutable bindings and captured variables; argument counts
and argument names for known functions and constructors; exhaustiveness of
matches over enums; misplaced `break`/`continue`/`return`/`?`; `?` mixing
Option and Result; unknown types; duplicate definitions; unused variables
(warnings). It does not check the types of values; annotations are checked
when the program runs.

## Style notes for writing Cogito

- Prefer `let`; use `var` only for values that change.
- Prefer expressions (`if`/`match` returning values) and pipelines.
- Return `Result`/`Option` for expected failures; use contracts for assumptions.
- Annotate function parameters and return types: annotations are checked,
  documented, and drive property-test generation.
- Write `test` and `property` blocks next to the code they test.
