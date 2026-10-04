# A tour of Cogito

This tutorial introduces Cogito step by step. Every snippet runs: paste it
into a file and run `cogito file.cog`, or type it into the REPL (`cogito`).
For the complete reference, see [llm-spec.md](llm-spec.md).

## 1. Hello, values

```cogito
print("Hello, world!")

let name = "Ada"          # an immutable binding
var visits = 0            # a variable that can change
visits += 1
print("Hello, {name}! Visit number {visits}.")
```

`let` creates a binding that never changes; `var` creates one that can.
Strings interpolate any expression in braces, and format specs come after a
colon:

```cogito
let price = 1234.5
print("{"total":<8}{price:>10.2}")   # total      1234.50
print("{42:05} {255:x} {0.25:.0%}")   # 00042 ff 25%
```

Statements end at the end of a line. There are no semicolons (though you may
use `;` to put two statements on one line).

## 2. Numbers, and why `/` is not `//`

```cogito
print(7 / 2)       # 3.5   (division always gives a Float)
print(7 // 2)      # 3     (floor division)
print(-7 % 3)      # 2     (modulo follows the divisor's sign)
print(2 ** 10)     # 1024
```

Ints are 64-bit. If a calculation overflows, Cogito stops with an error
instead of silently producing a wrong number. Dividing by zero is also an
error.

## 3. Conditions are Bools

```cogito
let temperature = 23
let feel = if temperature > 25 { "hot" } else if temperature > 15 { "mild" } else { "cold" }
print(feel)
```

`if` is an expression: it produces a value. Conditions must be `true` or
`false`. Writing `if count { ... }` is an error, because "is 0 false?" has a
different answer in every language. Write `if count != 0` instead.

Boolean operators are words: `and`, `or`, `not`.

## 4. Functions

```cogito
fn area(width: Float, height: Float) -> Float {
  width * height        # the last expression is the result
}

fn square(x: Int) -> Int => x * x       # one-expression body

fn greet(name: Str, greeting: Str = "Hello") -> Str => "{greeting}, {name}!"

print(area(2.0, 3.5))
print(greet("Ada"))
print(greet("Ada", greeting: "Welcome"))
```

Type annotations are optional, but when present they are checked every time
the function is called and every time it returns. `area(2, 3)` works (Int is
converted to Float); `area("2", 3)` is an error that names the parameter.
Where the types are already clear from the code, as here, that error is
reported before the program even starts:

```
error[E0121]: `area` expects `width` to be Float, but this argument is a Str
```

Where they are not (a value read from a file, a parameter without an
annotation), the check happens when the line runs. Either way, a value of
the wrong type never gets past an annotation.

Any function can be called with method syntax: `x.f(y)` means `f(x, y)`.
That is how built-ins like `len` and `split` read naturally:

```cogito
let words = "the quick brown fox".split(" ")
print(words.len())                     # 4
print(words.map(fn(w) => w.upper()))   # ["THE", "QUICK", "BROWN", "FOX"]
```

`fn(w) => w.upper()` is an anonymous function. Pipelines read left to right:
`x |> f(a)` means `f(x, a)`.

```cogito
let total = (1..=100)
  |> filter(fn(n) => n % 3 == 0)
  |> map(fn(n) => n * n)
  |> sum
print(total)
```

## 5. Lists, maps, tuples and records

```cogito
let primes = [2, 3, 5, 7, 11]
print(primes[0], primes[-1], primes[1..3])    # 2 11 [3, 5]
print(primes.get(10))                          # None (no crash)

let ages = ["Ada": 36, "Alan": 41]
print(ages["Ada"])
print(ages.get("Grace").unwrap_or(0))

let point = (3, 4)
let (x, y) = point                             # destructuring

let user = { name: "Ada", langs: ["en", "fr"] }   # an anonymous record
print(user.name)
let older = { ..user, name: "Ada L." }        # copy with changes
```

Comprehensions build lists:

```cogito
let squares = [n * n for n in 1..=10 if n % 2 == 0]
let grid = [(r, c) for r in 0..2 for c in 0..3]
```

To count or group things in a map, `update!` changes one entry, starting
from a default when the key is new:

```cogito
var counts: Map[Str, Int] = [:]
for word in "the cat and the hat".words() {
  counts.update!(word, 0, fn(n) => n + 1)
}
print(counts)    # ["the": 2, "cat": 1, "and": 1, "hat": 1]
```

## 6. Value semantics and mutation

Here is the most important rule in Cogito: **values never change behind your
back.**

```cogito
var a = [1, 2, 3]
let b = a          # b is an independent copy
a.push!(4)         # mutate a
a[0] = 100
print(a)           # [100, 2, 3, 4]
print(b)           # [1, 2, 3]   (unchanged)
```

Functions whose names end with `!` change their first argument, and that
argument must be a `var`. Every mutating built-in has a non-mutating twin
that returns a new value instead:

```cogito
var xs = [3, 1, 2]
let sorted = xs.sort()    # new list; xs unchanged
xs.sort!()                # sorts xs itself
```

You can write your own mutating functions:

```cogito
fn add_bonus!(scores: List[Int], bonus: Int) {
  for i in 0..scores.len() {
    scores[i] += bonus
  }
}

var scores = [70, 85, 90]
scores.add_bonus!(5)
print(scores)      # [75, 90, 95]
```

Copies are cheap: Cogito shares storage between copies until one of them
changes.

## 7. Your own types

A record type has named fields; an enum type is a choice between variants:

```cogito
type Point = { x: Float, y: Float }

type Shape =
  | Circle(center: Point, radius: Float)
  | Rect(corner: Point, width: Float, height: Float)

let c = Circle(center: Point(0.0, 0.0), radius: 2.0)
print(c.radius)
```

Constructors are called like functions, with named or positional arguments.
Their fields are type-checked.

## 8. Pattern matching

`match` takes a value apart:

```cogito
type Point = { x: Float, y: Float }
type Shape =
  | Circle(center: Point, radius: Float)
  | Rect(corner: Point, width: Float, height: Float)

fn area(s: Shape) -> Float => match s {
  Circle(_, r) => pi * r * r
  Rect(_, w, h) => w * h
}

fn describe(xs: List[Int]) -> Str => match xs {
  [] => "empty"
  [x] => "just {x}"
  [first, ..rest] if first > 100 => "starts big, then {rest.len()} more"
  [first, ..] => "starts with {first}"
}
```

If you forget a case, whether an enum variant, a list length or a value of
a tuple, Cogito tells you before the program runs:

```
error[E0109]: non-exhaustive match: `Rect(..)` is not handled
```

To ask only *whether* a value has a shape, use `is`:

```cogito
type Slot = Empty | Full(Int)

let slots = [Full(3), Empty, Full(5)]
print(slots.count(fn(s) => s is Full(_)))    # 2
```

## 9. When things can fail: Option and Result

There is no `null`. A value that may be missing is an `Option`: `Some(x)`
or `None`. An operation that may fail returns a `Result`: `Ok(x)` or
`Err(e)`.

```cogito
fn parse_point(s: Str) -> Result[(Int, Int), Str] {
  let parts = s.split(",")
  if parts.len() != 2 {
    return Err("expected two numbers, got `{s}`")
  }
  let x = parse_int(parts[0]).ok_or("bad x: {parts[0]}")?
  let y = parse_int(parts[1]).ok_or("bad y: {parts[1]}")?
  Ok((x, y))
}

print(parse_point("3,4"))      # Ok((3, 4))
print(parse_point("3,four"))   # Err("bad y: four")
```

The `?` operator unwraps an `Ok`, or returns the `Err` from the current
function immediately. It works the same way for `Some` and `None`.

## 10. Contracts

A contract states what a function needs and what it promises:

```cogito
fn average(xs: List[Float]) -> Float
  requires not xs.is_empty()
  ensures xs.min().unwrap() <= result and result <= xs.max().unwrap()
{
  xs.sum() / xs.len()
}
```

If a caller passes an empty list, the error says the *caller* broke the
precondition. If the function returns a value outside the promised range,
the error says the *function* broke its postcondition.

That promise looks obviously true: an average lies between the smallest and
the largest element. Let the computer check it:

```console
$ cogito verify stats.cog
stats.cog
  ✗ average  postcondition violated
      counterexample (after 64 cases, shrunk 2 times):
        xs = [0.1, 0.1, 0.1]
      error[E0302]: postcondition of `average` violated: `xs.min().unwrap() <= result and result <= xs.max().unwrap()`
        = note: where xs = [0.1, 0.1, 0.1], result = 0.10000000000000002
```

`cogito verify` generated random lists from the parameter type, kept the
ones that satisfy `requires`, checked `ensures` on each result, and shrank
the first failure to a minimal example. The bug is real: in floating point,
`0.1 + 0.1 + 0.1` divided by 3 is slightly more than `0.1`. The fix is to
keep the promise explicitly:

```cogito
fn average(xs: List[Float]) -> Float
  requires not xs.is_empty()
  ensures xs.min().unwrap() <= result and result <= xs.max().unwrap()
{
  let mean = xs.sum() / xs.len()
  # floating-point rounding can push the mean just outside [min, max]
  clamp(mean, xs.min().unwrap(), xs.max().unwrap())
}
```

```console
$ cogito verify stats.cog
stats.cog
  ✓ average  200 cases, contracts held, 13 inputs rejected by `requires`
```

A type can carry a contract too. A `where` clause on a record type states
what every value of the type satisfies:

```cogito
type Span = { lo: Int, hi: Int }
  where lo <= hi

fn overlap(a: Span, b: Span) -> Option[Span]
  ensures match result {
    Some(s) => a.lo <= s.lo and s.hi <= a.hi
    None => true
  }
{
  let lo = max(a.lo, b.lo)
  let hi = min(a.hi, b.hi)
  Some(Span(lo, hi))
}
```

`Span(5, 1)` is an error, and so is a change such as `s.lo = 9` that breaks
the invariant. `cogito verify` generates only valid spans as inputs, and
here finds that two spans that do not overlap make an invalid one:

```console
$ cogito verify spans.cog
spans.cog
  ✗ overlap  type invariant violated
      counterexample (on the first case, shrunk 12 times):
        a = Span(lo: 1, hi: 1)
        b = Span(lo: 0, hi: 0)
      error[E0303]: invariant of `Span` violated: `lo <= hi`
```

The fix is to return `None` when `lo > hi`. A function whose name ends in
`!` may break the invariant of its first argument while it runs (moving `lo`
before `hi`, say), as long as it holds again when the function returns.

## 11. Tests and properties

Tests live next to the code:

```cogito
fn reverse_words(s: Str) -> Str => s.split(" ").reverse().join(" ")

test "reverses word order" {
  assert reverse_words("one two three") == "three two one"
}

property "reversing twice changes nothing" (s: Str) where not (" " in s) {
  assert reverse_words(reverse_words(s)) == s
}
```

`cogito test file.cog` runs them. A `property` receives random inputs built
from its parameter types. If it fails, Cogito *shrinks* the input to the
simplest one that still fails, so you see `s = " "` instead of a 40-character
string. When an assertion fails, you see both sides:

```
error[E0300]: assertion failed: `reverse_words("a b") == "a b"`
  = note: left:  "b a"
          right: "a b"
```

## 12. Modules

Split programs across files with `import`. Paths are relative to the
importing file.

```cogito
# geometry.cog
type Vec2 = { x: Float, y: Float }
fn length(v: Vec2) -> Float => hypot(v.x, v.y)
```

```cogito
# main.cog
import "geometry.cog"

let v = geometry.Vec2(3.0, 4.0)
print(geometry.length(v))   # 5.0
print(v.length())           # methods are found in the type's module
```

## 13. Tools

`cogito fmt` lays out your files in the one canonical style (two-space
indentation, standard spacing; your line breaks and comments are kept), and
`cogito fmt --check` tells you whether anything would change. `cogito check`
reports every problem it can find without running anything.

`cogito lsp` is a language server: in an editor that supports the Language
Server Protocol you get these errors and warnings as you type, formatting,
documentation on hover, completion, an outline, and go to definition.
[editors/vscode](../editors/vscode) has a VS Code extension and setup notes
for other editors.

## 14. Where next

- Browse [the examples](../examples): a calculator interpreter, Conway's
  Game of Life, a contract-checked bank, N-queens, and more.
- Read [the specification](llm-spec.md) for every detail and the whole
  standard library (`cogito doc` lists the built-ins too).
- Read [DESIGN.md](../DESIGN.md) to learn why the language works this way.
