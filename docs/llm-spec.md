# Cogito language specification (compact)

Cogito is a small, expression-oriented, dynamically typed language with
optional type annotations that are checked at runtime, value semantics, contracts,
and built-in tests. This document is complete enough to write correct
programs; it is written to fit in a language model's context window.

Run: `cogito FILE.cog` · Test: `cogito test FILE.cog` · Check contracts:
`cogito verify FILE.cog` · Static check: `cogito check FILE.cog` · REPL: `cogito`.

## Lexical rules

- Comments start with `#` and run to the end of the line.
- Statements end at a newline (or `;`). Newlines inside `(...)` and `[...]`
  are ignored. A line starting with `.`, `|>`, `and` or `or` continues the
  previous line. A binary operator at the end of a line also continues it.
- **Names**: variables and functions are `lower_snake_case`; types and
  constructors are `UpperCamelCase`. This is enforced. A function whose name
  ends in `!` mutates its first argument (see Mutation).
- Keywords: `let var fn return if else while for in loop break continue match
  type test property requires ensures and or not true false import as assert where`.
- Numbers: `42`, `1_000`, `0xff`, `0b1010`, `0o17`, `3.14`, `1e-9`. Int is
  64-bit signed; overflow is an error (never wraps).
- Strings: `"..."` with escapes `\n \t \r \\ \" \0 \{ \} \u{1F600}`.
  Interpolation: `"x = {x + 1}"`, with an optional format spec after `:` —
  `{x:.2}` (2 decimals), `{s:>8}` `{s:<8}` `{s:^8}` (align in width 8),
  `{n:05}` (zero pad), `{n:+}`, `{n:x}` `{n:b}` `{n:o}` (hex/binary/octal),
  `{f:e}`, `{f:.1%}`, `{s:*>6}` (fill char). Write `\{` for a literal brace.
  `"""..."""` strings span lines and are dedented. Raw strings `r"..."` and
  `r"""..."""` have no escapes and no interpolation (use them for JSON).

## Values and types

| Type | Literal / constructor | Notes |
|---|---|---|
| `Int` | `42` | `/` always gives Float; `//` floor division; `%` floor modulo |
| `Float` | `3.0` | Int is accepted (converted) where Float is annotated |
| `Bool` | `true` `false` | no truthiness: conditions must be Bool |
| `Str` | `"hi"` | indexing/len count characters; immutable |
| `Unit` | `()` | value of statements, `if` without `else`, etc. |
| `List[T]` | `[1, 2, 3]`, `[..xs, 4]` | `xs[0]`, `xs[-1]`, `xs[1..3]`, `xs[2..]`, `xs[..2]` |
| `Map[K, V]` | `["a": 1, "b": 2]`, empty `[:]` | insertion-ordered; `m[k]`, `m.get(k)` |
| tuples | `(1, "a")`, `(x,)` | `t.0`, `t[1]` |
| records | `{ x: 1, y: 2 }`, `{ ..r, y: 5 }` | structural; field order irrelevant for `==` |
| `Range` | `0..10`, `0..=10`, `0..` | Int only; end exclusive (`..=` inclusive) |
| `Option[T]` | `Some(x)`, `None` | built-in enum |
| `Result[T, E]` | `Ok(x)`, `Err(e)` | built-in enum |
| `Ordering` | `Less`, `Equal`, `Greater` | returned by `compare(a, b)` |
| functions | `fn(x) => x + 1` | first-class |
| `Any` | | annotation that accepts anything |

**Value semantics**: every value behaves like an independent copy.
`let b = a` then changing `a` never changes `b`. (Implemented with
copy-on-write, so copies are cheap.) Closures capture *values*, not variables.

**Equality** `==` is structural (deep). `1 == 1.0` is true. Comparison
`< <= > >=` works on numbers, strings, lists/tuples (lexicographic), and
values of the same enum/record type. Comparing different kinds is an error.

## Declarations

```
let x = 1                 # immutable binding
var count = 0             # mutable binding
let (a, b) = (1, 2)       # destructuring (any pattern)
var [first, ..rest] = xs  # `var` patterns make every bound name mutable
let n: Int = 5            # annotations are checked at runtime

fn add(a: Int, b: Int) -> Int {    # block body: the last expression is the result
  a + b
}
fn square(x: Int) -> Int => x * x  # expression body
fn greet(name: Str, greeting: Str = "Hello") -> Str => "{greeting}, {name}!"
greet("Ada")  greet("Ada", "Hi")  greet(name: "Ada", greeting: "Yo")   # named args

fn first[T](xs: List[T]) -> Option[T] => xs.get(0)   # generic parameters (unchecked)

type Point = { x: Float, y: Float }          # record type
type Shape =                                  # enum (sum type)
  | Circle(center: Point, radius: Float)      # named fields
  | Rect(Point, Point)                        # positional fields
  | Empty                                     # no fields
type Tree[T] = Leaf | Node(Tree[T], T, Tree[T])
type Grid = List[List[Int]]                   # alias

let p = Point(x: 1.0, y: 2.0)   # or Point(1.0, 2.0); fields are type-checked
let c = Circle(center: p, radius: 3.0)
p.x   c.radius                  # field access (named fields only)
```

Top-level functions and types may be used before their definition; `let`/`var`
may not. Functions with the same name but different parameter type
annotations are **overloads**; the first whose annotations accept the
arguments is called. A user function named like a built-in (e.g. `len`) adds
an overload and falls back to the built-in. If a top-level `fn main()`
exists, it runs after the top-level statements.

## Expressions and control flow

Everything is an expression. Blocks `{ ... }` evaluate to their last expression.

```
let size = if n > 10 { "big" } else if n > 5 { "medium" } else { "small" }
while cond { ... }
for x in xs { ... }             # lists, ranges, strings (chars), maps ((k, v) tuples)
for (i, x) in xs.enumerate() { ... }
let found = loop { if done() { break value } }   # `loop` returns its break value
break  continue  return value
[x * x for x in 1..=10 if x % 2 == 0]           # comprehension (several `for`/`if` allowed)
```

Operators by precedence (loosest first): `or`; `and`; `not`;
`== != < <= > >= in` (`not in`) — **cannot be chained** (`a < b < c` is an
error); `|>`; `..` `..=`; `+ -`; `* / // %`; unary `-`; `**` (right-assoc).
`+` concatenates strings and lists; `str * n` and `list * n` repeat.
No implicit conversions: `"a" + 1` is an error; use `"a{1}"` or `str(1)`.

**Method syntax**: `x.f(a, b)` calls `f(x, a, b)` — any function, including
built-ins (`xs.len()`, `"a,b".split(",")`). If `x` is a record with a field
`f`, the field is called instead. For values whose type comes from an imported
module, functions of that module are also found.
**Pipelines**: `x |> f(a)` is `f(x, a)`; `x |> f` is `f(x)`.

## Pattern matching

```
match value {
  0 => "zero"
  1 | 2 => "one or two"                    # or-patterns
  n if n < 0 => "negative"                 # guard
  3..=9 => "small"                         # range (also "a"..="z")
  "hello" => "greeting"
  [] => "empty list"
  [x] => "one element"
  [first, ..rest] => "has a head"          # also [..init, last], [a, .., z]
  (a, b) => "pair"
  { name, age: years, .. } => "record"     # `..` allows extra fields
  Circle(center: c, radius: r) => "named"  # or positional: Circle(c, r)
  Rect(..) => "ignore fields"
  Some(x) => x
  n @ 10..=19 => "bind and test: {n}"
  _ => "anything"
}
```

A `match` on an enum must cover every variant (or have `_`); this is checked
before the program runs. At runtime, no matching arm is an error.

## Mutation

Only `var` bindings change. Assignment forms: `x = v`, `x += v` (also `-= *= /= %=`),
`xs[i] = v`, `m[k] = v` (inserts), `p.field = v`, nested `grid[r][c] = v`.
Mutating functions end in `!` and require a `var` (or field/index of one) as
first argument: `xs.push!(4)`, `xs.sort!()`, `m.insert!(k, v)`.
Non-mutating twins return new values: `ys = xs.push(4)`, `xs.sort()`.
User-defined `fn grow!(xs: List[Int], n: Int) { xs.push!(n) }` mutates the
caller's variable through its first parameter: `v.grow!(3)` or `grow!(v, 3)`.
Closures cannot assign to captured variables (they hold snapshots).

## Errors as values

Functions that may fail return `Result` (or `Option`). The postfix `?`
operator unwraps `Ok(x)`/`Some(x)` or returns the `Err`/`None` from the
current function:

```
fn parse_age(s: Str) -> Result[Int, Str] {
  let n = parse_int(s).ok_or("not a number: {s}")?
  if n < 0 { return Err("negative") }
  Ok(n)
}
```

Bugs (index out of bounds, division by zero, failed contracts, `panic(msg)`)
stop the program with a diagnostic and stack trace. `catch(fn() => expr)`
converts such an error into `Err(message)` (mainly for tests).

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
```

`cogito verify FILE` generates random arguments from the parameter types for
every function with contracts, discards inputs that fail `requires`, and
reports (shrunk) counterexamples that break `ensures` or crash.

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

Generators exist for Int, Float, Bool, Str, Unit, Range, List, Map, tuples,
records and user types (including recursive enums). Failing inputs are shrunk
to a minimal counterexample. `cogito test` runs top-level statements but not `main`.

## Modules

```
import "lib/geometry.cog"            # binds `geometry`; path relative to this file
import "lib/geometry.cog" as geo
geo.area(c)   geo.Point(1.0, 2.0)    # qualified access
fn f(p: geo.Point) -> Float => ...   # qualified types
```

## Built-in functions (all callable as methods: `xs.map(f)`)

- **I/O**: `print(..)` `write(..)` (no newline) `eprint(..)` `input(prompt)`
  `read_line() -> Option[Str]` `read_stdin()` `read_file(p) -> Result[Str, Str]`
  `write_file(p, s)` `append_file(p, s)` `file_exists(p)` `list_dir(p)`
  `args() -> List[Str]` `env(name) -> Option[Str]` `exit(code)` `time()`
  `clock()` `sleep(seconds)`
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
- **Strings**: `split(sep)` (no sep: whitespace) `lines words chars trim
  trim_start trim_end upper lower capitalize starts_with ends_with replace(a, b)
  pad_left(w, fill) pad_right(w, fill) is_digit is_alpha is_alnum is_space
  is_upper is_lower reverse repeat count(sub) index_of(sub)`, slicing `s[1..3]`.
- **Option/Result**: `unwrap expect(msg) unwrap_or(d) unwrap_or_else(f)
  is_some is_none is_ok is_err map(f) and_then(f) map_err(f) ok_or(e) ok`
- **JSON**: `to_json(x, indent = 0)`, `parse_json(s) -> Result` (objects become
  Maps with Str keys, `null` becomes `None`).

## Style notes for writing Cogito

- Prefer `let`; use `var` only for values that change.
- Prefer expressions (`if`/`match` returning values) and pipelines.
- Return `Result`/`Option` for expected failures; use contracts for assumptions.
- Annotate function parameters and return types: annotations are checked,
  documented, and drive property-test generation.
- Write `test` and `property` blocks next to the code they test.
