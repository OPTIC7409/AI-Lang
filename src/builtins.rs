//! The standard library: built-in functions.
//!
//! Naming conventions:
//! * functions never mutate their arguments; they return new values;
//! * functions whose name ends in `!` mutate their first argument in place,
//!   which must be a `var` (or a field/index of one): `xs.push!(4)`;
//! * functions that may have no answer return an Option (`first`, `get`,
//!   `find`, `min`, `parse_int`, ...); functions that can fail for reasons
//!   outside the program's control return a Result (`read_file`, ...).

use crate::interp::{norm_index, Ctrl, Interp, R};
use crate::span::Span;
use crate::types::{OPTION_ID, ORDERING_ID, RESULT_ID};
use crate::value::*;
use std::cmp::Ordering;
use std::io::Write;
use std::rc::Rc;

pub type PureFn = fn(&mut Interp, Vec<Value>, Span) -> R;
pub type MutFn = fn(&mut Interp, &mut Value, Vec<Value>, Span) -> R;

#[derive(Clone, Copy)]
pub enum BFn {
    Pure(PureFn),
    Mut(MutFn),
}

pub struct BuiltinDef {
    pub name: &'static str,
    pub min: u8,
    pub max: u8,
    pub f: BFn,
    /// First line: signature. Second line: description.
    pub doc: &'static str,
    pub category: &'static str,
}

pub const VARIADIC: u8 = 255;

macro_rules! b {
    ($cat:expr, $name:expr, $min:expr, $max:expr, $f:expr, $doc:expr) => {
        BuiltinDef { name: $name, min: $min, max: $max, f: BFn::Pure($f), doc: $doc, category: $cat }
    };
}
macro_rules! m {
    ($cat:expr, $name:expr, $min:expr, $max:expr, $f:expr, $doc:expr) => {
        BuiltinDef { name: $name, min: $min, max: $max, f: BFn::Mut($f), doc: $doc, category: $cat }
    };
}

pub static BUILTINS: &[BuiltinDef] = &[
    // ---- output and input
    b!("io", "print", 0, VARIADIC, b_print, "print(values...)\nPrint values separated by spaces, then a newline."),
    b!("io", "write", 0, VARIADIC, b_write, "write(values...)\nPrint values separated by spaces, without a newline."),
    b!("io", "eprint", 0, VARIADIC, b_eprint, "eprint(values...)\nPrint values to standard error."),
    b!("io", "input", 0, 1, b_input, "input(prompt: Str = \"\") -> Str\nRead a line from standard input (without the newline)."),
    b!("io", "read_line", 0, 0, b_read_line, "read_line() -> Option[Str]\nRead a line from standard input; None at end of input."),
    b!("io", "read_stdin", 0, 0, b_read_stdin, "read_stdin() -> Str\nRead all of standard input."),
    b!("io", "read_file", 1, 1, b_read_file, "read_file(path: Str) -> Result[Str, Str]\nRead a whole text file."),
    b!("io", "write_file", 2, 2, b_write_file, "write_file(path: Str, text: Str) -> Result[Unit, Str]\nWrite (or replace) a text file."),
    b!("io", "append_file", 2, 2, b_append_file, "append_file(path: Str, text: Str) -> Result[Unit, Str]\nAppend text to a file, creating it if needed."),
    b!("io", "file_exists", 1, 1, b_file_exists, "file_exists(path: Str) -> Bool\nWhether a file or directory exists."),
    b!("io", "list_dir", 1, 1, b_list_dir, "list_dir(path: Str) -> Result[List[Str], Str]\nThe sorted names of the entries in a directory."),
    b!("io", "args", 0, 0, b_args, "args() -> List[Str]\nThe command-line arguments given after the script name."),
    b!("io", "env", 1, 1, b_env, "env(name: Str) -> Option[Str]\nRead an environment variable."),
    b!("io", "exit", 0, 1, b_exit, "exit(code: Int = 0)\nStop the program immediately."),
    b!("io", "time", 0, 0, b_time, "time() -> Float\nSeconds since the Unix epoch."),
    b!("io", "clock", 0, 0, b_clock, "clock() -> Float\nSeconds since the program started (for measuring durations)."),
    b!("io", "sleep", 1, 1, b_sleep, "sleep(seconds: Float)\nPause the program."),
    b!("io", "flush", 0, 0, b_flush, "flush()\nWrite out buffered output now. Output to a terminal appears at once; output to a pipe or file is\nbuffered until the buffer fills or the program ends."),
    // ---- debugging and errors
    b!("core", "panic", 0, 1, b_panic, "panic(message: Str)\nStop with an error. Use for bugs, not for expected failures (return Err for those)."),
    b!("core", "todo", 0, 1, b_todo, "todo(message: Str = \"\")\nMark unfinished code; stops with an error if reached."),
    b!("core", "dbg", 1, 1, b_dbg, "dbg(x: T) -> T\nPrint a value with its location to standard error, and return it."),
    b!("core", "catch", 1, 1, b_catch, "catch(f: fn() -> T) -> Result[T, Str]\nCall f; turn a runtime error into Err(message). Mainly for tests."),
    b!("core", "type_of", 1, 1, b_type_of, "type_of(x) -> Str\nThe name of a value's type: \"Int\", \"List\", \"Point\", ..."),
    // ---- conversion
    b!("convert", "str", 1, 1, b_str, "str(x) -> Str\nConvert any value to its printed form."),
    b!("convert", "repr", 1, 1, b_repr, "repr(x) -> Str\nThe source-code form of a value (strings are quoted)."),
    b!("convert", "int", 1, 1, b_int, "int(x: Float | Str | Bool) -> Int\nConvert to Int (truncating Floats). Errors on invalid strings; see parse_int."),
    b!("convert", "float", 1, 1, b_float, "float(x: Int | Str) -> Float\nConvert to Float. Errors on invalid strings; see parse_float."),
    b!("convert", "parse_int", 1, 2, b_parse_int, "parse_int(s: Str, base: Int = 10) -> Option[Int]\nParse an integer in the given base (2 to 36; a matching 0x, 0o or 0b prefix is allowed), or None."),
    b!("convert", "parse_float", 1, 1, b_parse_float, "parse_float(s: Str) -> Option[Float]\nParse a number, or None."),
    b!("convert", "ord", 1, 1, b_ord, "ord(c: Str) -> Int\nThe Unicode code point of a one-character string."),
    b!("convert", "chr", 1, 1, b_chr, "chr(n: Int) -> Str\nThe one-character string for a Unicode code point."),
    b!("convert", "to_json", 1, 2, b_to_json, "to_json(x, indent: Int = 0) -> Str\nSerialize a value as JSON (records and maps become objects)."),
    b!("convert", "parse_json", 1, 1, b_parse_json, "parse_json(s: Str) -> Result[Any, Str]\nParse JSON: objects become Maps, null becomes None."),
    // ---- math
    b!("math", "abs", 1, 1, b_abs, "abs(x: Int | Float) -> Int | Float\nAbsolute value."),
    b!("math", "sqrt", 1, 1, b_sqrt, "sqrt(x: Float) -> Float\nSquare root (error for negative numbers)."),
    b!("math", "pow", 2, 2, b_pow, "pow(x: Float, y: Float) -> Float\nx raised to the power y, as a Float."),
    b!("math", "exp", 1, 1, b_exp, "exp(x: Float) -> Float\ne raised to x."),
    b!("math", "ln", 1, 1, b_ln, "ln(x: Float) -> Float\nNatural logarithm (error for x <= 0)."),
    b!("math", "log", 2, 2, b_log, "log(x: Float, base: Float) -> Float\nLogarithm in the given base."),
    b!("math", "log2", 1, 1, b_log2, "log2(x: Float) -> Float\nBase-2 logarithm."),
    b!("math", "log10", 1, 1, b_log10, "log10(x: Float) -> Float\nBase-10 logarithm."),
    b!("math", "sin", 1, 1, b_sin, "sin(x: Float) -> Float\nSine (radians)."),
    b!("math", "cos", 1, 1, b_cos, "cos(x: Float) -> Float\nCosine (radians)."),
    b!("math", "tan", 1, 1, b_tan, "tan(x: Float) -> Float\nTangent (radians)."),
    b!("math", "asin", 1, 1, b_asin, "asin(x: Float) -> Float\nArc sine."),
    b!("math", "acos", 1, 1, b_acos, "acos(x: Float) -> Float\nArc cosine."),
    b!("math", "atan", 1, 1, b_atan, "atan(x: Float) -> Float\nArc tangent."),
    b!("math", "atan2", 2, 2, b_atan2, "atan2(y: Float, x: Float) -> Float\nAngle of the point (x, y)."),
    b!("math", "hypot", 2, 2, b_hypot, "hypot(x: Float, y: Float) -> Float\nsqrt(x*x + y*y), without overflow."),
    b!("math", "floor", 1, 1, b_floor, "floor(x: Float) -> Int\nRound down to an Int."),
    b!("math", "ceil", 1, 1, b_ceil, "ceil(x: Float) -> Int\nRound up to an Int."),
    b!("math", "round", 1, 2, b_round, "round(x: Float, digits: Int = 0) -> Int | Float\nRound to the nearest Int (halves away from zero), or to `digits` decimals as a Float."),
    b!("math", "trunc", 1, 1, b_trunc, "trunc(x: Float) -> Int\nRound toward zero."),
    b!("math", "sign", 1, 1, b_sign, "sign(x: Int | Float) -> Int\n-1, 0 or 1."),
    b!("math", "clamp", 3, 3, b_clamp, "clamp(x, lo, hi)\nLimit x to the range [lo, hi]."),
    b!("math", "gcd", 2, 2, b_gcd, "gcd(a: Int, b: Int) -> Int\nGreatest common divisor."),
    b!("math", "lcm", 2, 2, b_lcm, "lcm(a: Int, b: Int) -> Int\nLeast common multiple."),
    b!("math", "is_nan", 1, 1, b_is_nan, "is_nan(x: Float) -> Bool\nWhether x is not-a-number."),
    b!("math", "fixed", 2, 2, b_fixed, "fixed(x: Float, digits: Int) -> Str\nFormat a number with exactly `digits` decimals."),
    b!("math", "wrapping_add", 2, 2, b_wrapping_add, "wrapping_add(a: Int, b: Int) -> Int\nAddition that wraps around on overflow instead of failing (for hashing and checksums)."),
    b!("math", "wrapping_sub", 2, 2, b_wrapping_sub, "wrapping_sub(a: Int, b: Int) -> Int\nSubtraction that wraps around on overflow."),
    b!("math", "wrapping_mul", 2, 2, b_wrapping_mul, "wrapping_mul(a: Int, b: Int) -> Int\nMultiplication that wraps around on overflow."),
    b!("core", "hash", 1, 1, b_hash, "hash(x) -> Int\nA hash of any value: equal values have equal hashes. Stable within one version of Cogito."),
    b!("math", "bit_and", 2, 2, b_bit_and, "bit_and(a: Int, b: Int) -> Int\nBitwise and."),
    b!("math", "bit_or", 2, 2, b_bit_or, "bit_or(a: Int, b: Int) -> Int\nBitwise or."),
    b!("math", "bit_xor", 2, 2, b_bit_xor, "bit_xor(a: Int, b: Int) -> Int\nBitwise exclusive or."),
    b!("math", "bit_not", 1, 1, b_bit_not, "bit_not(a: Int) -> Int\nBitwise complement."),
    b!("math", "shl", 2, 2, b_shl, "shl(a: Int, n: Int) -> Int\nShift left by n bits; an error if the result does not fit in an Int (see wrapping_shl)."),
    b!("math", "shr", 2, 2, b_shr, "shr(a: Int, n: Int) -> Int\nArithmetic shift right by n bits (the sign is kept: shr(-8, 1) == -4)."),
    b!("math", "wrapping_shl", 2, 2, b_wrapping_shl, "wrapping_shl(a: Int, n: Int) -> Int\nShift left by n bits, dropping the bits shifted out (for hashes and random number generators such as xorshift)."),
    b!("math", "shr_logical", 2, 2, b_shr_logical, "shr_logical(a: Int, n: Int) -> Int\nShift right by n bits, filling with zeros (the Int as 64 unsigned bits: shr_logical(-1, 60) == 15)."),
    b!("math", "seed", 1, 1, b_seed, "seed(n: Int)\nSeed the random number generator, for reproducible runs."),
    b!("math", "random", 0, 0, b_random, "random() -> Float\nA random Float in [0, 1)."),
    b!("math", "random_int", 2, 2, b_random_int, "random_int(lo: Int, hi: Int) -> Int\nA random Int in [lo, hi] (inclusive)."),
    b!("math", "shuffle", 1, 1, b_shuffle, "shuffle(xs: List[T]) -> List[T]\nA randomly reordered copy."),
    b!("math", "choice", 1, 1, b_choice, "choice(xs: List[T]) -> Option[T]\nA random element, or None for an empty list."),
    // ---- comparison
    b!("core", "compare", 2, 2, b_compare, "compare(a, b) -> Ordering\nLess, Equal or Greater."),
    b!("core", "min", 1, VARIADIC, b_min, "min(xs) -> Option[T]   |   min(a, b, ...) -> T\nThe smallest element of a collection (None if empty), or the smallest argument."),
    b!("core", "max", 1, VARIADIC, b_max, "max(xs) -> Option[T]   |   max(a, b, ...) -> T\nThe largest element of a collection (None if empty), or the largest argument."),
    // ---- collections
    b!("collections", "len", 1, 1, b_len, "len(x: List | Str | Map | Tuple | Range) -> Int\nNumber of elements (characters, for strings)."),
    b!("collections", "is_empty", 1, 1, b_is_empty, "is_empty(x) -> Bool\nWhether a collection or string has no elements."),
    b!("collections", "range", 1, 3, b_range, "range(end) | range(start, end) | range(start, end, step)\nA range (step 1) or a list of numbers with the given step."),
    b!("collections", "push", 2, 2, b_push, "push(xs: List[T], x: T) -> List[T]\nA new list with x added at the end."),
    m!("collections", "push!", 2, 2, m_push, "xs.push!(x)\nAdd x to the end of the list variable xs."),
    m!("collections", "pop!", 1, 1, m_pop, "xs.pop!() -> Option[T]\nRemove and return the last element of the list variable xs."),
    b!("collections", "insert", 2, 3, b_insert, "insert(xs: List[T], i: Int, x: T) -> List[T]   |   insert(m: Map, k, v) -> Map   |   insert(s: Set[T], x: T) -> Set[T]\nA new list with x inserted at index i, a new map with k set to v, or a new set with x added."),
    m!("collections", "insert!", 2, 3, m_insert, "xs.insert!(i, x)   |   m.insert!(k, v)   |   s.insert!(x) -> Bool\nInsert into the list, map or set variable in place (for a set: true if x was not already there)."),
    b!("collections", "remove", 2, 2, b_remove, "remove(xs: List[T], i: Int) -> List[T]   |   remove(m: Map, k) -> Map\nA copy without the element at index i (or key k)."),
    m!("collections", "remove!", 2, 2, m_remove, "xs.remove!(i) -> T   |   m.remove!(k) -> Option[V]\nRemove from the list or map variable in place, returning what was removed."),
    b!("collections", "set", 3, 3, b_set, "set(xs: List[T], i: Int, x: T) -> List[T]   |   set(m: Map, k, v) -> Map\nA copy with one element replaced (same as assigning to a `var` copy)."),
    m!("collections", "extend!", 2, 2, m_extend, "xs.extend!(ys)\nAppend all elements of ys to the list variable xs."),
    m!("collections", "clear!", 1, 1, m_clear, "xs.clear!()\nRemove all elements from a list or map variable."),
    m!("collections", "swap!", 3, 3, m_swap, "xs.swap!(i, j)\nSwap two elements of the list variable xs."),
    b!("collections", "get", 2, 2, b_get, "get(xs, i) -> Option[T]   |   get(m, k) -> Option[V]\nThe element at index i (or key k), or None."),
    b!("collections", "update", 4, 4, b_update, "update(m: Map[K, V], k: K, default: V, f: fn(V) -> V) -> Map[K, V]\nA copy with m[k] replaced by f(m[k]), using the default when k is missing."),
    m!("collections", "update!", 4, 4, m_update, "m.update!(k, default, f)\nSet m[k] to f(m[k]) in the map variable, using the default when k is missing: `counts.update!(w, 0, fn(n) => n + 1)`."),
    b!("collections", "get_or", 3, 3, b_get_or, "get_or(xs_or_map, key, default)\nThe element at the index or key, or the default."),
    b!("collections", "first", 1, 1, b_first, "first(xs) -> Option[T]\nThe first element, or None."),
    b!("collections", "last", 1, 1, b_last, "last(xs) -> Option[T]\nThe last element, or None."),
    b!("collections", "map", 2, 2, b_map, "map(xs, f) -> List   |   map(opt, f) -> Option   |   map(res, f) -> Result\nApply f to each element (or to the value inside Some/Ok)."),
    b!("collections", "filter", 2, 2, b_filter, "filter(xs, pred) -> List   |   filter(m, pred(k, v)) -> Map   |   filter(s, pred) -> Str\nKeep the elements for which pred returns true."),
    b!("collections", "reduce", 2, 2, b_reduce, "reduce(xs, f: fn(acc, x) -> acc)\nCombine the elements from left to right. Error on an empty collection; see fold."),
    b!("collections", "fold", 3, 3, b_fold, "fold(xs, init, f: fn(acc, x) -> acc)\nCombine the elements from left to right, starting from init."),
    b!("collections", "sum", 1, 1, b_sum, "sum(xs) -> Int | Float\nThe sum of the numbers (0 for an empty collection)."),
    b!("collections", "product", 1, 1, b_product, "product(xs) -> Int | Float\nThe product of the numbers (1 for an empty collection)."),
    b!("collections", "min_by", 2, 2, b_min_by, "min_by(xs, key: fn(x) -> K) -> Option[T]\nThe element with the smallest key."),
    b!("collections", "max_by", 2, 2, b_max_by, "max_by(xs, key: fn(x) -> K) -> Option[T]\nThe element with the largest key."),
    b!("collections", "sort", 1, 1, b_sort, "sort(xs) -> List\nA sorted copy (stable, ascending); sorting a Str sorts its characters."),
    m!("collections", "sort!", 1, 1, m_sort, "xs.sort!()\nSort the list variable in place."),
    b!("collections", "sort_by", 2, 2, b_sort_by, "sort_by(xs, key: fn(x) -> K) -> List\nA copy sorted by a key (stable, ascending)."),
    m!("collections", "sort_by!", 2, 2, m_sort_by, "xs.sort_by!(key)\nSort the list variable in place by a key."),
    b!("collections", "sort_with", 2, 2, b_sort_with, "sort_with(xs, cmp: fn(a, b) -> Ordering) -> List\nA copy sorted with a comparison function."),
    b!("collections", "reverse", 1, 1, b_reverse, "reverse(xs) -> List   |   reverse(s: Str) -> Str\nA reversed copy."),
    m!("collections", "reverse!", 1, 1, m_reverse, "xs.reverse!()\nReverse the list variable in place."),
    b!("collections", "contains", 2, 2, b_contains, "contains(xs, x) -> Bool\nWhether xs contains x (a substring, for strings; a key, for maps). Same as `x in xs`."),
    b!("collections", "index_of", 2, 2, b_index_of, "index_of(xs, x) -> Option[Int]\nThe first index of x (of a substring, for strings)."),
    b!("collections", "find", 2, 2, b_find, "find(xs, pred) -> Option[T]\nThe first element for which pred is true."),
    b!("collections", "find_index", 2, 2, b_find_index, "find_index(xs, pred) -> Option[Int]\nThe index of the first element for which pred is true."),
    b!("collections", "any", 1, 2, b_any, "any(xs, pred) -> Bool\nWhether pred is true for some element (or whether some element is true)."),
    b!("collections", "all", 1, 2, b_all, "all(xs, pred) -> Bool\nWhether pred is true for every element (or whether every element is true)."),
    b!("collections", "count", 2, 2, b_count, "count(xs, x) -> Int\nHow many elements match x, a predicate, or equal the value x (for a string: how many times the substring x occurs)."),
    b!("collections", "take", 2, 2, b_take, "take(xs, n) -> List\nThe first n elements (characters, for strings)."),
    b!("collections", "drop", 2, 2, b_drop, "drop(xs, n) -> List\nAll but the first n elements (characters, for strings)."),
    b!("collections", "take_while", 2, 2, b_take_while, "take_while(xs, pred) -> List\nThe longest prefix whose elements satisfy pred (a Str for a Str)."),
    b!("collections", "drop_while", 2, 2, b_drop_while, "drop_while(xs, pred) -> List\nThe rest after the longest prefix satisfying pred (a Str for a Str)."),
    b!("collections", "slice", 3, 3, b_slice, "slice(xs, start, end) -> List | Str\nElements from start (inclusive) to end (exclusive); same as xs[start..end]."),
    b!("collections", "zip", 2, 2, b_zip, "zip(xs, ys) -> List[(A, B)]\nPair up elements; stops at the shorter input."),
    b!("collections", "enumerate", 1, 1, b_enumerate, "enumerate(xs) -> List[(Int, T)]\nPair each element with its index."),
    b!("collections", "flat_map", 2, 2, b_flat_map, "flat_map(xs, f: fn(x) -> List) -> List\nMap, then flatten one level."),
    b!("collections", "flatten", 1, 1, b_flatten, "flatten(xss: List[List[T]]) -> List[T]\nConcatenate a list of lists."),
    b!("collections", "join", 1, 2, b_join, "join(xs, sep: Str = \"\") -> Str\nConcatenate the printed forms of the elements, separated by sep."),
    b!("collections", "unique", 1, 1, b_unique, "unique(xs) -> List\nRemove duplicates, keeping the first occurrence (a Str for a Str)."),
    b!("collections", "group_by", 2, 2, b_group_by, "group_by(xs, key: fn(x) -> K) -> Map[K, List[T]]\nGroup elements by a key."),
    b!("collections", "tally", 1, 1, b_tally, "tally(xs) -> Map[T, Int]\nCount how many times each element occurs."),
    b!("collections", "partition", 2, 2, b_partition, "partition(xs, pred) -> (List, List)\nSplit into (elements where pred is true, the rest)."),
    b!("collections", "chunks", 2, 2, b_chunks, "chunks(xs, n) -> List[List]\nSplit into consecutive pieces of length n (the last may be shorter); pieces of a Str are Strs."),
    b!("collections", "windows", 2, 2, b_windows, "windows(xs, n) -> List[List]\nAll consecutive runs of length n; runs of a Str are Strs."),
    b!("collections", "repeat", 2, 2, b_repeat, "repeat(x, n) -> List   |   repeat(s: Str, n) -> Str\nn copies of x."),
    b!("collections", "each", 2, 2, b_each, "each(xs, f)\nCall f on every element (with (key, value) for maps)."),
    b!("collections", "to_list", 1, 1, b_to_list, "to_list(x) -> List\nThe elements of a range, string (characters), map (entries) or tuple."),
    b!("collections", "to_map", 1, 1, b_to_map, "to_map(pairs: List[(K, V)]) -> Map[K, V]\nBuild a map from (key, value) pairs."),
    b!("collections", "keys", 1, 1, b_keys, "keys(m: Map[K, V]) -> List[K]\nThe keys, in insertion order."),
    b!("collections", "values", 1, 1, b_values, "values(m: Map[K, V]) -> List[V]\nThe values, in insertion order."),
    b!("collections", "entries", 1, 1, b_entries, "entries(m: Map[K, V]) -> List[(K, V)]\nThe (key, value) pairs, in insertion order."),
    b!("collections", "has", 2, 2, b_has, "has(m: Map, k) -> Bool   |   has(s: Set, x) -> Bool\nWhether the map has the key (or the set the element)."),
    b!("collections", "to_set", 0, 1, b_to_set, "to_set(xs = []) -> Set[T]\nA set of the elements of a list, range, string or set: duplicates are dropped, and the elements keep the order in which they first appear."),
    b!("collections", "union", 2, 2, b_union, "union(s: Set[T], t: Set[T]) -> Set[T]\nThe elements in s or in t."),
    b!("collections", "intersection", 2, 2, b_intersection, "intersection(s: Set[T], t: Set[T]) -> Set[T]\nThe elements in both s and t."),
    b!("collections", "difference", 2, 2, b_difference, "difference(s: Set[T], t: Set[T]) -> Set[T]\nThe elements of s that are not in t."),
    b!("collections", "is_subset", 2, 2, b_is_subset, "is_subset(s: Set[T], t: Set[T]) -> Bool\nWhether every element of s is in t."),
    b!("collections", "merge", 2, 2, b_merge, "merge(m: Map, other: Map) -> Map\nAll entries of m and other (other wins on conflicts)."),
    b!("collections", "map_values", 2, 2, b_map_values, "map_values(m: Map[K, V], f: fn(V) -> W) -> Map[K, W]\nApply f to every value."),
    // ---- strings
    b!("strings", "split", 1, 3, b_split, "split(s: Str, sep: Str, limit: Int) -> List[Str]\nSplit on a separator (on whitespace, when sep is omitted), into at most `limit` pieces if given."),
    b!("strings", "split_once", 2, 2, b_split_once, "split_once(s: Str, sep: Str) -> Option[(Str, Str)]\nThe parts before and after the first occurrence of sep."),
    b!("strings", "strip_prefix", 2, 2, b_strip_prefix, "strip_prefix(s: Str, prefix: Str) -> Option[Str]\nThe rest of s after the prefix, or None if s does not start with it."),
    b!("strings", "strip_suffix", 2, 2, b_strip_suffix, "strip_suffix(s: Str, suffix: Str) -> Option[Str]\nThe rest of s before the suffix, or None if s does not end with it."),
    b!("strings", "lines", 1, 1, b_lines, "lines(s: Str) -> List[Str]\nSplit into lines."),
    b!("strings", "words", 1, 1, b_words, "words(s: Str) -> List[Str]\nSplit on whitespace."),
    b!("strings", "chars", 1, 1, b_chars, "chars(s: Str) -> List[Str]\nThe characters, as one-character strings."),
    b!("strings", "trim", 1, 1, b_trim, "trim(s: Str) -> Str\nRemove leading and trailing whitespace."),
    b!("strings", "trim_start", 1, 1, b_trim_start, "trim_start(s: Str) -> Str\nRemove leading whitespace."),
    b!("strings", "trim_end", 1, 1, b_trim_end, "trim_end(s: Str) -> Str\nRemove trailing whitespace."),
    b!("strings", "upper", 1, 1, b_upper, "upper(s: Str) -> Str\nUppercase."),
    b!("strings", "lower", 1, 1, b_lower, "lower(s: Str) -> Str\nLowercase."),
    b!("strings", "capitalize", 1, 1, b_capitalize, "capitalize(s: Str) -> Str\nUppercase the first character."),
    b!("strings", "starts_with", 2, 2, b_starts_with, "starts_with(s: Str, prefix: Str) -> Bool\nWhether s starts with prefix."),
    b!("strings", "ends_with", 2, 2, b_ends_with, "ends_with(s: Str, suffix: Str) -> Bool\nWhether s ends with suffix."),
    b!("strings", "replace", 3, 3, b_replace, "replace(s: Str, from: Str, to: Str) -> Str\nReplace every occurrence of from."),
    b!("strings", "pad_left", 2, 3, b_pad_left, "pad_left(s, width: Int, fill: Str = \" \") -> Str\nPad on the left to the given width."),
    b!("strings", "pad_right", 2, 3, b_pad_right, "pad_right(s, width: Int, fill: Str = \" \") -> Str\nPad on the right to the given width."),
    b!("strings", "is_digit", 1, 1, b_is_digit, "is_digit(s: Str) -> Bool\nWhether s is non-empty and all ASCII digits."),
    b!("strings", "is_alpha", 1, 1, b_is_alpha, "is_alpha(s: Str) -> Bool\nWhether s is non-empty and all letters."),
    b!("strings", "is_alnum", 1, 1, b_is_alnum, "is_alnum(s: Str) -> Bool\nWhether s is non-empty and all letters or digits."),
    b!("strings", "is_space", 1, 1, b_is_space, "is_space(s: Str) -> Bool\nWhether s is non-empty and all whitespace."),
    b!("strings", "is_upper", 1, 1, b_is_upper, "is_upper(s: Str) -> Bool\nWhether s has letters and they are all uppercase."),
    b!("strings", "is_lower", 1, 1, b_is_lower, "is_lower(s: Str) -> Bool\nWhether s has letters and they are all lowercase."),
    // ---- Option and Result
    b!("option", "unwrap", 1, 1, b_unwrap, "unwrap(x: Option[T] | Result[T, E]) -> T\nThe value inside Some/Ok; an error for None/Err."),
    b!("option", "expect", 2, 2, b_expect, "expect(x: Option[T] | Result[T, E], message: Str) -> T\nLike unwrap, with a custom error message."),
    b!("option", "unwrap_or", 2, 2, b_unwrap_or, "unwrap_or(x: Option[T] | Result[T, E], default: T) -> T\nThe value inside Some/Ok, or the default."),
    b!("option", "unwrap_or_else", 2, 2, b_unwrap_or_else, "unwrap_or_else(x, f: fn() -> T) -> T\nThe value inside Some/Ok, or f() (f(e) for Err(e))."),
    b!("option", "is_some", 1, 1, b_is_some, "is_some(x: Option[T]) -> Bool\nWhether x is Some."),
    b!("option", "is_none", 1, 1, b_is_none, "is_none(x: Option[T]) -> Bool\nWhether x is None."),
    b!("option", "is_ok", 1, 1, b_is_ok, "is_ok(x: Result[T, E]) -> Bool\nWhether x is Ok."),
    b!("option", "is_err", 1, 1, b_is_err, "is_err(x: Result[T, E]) -> Bool\nWhether x is Err."),
    b!("option", "and_then", 2, 2, b_and_then, "and_then(x, f: fn(T) -> Option[U] | Result[U, E])\nChain a computation that may fail."),
    b!("option", "map_err", 2, 2, b_map_err, "map_err(x: Result[T, E], f: fn(E) -> F) -> Result[T, F]\nTransform the error value."),
    b!("option", "ok_or", 2, 2, b_ok_or, "ok_or(x: Option[T], err: E) -> Result[T, E]\nSome(v) becomes Ok(v); None becomes Err(err)."),
    b!("option", "ok", 1, 1, b_ok, "ok(x: Result[T, E]) -> Option[T]\nOk(v) becomes Some(v); Err becomes None."),
    b!("option", "err", 1, 1, b_err, "err(x: Result[T, E]) -> Option[E]\nErr(e) becomes Some(e); Ok becomes None."),
    b!("option", "unwrap_err", 1, 1, b_unwrap_err, "unwrap_err(x: Result[T, E]) -> E\nThe error inside Err; an error for Ok."),
    b!("option", "collect_ok", 1, 1, b_collect_ok, "collect_ok(xs: List[Result[T, E]]) -> Result[List[T], E]\nOk with all the values, or the first Err."),
    b!("option", "collect_some", 1, 1, b_collect_some, "collect_some(xs: List[Option[T]]) -> Option[List[T]]\nSome with all the values, or None if any element is None."),
];

/// Parameter names of a built-in, read from its documented signature (the
/// longest non-variadic alternative). Empty for variadic and mutating built-ins.
/// The parameter names of each form of a built-in, read from its doc
/// signature (`remove(xs: List[T], i: Int)  |  remove(m: Map, k)`).
pub fn param_forms(idx: u16) -> &'static [Vec<String>] {
    static NAMES: std::sync::OnceLock<Vec<Vec<Vec<String>>>> = std::sync::OnceLock::new();
    let all = NAMES.get_or_init(|| BUILTINS.iter().map(|b| parse_param_forms(b.doc)).collect());
    &all[idx as usize]
}

/// The parameter names of a built-in's main (longest) form.
pub fn param_names(idx: u16) -> &'static [String] {
    param_forms(idx).iter().max_by_key(|f| f.len()).map(|f| f.as_slice()).unwrap_or(&[])
}

/// The form whose parameters include all the given names.
pub fn param_form_for(idx: u16, named: &[&str]) -> Option<&'static [String]> {
    param_forms(idx).iter().find(|f| named.iter().all(|n| f.iter().any(|x| x == n))).map(|f| f.as_slice())
}

/// Split at `|` outside brackets and quotes.
fn split_top(s: &str) -> Vec<&str> {
    let (mut depth, mut in_str, mut start) = (0i32, false, 0);
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' | '[' if !in_str => depth += 1,
            ')' | ']' if !in_str => depth -= 1,
            '|' if depth == 0 && !in_str => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn parse_param_forms(doc: &str) -> Vec<Vec<String>> {
    let first = doc.lines().next().unwrap_or("");
    let mut forms = Vec::new();
    for alt in split_top(first) {
        let alt = alt.trim();
        if alt.contains("...") || !alt.contains('(') {
            continue;
        }
        // `xs.swap!(i, j)`: the receiver is the first parameter.
        let recv = alt.split_once('.').filter(|(r, rest)| !r.contains('(') && rest.contains('(')).map(|(r, _)| r.trim().to_string());
        let open = alt.find('(').unwrap();
        let mut depth = 0;
        let mut close = None;
        let mut in_str = false;
        for (i, c) in alt[open..].char_indices() {
            match c {
                '"' => in_str = !in_str,
                '(' | '[' if !in_str => depth += 1,
                ')' | ']' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { continue };
        let inner = &alt[open + 1..close];
        let mut names: Vec<String> = recv.into_iter().collect();
        let mut depth = 0;
        let mut in_str = false;
        let mut cur = String::new();
        for c in inner.chars().chain(std::iter::once(',')) {
            match c {
                '"' => {
                    in_str = !in_str;
                    cur.push(c)
                }
                '[' | '(' if !in_str => {
                    depth += 1;
                    cur.push(c)
                }
                ']' | ')' if !in_str => {
                    depth -= 1;
                    cur.push(c)
                }
                ',' if depth == 0 && !in_str => {
                    let name: String = cur.trim().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if !name.is_empty() {
                        names.push(name);
                    }
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        if !names.is_empty() && !forms.contains(&names) {
            forms.push(names);
        }
    }
    forms
}

/// Combine positional and named arguments for a built-in into one positional list.
pub fn arrange_named(idx: u16, mut pos: Vec<Value>, named: Vec<(crate::types::Name, Value)>) -> Result<Vec<Value>, String> {
    let b = &BUILTINS[idx as usize];
    let given: Vec<&str> = named.iter().map(|(n, _)| &**n).collect();
    let Some(names) = param_form_for(idx, &given) else {
        let all = param_names(idx);
        if all.is_empty() {
            return Err(format!("built-in function `{}` does not take named arguments", b.name));
        }
        let unknown = given.iter().find(|n| !param_forms(idx).iter().any(|f| f.iter().any(|x| x == *n))).copied().unwrap_or(given[0]);
        let hint =
            crate::diagnostic::suggest(unknown, all.iter().map(|s| s.as_str())).map(|s| format!("; did you mean `{}`?", s)).unwrap_or_default();
        return Err(format!("`{}` has no parameter named `{}` (its parameters are: {}){}", b.name, unknown, all.join(", "), hint));
    };
    let mut slots: Vec<Option<Value>> = pos.drain(..).map(Some).collect();
    for (n, v) in named {
        let i = names.iter().position(|x| **x == *n).unwrap_or(0);
        if slots.len() <= i {
            slots.resize(i + 1, None);
        }
        if slots[i].is_some() {
            return Err(format!("argument `{}` is given twice", n));
        }
        slots[i] = Some(v);
    }
    let mut out = Vec::with_capacity(slots.len());
    for (i, s) in slots.into_iter().enumerate() {
        match s {
            Some(v) => out.push(v),
            None => return Err(format!("argument `{}` of `{}` is missing (built-ins cannot skip arguments)", names[i], b.name)),
        }
    }
    Ok(out)
}

pub fn builtin_index(name: &str) -> Option<usize> {
    BUILTINS.iter().position(|b| b.name == name)
}

// ============================================================ helpers

fn type_err(it: &Interp, f: &str, i: usize, expected: &str, got: &Value, sp: Span) -> Ctrl {
    let mut d = it.diag(sp, "E0200", format!("argument {} of `{}` must be {}, got {}", i + 1, f, expected, describe(got)));
    if let Value::Variant(v) = got {
        let what = match v.ty.id {
            OPTION_ID => "an Option",
            RESULT_ID => "a Result",
            _ => "",
        };
        if !what.is_empty() && !expected.contains("Option") && !expected.contains("Result") {
            d = d.help(format!("this is {}; get the value out first with `?`, `match`, or `.unwrap_or(default)`", what));
        }
    }
    it.fail(d)
}

fn int_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<i64> {
    match &a[i] {
        Value::Int(n) => Ok(*n),
        v => Err(type_err(it, f, i, "an Int", v, sp)),
    }
}

fn num_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<f64> {
    match &a[i] {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        v => Err(type_err(it, f, i, "a number", v, sp)),
    }
}

fn str_arg<'a>(it: &Interp, a: &'a [Value], i: usize, f: &str, sp: Span) -> R<&'a str> {
    match &a[i] {
        Value::Str(s) => Ok(s.as_str()),
        v => Err(type_err(it, f, i, "a Str", v, sp)),
    }
}

fn fn_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<Value> {
    if a[i].is_callable() {
        Ok(a[i].clone())
    } else {
        Err(type_err(it, f, i, "a function", &a[i], sp))
    }
}

fn usize_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<usize> {
    let n = int_arg(it, a, i, f, sp)?;
    if n < 0 {
        return Err(it.err(sp, "E0216", format!("argument {} of `{}` must not be negative, got {}", i + 1, f, n)));
    }
    Ok(n as usize)
}

/// The elements of an iterable argument (moving out of the list when unshared).
fn items(it: &mut Interp, v: Value, f: &str, i: usize, sp: Span) -> R<Vec<Value>> {
    match v {
        Value::List(_) | Value::Tuple(_) | Value::Range(_) | Value::Str(_) | Value::Map(_) | Value::Set(_) => it.iter_values(v, sp),
        other => Err(type_err(it, f, i, "a list, range, string, map, set or tuple", &other, sp)),
    }
}

fn take_arg(a: &mut [Value], i: usize) -> Value {
    std::mem::take(&mut a[i])
}

fn bool_result(it: &mut Interp, f: &Value, args: Vec<Value>, sp: Span, name: &str) -> R<bool> {
    let r = it.call(f, args, sp)?;
    to_bool(it, r, sp, name)
}

/// As `bool_result`, for one argument (the common case).
fn bool_result1(it: &mut Interp, f: &Value, x: Value, sp: Span, name: &str) -> R<bool> {
    let r = it.call1(f, x, sp)?;
    to_bool(it, r, sp, name)
}

fn to_bool(it: &Interp, r: Value, sp: Span, name: &str) -> R<bool> {
    match r {
        Value::Bool(b) => Ok(b),
        other => Err(it.err(sp, "E0209", format!("the function passed to `{}` must return a Bool, but it returned {}", name, describe(&other)))),
    }
}

/// Call f with one element; for map entries (k, v) the function may take two arguments.
fn call_entry(it: &mut Interp, f: &Value, x: Value, two: bool, sp: Span) -> R {
    if two {
        if let Value::Tuple(t) = &x {
            if t.len() == 2 {
                return it.call(f, vec![t[0].clone(), t[1].clone()], sp);
            }
        }
    }
    it.call1(f, x, sp)
}

/// Call a predicate on one element; for map entries `(k, v)` the predicate
/// may take two arguments.
fn entry_pred(it: &mut Interp, f: &Value, x: Value, two: bool, sp: Span, name: &str) -> R<bool> {
    if two {
        if let Value::Tuple(t) = &x {
            if t.len() == 2 {
                return bool_result(it, f, vec![t[0].clone(), t[1].clone()], sp, name);
            }
        }
    }
    bool_result1(it, f, x, sp, name)
}

/// Functions that pick out or reorder the characters of a Str give a Str.
fn restring(was_str: bool, xs: Vec<Value>) -> Value {
    if was_str {
        let mut out = String::new();
        for x in &xs {
            if let Value::Str(c) = x {
                out.push_str(c);
            }
        }
        Value::str(out)
    } else {
        Value::list(xs)
    }
}

fn wants_two(f: &Value) -> bool {
    match f {
        Value::Func(c) => c.def.params.len() == 2,
        _ => false,
    }
}

fn merge_sort<T>(v: Vec<T>, cmp: &mut dyn FnMut(&T, &T) -> R<Ordering>) -> R<Vec<T>> {
    let n = v.len();
    if n <= 1 {
        return Ok(v);
    }
    // insertion sort for small inputs
    if n <= 16 {
        let mut v = v;
        for i in 1..n {
            let mut j = i;
            while j > 0 && cmp(&v[j - 1], &v[j])? == Ordering::Greater {
                v.swap(j - 1, j);
                j -= 1;
            }
        }
        return Ok(v);
    }
    let mut v = v;
    let right = v.split_off(n / 2);
    let left = merge_sort(v, cmp)?;
    let right = merge_sort(right, cmp)?;
    let mut out = Vec::with_capacity(n);
    let mut li = left.into_iter().peekable();
    let mut ri = right.into_iter().peekable();
    loop {
        match (li.peek(), ri.peek()) {
            (Some(a), Some(b)) => {
                if cmp(a, b)? == Ordering::Greater {
                    out.push(ri.next().unwrap());
                } else {
                    out.push(li.next().unwrap());
                }
            }
            (Some(_), None) => out.push(li.next().unwrap()),
            (None, Some(_)) => out.push(ri.next().unwrap()),
            (None, None) => break,
        }
    }
    Ok(out)
}

fn cmp_values(it: &Interp, a: &Value, b: &Value, sp: Span) -> R<Ordering> {
    compare(a, b).ok_or_else(|| it.err(sp, "E0211", format!("cannot compare {} with {}", describe(a), describe(b))))
}

fn sort_values(it: &Interp, xs: Vec<Value>, sp: Span) -> R<Vec<Value>> {
    merge_sort(xs, &mut |a, b| cmp_values(it, a, b, sp))
}

fn sort_by_key(it: &mut Interp, xs: Vec<Value>, key: &Value, sp: Span) -> R<Vec<Value>> {
    let mut keys = Vec::with_capacity(xs.len());
    for x in &xs {
        keys.push(it.call1(key, x.clone(), sp)?);
    }
    // Sort the positions by key (stable), then pick the elements in order.
    let itr: &Interp = it;
    let order = merge_sort((0..xs.len()).collect(), &mut |a: &usize, b: &usize| cmp_values(itr, &keys[*a], &keys[*b], sp))?;
    let mut xs: Vec<Option<Value>> = xs.into_iter().map(Some).collect();
    Ok(order.into_iter().map(|i| xs[i].take().unwrap_or_default()).collect())
}

fn ordering_of(it: &Interp, v: &Value, sp: Span) -> R<Ordering> {
    match v {
        Value::Variant(vv) if vv.ty.id == ORDERING_ID => Ok(match vv.tag {
            0 => Ordering::Less,
            1 => Ordering::Equal,
            _ => Ordering::Greater,
        }),
        Value::Int(n) => Ok(n.cmp(&0)),
        other => Err(it.err(
            sp,
            "E0200",
            format!("a comparison function must return an Ordering (Less, Equal, Greater) or an Int, got {}", describe(other)),
        )),
    }
}

fn list_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<Rc<List>> {
    match &a[i] {
        Value::List(xs) => Ok(xs.clone()),
        v => Err(type_err(it, f, i, "a List", v, sp)),
    }
}

fn map_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<Rc<MapVal>> {
    match &a[i] {
        Value::Map(m) => Ok(m.clone()),
        v => Err(type_err(it, f, i, "a Map", v, sp)),
    }
}

fn io_err(it: &Interp, path: &str, e: std::io::Error) -> Value {
    it.err_val(Value::str(format!("{}: {}", path, e)))
}

// ============================================================ io

fn join_display(args: &[Value]) -> String {
    let mut s = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        write_value(&mut s, a, false);
    }
    s
}

fn b_print(it: &mut Interp, a: Vec<Value>, _: Span) -> R {
    let mut s = join_display(&a);
    s.push('\n');
    it.write_out(&s);
    Ok(Value::Unit)
}

fn b_write(it: &mut Interp, a: Vec<Value>, _: Span) -> R {
    let s = join_display(&a);
    it.write_out(&s);
    Ok(Value::Unit)
}

fn b_eprint(it: &mut Interp, a: Vec<Value>, _: Span) -> R {
    if it.silent {
        return Ok(Value::Unit);
    }
    let mut s = join_display(&a);
    s.push('\n');
    it.write_err(&s);
    Ok(Value::Unit)
}

fn b_input(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if let Some(p) = a.first() {
        let p = match p {
            Value::Str(s) => s.to_string(),
            v => return Err(type_err(it, "input", 0, "a Str", v, sp)),
        };
        it.write_out(&p);
    }
    it.flush();
    let mut bytes = Vec::new();
    let _ = it.read_input_line(&mut bytes);
    let line = String::from_utf8_lossy(&bytes).to_string();
    let line = line.strip_suffix('\n').unwrap_or(&line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    Ok(Value::str(line))
}

fn b_read_line(it: &mut Interp, _: Vec<Value>, _: Span) -> R {
    it.flush();
    let mut bytes = Vec::new();
    match it.read_input_line(&mut bytes) {
        Ok(0) | Err(_) => Ok(it.none()),
        Ok(_) => {
            let line = String::from_utf8_lossy(&bytes).to_string();
            let l = line.strip_suffix('\n').unwrap_or(&line);
            let l = l.strip_suffix('\r').unwrap_or(l);
            Ok(it.some(Value::str(l)))
        }
    }
}

fn b_read_stdin(it: &mut Interp, _: Vec<Value>, _: Span) -> R {
    it.flush();
    let mut bytes = Vec::new();
    let _ = it.read_input_all(&mut bytes);
    Ok(Value::str(String::from_utf8_lossy(&bytes).to_string()))
}

fn b_read_file(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let p = str_arg(it, &a, 0, "read_file", sp)?;
    Ok(match std::fs::read_to_string(p) {
        Ok(s) => it.ok(Value::str(s)),
        Err(e) => io_err(it, p, e),
    })
}

fn b_write_file(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let p = str_arg(it, &a, 0, "write_file", sp)?;
    let t = str_arg(it, &a, 1, "write_file", sp)?;
    Ok(match std::fs::write(p, t) {
        Ok(()) => it.ok(Value::Unit),
        Err(e) => io_err(it, p, e),
    })
}

fn b_append_file(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let p = str_arg(it, &a, 0, "append_file", sp)?;
    let t = str_arg(it, &a, 1, "append_file", sp)?;
    let r = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| f.write_all(t.as_bytes()));
    Ok(match r {
        Ok(()) => it.ok(Value::Unit),
        Err(e) => io_err(it, p, e),
    })
}

fn b_file_exists(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let p = str_arg(it, &a, 0, "file_exists", sp)?;
    Ok(Value::Bool(std::path::Path::new(p).exists()))
}

fn b_list_dir(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let p = str_arg(it, &a, 0, "list_dir", sp)?;
    match std::fs::read_dir(p) {
        Ok(rd) => {
            let mut names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
            names.sort();
            Ok(it.ok(Value::list(names.into_iter().map(Value::str).collect())))
        }
        Err(e) => Ok(io_err(it, p, e)),
    }
}

fn b_args(it: &mut Interp, _: Vec<Value>, _: Span) -> R {
    Ok(Value::list(it.args.iter().map(|s| Value::str(s.as_str())).collect()))
}

fn b_env(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let n = str_arg(it, &a, 0, "env", sp)?;
    Ok(it.option(std::env::var(n).ok().map(Value::str)))
}

fn b_exit(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let code = if a.is_empty() { 0 } else { int_arg(it, &a, 0, "exit", sp)? };
    if !(0..=255).contains(&code) {
        return Err(it.err(sp, "E0216", format!("exit codes must be between 0 and 255, got {}", code)));
    }
    if it.test_mode {
        return Err(it.fail(
            it.diag(sp, "E0220", format!("`exit({})` was called while running tests", code))
                .help("tests run in the same process as the test runner; return or assert instead of exiting"),
        ));
    }
    it.flush();
    if it.embedded {
        it.exit_code = Some(code as i32);
        return Err(Ctrl::Exit(code as i32));
    }
    std::process::exit(code as i32);
}

fn b_time(_: &mut Interp, _: Vec<Value>, _: Span) -> R {
    Ok(Value::Float(crate::platform::now_seconds()))
}

/// Start the clock that `clock()` measures from.
pub fn start_clock() {
    crate::platform::monotonic_seconds();
}

fn b_clock(it: &mut Interp, _: Vec<Value>, _: Span) -> R {
    Ok(Value::Float(crate::platform::monotonic_seconds() - it.clock_start))
}

fn b_flush(it: &mut Interp, _: Vec<Value>, _: Span) -> R {
    it.flush();
    Ok(Value::Unit)
}

fn b_sleep(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = num_arg(it, &a, 0, "sleep", sp)?;
    let Ok(d) = std::time::Duration::try_from_secs_f64(s) else {
        return Err(it.err(sp, "E0216", format!("`sleep` needs a non-negative, finite number of seconds, got {}", format_float(s))));
    };
    it.flush();
    crate::platform::sleep(d);
    Ok(Value::Unit)
}

// ============================================================ core

fn b_panic(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let msg = a.first().map(display).unwrap_or_else(|| "explicit panic".into());
    Err(it.err(sp, "E0217", format!("panic: {}", msg)))
}

fn b_todo(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let msg = a.first().map(|m| format!(": {}", display(m))).unwrap_or_default();
    Err(it.err(sp, "E0218", format!("not yet implemented{}", msg)))
}

fn b_dbg(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    if !it.silent {
        let loc = if (sp.file as usize) < it.ctx.sm.files.len() { it.ctx.sm.location(sp) } else { "?".into() };
        let src = if (sp.file as usize) < it.ctx.sm.files.len() { it.ctx.sm.snippet(sp).to_string() } else { String::new() };
        it.write_err(&format!("[{}] {} = {}\n", loc, src, repr(&v)));
    }
    Ok(v)
}

fn b_catch(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 0, "catch", sp)?;
    let depth = it.stack.len();
    it.catching += 1;
    let r = it.call(&f, vec![], sp);
    it.catching -= 1;
    match r {
        Ok(v) => Ok(it.ok(v)),
        Err(Ctrl::Error(d)) => {
            it.stack.truncate(depth);
            Ok(it.err_val(Value::str(d.message)))
        }
        Err(other) => Err(other),
    }
}

fn b_type_of(_: &mut Interp, a: Vec<Value>, _: Span) -> R {
    Ok(Value::str(type_name(&a[0])))
}

// ============================================================ conversion

fn b_str(_: &mut Interp, a: Vec<Value>, _: Span) -> R {
    match &a[0] {
        Value::Str(_) => Ok(a[0].clone()),
        v => Ok(Value::str(display(v))),
    }
}

fn b_repr(_: &mut Interp, a: Vec<Value>, _: Span) -> R {
    Ok(Value::str(repr(&a[0])))
}

fn b_int(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match &a[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Float(f) => {
            if !f.is_finite() || f.abs() >= 9.223372036854776e18 {
                return Err(it.err(sp, "E0207", format!("{} cannot be converted to Int", format_float(*f))));
            }
            Ok(Value::Int(f.trunc() as i64))
        }
        Value::Bool(b) => Ok(Value::Int(*b as i64)),
        Value::Str(s) => match s.trim().replace('_', "").parse::<i64>() {
            Ok(n) => Ok(Value::Int(n)),
            Err(_) => Err(it.fail(
                it.diag(sp, "E0216", format!("cannot convert {} to Int", repr(&a[0])))
                    .help("use `parse_int(s)`, which returns None instead of failing"),
            )),
        },
        v => Err(type_err(it, "int", 0, "a Float, Str or Bool", v, sp)),
    }
}

fn b_float(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match &a[0] {
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Str(s) => match parse_float_text(&s.trim().replace('_', "")) {
            Some(n) => Ok(Value::Float(n)),
            None => Err(it.fail(
                it.diag(sp, "E0216", format!("cannot convert {} to Float", repr(&a[0])))
                    .help("use `parse_float(s)`, which returns None instead of failing"),
            )),
        },
        v => Err(type_err(it, "float", 0, "an Int or Str", v, sp)),
    }
}

fn b_parse_int(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "parse_int", sp)?.trim();
    let base = if a.len() == 2 { int_arg(it, &a, 1, "parse_int", sp)? } else { 10 };
    if !(2..=36).contains(&base) {
        return Err(it.err(sp, "E0216", format!("`parse_int` base must be between 2 and 36, got {}", base)));
    }
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let prefix = match base {
        16 => ["0x", "0X"],
        8 => ["0o", "0O"],
        2 => ["0b", "0B"],
        _ => ["", ""],
    };
    let digits = prefix.iter().filter(|p| !p.is_empty()).find_map(|p| digits.strip_prefix(p)).unwrap_or(digits);
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return Ok(it.none());
    }
    let Some(digits) = without_digit_underscores(digits, |c| c.is_ascii_alphanumeric()) else { return Ok(it.none()) };
    let digits = digits.as_str();
    // Parse the magnitude as u64 so that min_int is representable.
    let n = u64::from_str_radix(digits, base as u32).ok().and_then(|m| if neg { 0i64.checked_sub_unsigned(m) } else { i64::try_from(m).ok() });
    Ok(it.option(n.map(Value::Int)))
}

fn b_wrapping_add(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(int_arg(it, &a, 0, "wrapping_add", sp)?.wrapping_add(int_arg(it, &a, 1, "wrapping_add", sp)?)))
}

fn b_wrapping_sub(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(int_arg(it, &a, 0, "wrapping_sub", sp)?.wrapping_sub(int_arg(it, &a, 1, "wrapping_sub", sp)?)))
}

fn b_wrapping_mul(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(int_arg(it, &a, 0, "wrapping_mul", sp)?.wrapping_mul(int_arg(it, &a, 1, "wrapping_mul", sp)?)))
}

fn b_hash(_: &mut Interp, a: Vec<Value>, _: Span) -> R {
    Ok(Value::Int(hash_of(&a[0]) as i64))
}

/// A Float from text: `inf` and `-inf` (as `str` prints them) are allowed,
/// but not NaN, or a number too large for a Float (`1e999`).
/// `t` without its digit-group underscores (`1_000`), or None if an
/// underscore is anywhere but between two digits (`_1`, `1_`, `1__0`).
fn without_digit_underscores(t: &str, digit: fn(char) -> bool) -> Option<String> {
    let cs: Vec<char> = t.chars().collect();
    for (i, c) in cs.iter().enumerate() {
        if *c == '_' && !(i > 0 && i + 1 < cs.len() && digit(cs[i - 1]) && digit(cs[i + 1])) {
            return None;
        }
    }
    Some(cs.into_iter().filter(|c| *c != '_').collect())
}

fn parse_float_text(t: &str) -> Option<f64> {
    let f = t.parse::<f64>().ok()?;
    let word = t.trim_start_matches(['+', '-']);
    (f.is_finite() || word.eq_ignore_ascii_case("inf") || word.eq_ignore_ascii_case("infinity")).then_some(f)
}

fn b_parse_float(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "parse_float", sp)?;
    let t = without_digit_underscores(s.trim(), |c| c.is_ascii_digit());
    Ok(it.option(t.and_then(|t| parse_float_text(&t)).map(Value::Float)))
}

fn b_ord(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "ord", sp)?;
    let mut cs = s.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) => Ok(Value::Int(c as i64)),
        _ => Err(it.err(sp, "E0216", format!("`ord` needs a one-character string, got {}", repr(&a[0])))),
    }
}

fn b_chr(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let n = int_arg(it, &a, 0, "chr", sp)?;
    match u32::try_from(n).ok().and_then(char::from_u32) {
        Some(c) => Ok(Value::str(c.to_string())),
        None => Err(it.err(sp, "E0216", format!("{} is not a valid Unicode code point", n))),
    }
}

// ============================================================ math

fn b_abs(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match &a[0] {
        Value::Int(n) => n.checked_abs().map(Value::Int).ok_or_else(|| it.err(sp, "E0207", "integer overflow in abs")),
        Value::Float(f) => Ok(Value::Float(f.abs())),
        v => Err(type_err(it, "abs", 0, "a number", v, sp)),
    }
}

fn float_fn(it: &mut Interp, a: &[Value], sp: Span, name: &str, f: fn(f64) -> f64, domain: Option<(&str, fn(f64) -> bool)>) -> R {
    let x = num_arg(it, a, 0, name, sp)?;
    if let Some((msg, ok)) = domain {
        if !ok(x) {
            return Err(it.err(sp, "E0216", format!("`{}` is undefined for {}: {}", name, format_float(x), msg)));
        }
    }
    Ok(Value::Float(f(x)))
}

fn b_sqrt(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "sqrt", f64::sqrt, Some(("the argument must not be negative", |x| x >= 0.0)))
}
fn b_exp(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "exp", f64::exp, None)
}
fn b_ln(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "ln", f64::ln, Some(("the argument must be positive", |x| x > 0.0)))
}
fn b_log2(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "log2", f64::log2, Some(("the argument must be positive", |x| x > 0.0)))
}
fn b_log10(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "log10", f64::log10, Some(("the argument must be positive", |x| x > 0.0)))
}
fn b_sin(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "sin", f64::sin, None)
}
fn b_cos(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "cos", f64::cos, None)
}
fn b_tan(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "tan", f64::tan, None)
}
fn b_asin(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "asin", f64::asin, Some(("the argument must be in [-1, 1]", |x| (-1.0..=1.0).contains(&x))))
}
fn b_acos(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "acos", f64::acos, Some(("the argument must be in [-1, 1]", |x| (-1.0..=1.0).contains(&x))))
}
fn b_atan(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    float_fn(it, &a, sp, "atan", f64::atan, None)
}

fn b_atan2(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let y = num_arg(it, &a, 0, "atan2", sp)?;
    let x = num_arg(it, &a, 1, "atan2", sp)?;
    Ok(Value::Float(y.atan2(x)))
}

fn b_hypot(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = num_arg(it, &a, 0, "hypot", sp)?;
    let y = num_arg(it, &a, 1, "hypot", sp)?;
    Ok(Value::Float(x.hypot(y)))
}

fn b_pow(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = num_arg(it, &a, 0, "pow", sp)?;
    let y = num_arg(it, &a, 1, "pow", sp)?;
    if let Some(m) = power_error(x, y) {
        return Err(it.err(sp, "E0216", m));
    }
    Ok(Value::Float(x.powf(y)))
}

/// Why `x` to the power `y` has no value: a negative base with a fractional
/// exponent, or zero to a negative power (a division by zero).
pub fn power_error(x: f64, y: f64) -> Option<String> {
    if x < 0.0 && y.is_finite() && y.fract() != 0.0 {
        return Some(negative_power(x, y));
    }
    if x == 0.0 && y < 0.0 {
        return Some(format!("0 to the power {} divides by zero", format_float(y)));
    }
    None
}

/// The message for a negative number raised to a fractional power, which
/// has no real value (rather than a silent NaN).
pub fn negative_power(x: f64, y: f64) -> String {
    format!(
        "{} to the power {} is not a real number: a negative base needs a whole-number exponent (for a cube root of a negative x, use -pow(-x, 1.0 / 3.0))",
        format_float(x),
        format_float(y)
    )
}

fn b_log(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = num_arg(it, &a, 0, "log", sp)?;
    let b = num_arg(it, &a, 1, "log", sp)?;
    if x <= 0.0 || b <= 0.0 || b == 1.0 {
        return Err(it.err(sp, "E0216", format!("log({}, {}) is undefined", format_float(x), format_float(b))));
    }
    Ok(Value::Float(x.ln() / b.ln()))
}

fn to_int_checked(it: &Interp, f: f64, sp: Span, name: &str) -> R {
    if !f.is_finite() || f.abs() >= 9.223372036854776e18 {
        return Err(it.err(sp, "E0207", format!("`{}` result {} does not fit in an Int", name, format_float(f))));
    }
    Ok(Value::Int(f as i64))
}

fn b_floor(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if let Value::Int(n) = a[0] {
        return Ok(Value::Int(n));
    }
    let x = num_arg(it, &a, 0, "floor", sp)?;
    to_int_checked(it, x.floor(), sp, "floor")
}

fn b_ceil(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if let Value::Int(n) = a[0] {
        return Ok(Value::Int(n));
    }
    let x = num_arg(it, &a, 0, "ceil", sp)?;
    to_int_checked(it, x.ceil(), sp, "ceil")
}

fn b_trunc(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if let Value::Int(n) = a[0] {
        return Ok(Value::Int(n));
    }
    let x = num_arg(it, &a, 0, "trunc", sp)?;
    to_int_checked(it, x.trunc(), sp, "trunc")
}

fn b_round(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if a.len() == 2 {
        let x = num_arg(it, &a, 0, "round", sp)?;
        let d = int_arg(it, &a, 1, "round", sp)?;
        if (0..=340).contains(&d) && x.is_finite() {
            // Decimal rounding of the exact value, as `fixed` does.
            return Ok(Value::Float(crate::value::format_fixed(x, d as usize).parse().unwrap_or(x)));
        }
        let m = 10f64.powi(d.clamp(-300, 300) as i32);
        return Ok(Value::Float((x * m).round() / m));
    }
    if let Value::Int(n) = a[0] {
        return Ok(Value::Int(n));
    }
    let x = num_arg(it, &a, 0, "round", sp)?;
    to_int_checked(it, x.round(), sp, "round")
}

fn b_sign(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match &a[0] {
        Value::Int(n) => Ok(Value::Int(n.signum())),
        Value::Float(f) => Ok(Value::Int(if *f > 0.0 {
            1
        } else if *f < 0.0 {
            -1
        } else {
            0
        })),
        v => Err(type_err(it, "sign", 0, "a number", v, sp)),
    }
}

fn b_clamp(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if cmp_values(it, &a[1], &a[2], sp)? == Ordering::Greater {
        return Err(it.err(sp, "E0216", format!("`clamp` needs lo <= hi, got lo = {} and hi = {}", repr(&a[1]), repr(&a[2]))));
    }
    if cmp_values(it, &a[0], &a[1], sp)? == Ordering::Less {
        return Ok(a[1].clone());
    }
    if cmp_values(it, &a[0], &a[2], sp)? == Ordering::Greater {
        return Ok(a[2].clone());
    }
    Ok(a[0].clone())
}

fn gcd(a: i64, b: i64) -> u64 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn b_gcd(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = int_arg(it, &a, 0, "gcd", sp)?;
    let y = int_arg(it, &a, 1, "gcd", sp)?;
    i64::try_from(gcd(x, y)).map(Value::Int).map_err(|_| it.err(sp, "E0207", format!("integer overflow in gcd({}, {})", x, y)))
}

fn b_lcm(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = int_arg(it, &a, 0, "lcm", sp)?;
    let y = int_arg(it, &a, 1, "lcm", sp)?;
    if x == 0 || y == 0 {
        return Ok(Value::Int(0));
    }
    let l = (x.unsigned_abs() as u128 / gcd(x, y) as u128) * y.unsigned_abs() as u128;
    i64::try_from(l).map(Value::Int).map_err(|_| it.err(sp, "E0207", format!("integer overflow in lcm({}, {})", x, y)))
}

fn b_is_nan(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Bool(num_arg(it, &a, 0, "is_nan", sp)?.is_nan()))
}

fn b_fixed(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let d = usize_arg(it, &a, 1, "fixed", sp)?.min(100);
    if let Value::Int(n) = a[0] {
        return Ok(Value::str(if d == 0 { n.to_string() } else { format!("{}.{}", n, "0".repeat(d)) }));
    }
    let x = num_arg(it, &a, 0, "fixed", sp)?;
    Ok(Value::str(format_fixed(x, d)))
}

fn b_bit_and(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(int_arg(it, &a, 0, "bit_and", sp)? & int_arg(it, &a, 1, "bit_and", sp)?))
}
fn b_bit_or(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(int_arg(it, &a, 0, "bit_or", sp)? | int_arg(it, &a, 1, "bit_or", sp)?))
}
fn b_bit_xor(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(int_arg(it, &a, 0, "bit_xor", sp)? ^ int_arg(it, &a, 1, "bit_xor", sp)?))
}
fn b_bit_not(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(!int_arg(it, &a, 0, "bit_not", sp)?))
}
fn b_shl(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = int_arg(it, &a, 0, "shl", sp)?;
    let n = int_arg(it, &a, 1, "shl", sp)?;
    if !(0..64).contains(&n) {
        return Err(it.err(sp, "E0216", format!("shift amount must be in 0..64, got {}", n)));
    }
    let r = x.wrapping_shl(n as u32);
    if r >> n != x {
        return Err(it.fail(
            it.diag(sp, "E0207", format!("integer overflow: shl({}, {})", x, n))
                .help("to drop the bits shifted out (as hashes and random number generators do), use `wrapping_shl`"),
        ));
    }
    Ok(Value::Int(r))
}

fn shift_amount(it: &mut Interp, a: &[Value], sp: Span, name: &str) -> R<u32> {
    let n = int_arg(it, a, 1, name, sp)?;
    if !(0..64).contains(&n) {
        return Err(it.err(sp, "E0216", format!("shift amount must be in 0..64, got {}", n)));
    }
    Ok(n as u32)
}

fn b_wrapping_shl(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = int_arg(it, &a, 0, "wrapping_shl", sp)?;
    let n = shift_amount(it, &a, sp, "wrapping_shl")?;
    Ok(Value::Int(x.wrapping_shl(n)))
}

fn b_shr_logical(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = int_arg(it, &a, 0, "shr_logical", sp)?;
    let n = shift_amount(it, &a, sp, "shr_logical")?;
    Ok(Value::Int(((x as u64) >> n) as i64))
}
fn b_shr(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let x = int_arg(it, &a, 0, "shr", sp)?;
    let n = int_arg(it, &a, 1, "shr", sp)?;
    if !(0..64).contains(&n) {
        return Err(it.err(sp, "E0216", format!("shift amount must be in 0..64, got {}", n)));
    }
    Ok(Value::Int(x >> n))
}

fn b_seed(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let n = int_arg(it, &a, 0, "seed", sp)?;
    it.rng = crate::interp::Rng::new(n as u64);
    Ok(Value::Unit)
}

fn b_random(it: &mut Interp, _: Vec<Value>, _: Span) -> R {
    Ok(Value::Float(it.rng.float()))
}

fn b_random_int(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let lo = int_arg(it, &a, 0, "random_int", sp)?;
    let hi = int_arg(it, &a, 1, "random_int", sp)?;
    if hi < lo {
        return Err(it.err(sp, "E0216", format!("`random_int` needs lo <= hi, got {} and {}", lo, hi)));
    }
    Ok(Value::Int(it.rng.range(lo, hi)))
}

fn b_shuffle(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let mut xs = items(it, v, "shuffle", 0, sp)?;
    for i in (1..xs.len()).rev() {
        let j = it.rng.below(i + 1);
        xs.swap(i, j);
    }
    Ok(Value::list(xs))
}

fn b_choice(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let xs = list_arg(it, &a, 0, "choice", sp)?;
    if xs.is_empty() {
        return Ok(it.none());
    }
    let i = it.rng.below(xs.len());
    Ok(it.some(xs[i].clone()))
}

// ============================================================ comparison

fn b_compare(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let o = cmp_values(it, &a[0], &a[1], sp)?;
    Ok(it.ordering(o))
}

fn extreme(it: &mut Interp, mut a: Vec<Value>, sp: Span, want: Ordering, name: &str) -> R {
    if a.len() == 1 {
        let v = take_arg(&mut a, 0);
        let xs = items(it, v, name, 0, sp)?;
        let mut best: Option<Value> = None;
        for x in xs {
            best = Some(match best {
                None => x,
                Some(b) => {
                    if cmp_values(it, &x, &b, sp)? == want {
                        x
                    } else {
                        b
                    }
                }
            });
        }
        return Ok(it.option(best));
    }
    let mut best = a[0].clone();
    for x in a.into_iter().skip(1) {
        if cmp_values(it, &x, &best, sp)? == want {
            best = x;
        }
    }
    Ok(best)
}

fn b_min(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    extreme(it, a, sp, Ordering::Less, "min")
}

fn b_max(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    extreme(it, a, sp, Ordering::Greater, "max")
}

// ============================================================ collections

fn b_len(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Int(match &a[0] {
        Value::Str(s) => s.char_len() as i64,
        Value::List(xs) | Value::Tuple(xs) => xs.len() as i64,
        Value::Map(m) | Value::Set(m) => m.len() as i64,
        Value::Range(r) => match r.len_u128() {
            Some(n) => match i64::try_from(n) {
                Ok(n) => n,
                Err(_) => return Err(it.err(sp, "E0207", "integer overflow: the range has more than max_int elements")),
            },
            None => return Err(it.err(sp, "E0216", "an unbounded range has no length")),
        },
        Value::Record(r) => r.values.len() as i64,
        v => return Err(type_err(it, "len", 0, "a collection or string", v, sp)),
    }))
}

fn b_is_empty(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Bool(match &a[0] {
        Value::Str(s) => s.is_empty(),
        Value::List(xs) | Value::Tuple(xs) => xs.is_empty(),
        Value::Map(m) | Value::Set(m) => m.is_empty(),
        Value::Range(r) => r.len() == Some(0),
        v => return Err(type_err(it, "is_empty", 0, "a collection or string", v, sp)),
    }))
}

fn b_range(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let (start, end) =
        if a.len() == 1 { (0, int_arg(it, &a, 0, "range", sp)?) } else { (int_arg(it, &a, 0, "range", sp)?, int_arg(it, &a, 1, "range", sp)?) };
    if a.len() == 3 {
        let step = int_arg(it, &a, 2, "range", sp)?;
        if step == 0 {
            return Err(it.err(sp, "E0216", "`range` step must not be zero"));
        }
        let mut out = Vec::new();
        let mut i = start;
        while (step > 0 && i < end) || (step < 0 && i > end) {
            out.push(Value::Int(i));
            if out.len() > 100_000_000 {
                return Err(it.err(sp, "E0216", "range is too large"));
            }
            match i.checked_add(step) {
                Some(n) => i = n,
                None => break,
            }
        }
        return Ok(Value::list(out));
    }
    Ok(Value::Range(Rc::new(RangeVal { start, end: Some(end as i128) })))
}

fn owned_list(it: &Interp, v: Value, f: &str, i: usize, sp: Span) -> R<Vec<Value>> {
    match v {
        Value::List(xs) => Ok(list_into_vec(xs)),
        other => Err(type_err(it, f, i, "a List", &other, sp)),
    }
}

fn b_push(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let x = take_arg(&mut a, 1);
    let v = take_arg(&mut a, 0);
    let mut xs = owned_list(it, v, "push", 0, sp)?;
    xs.push(x);
    Ok(Value::list(xs))
}

fn m_push(it: &mut Interp, t: &mut Value, mut a: Vec<Value>, sp: Span) -> R {
    match t {
        Value::List(xs) => {
            Rc::make_mut(xs).push(take_arg(&mut a, 0));
            Ok(Value::Unit)
        }
        v => Err(type_err(it, "push!", 0, "a List", v, sp)),
    }
}

fn m_pop(it: &mut Interp, t: &mut Value, _: Vec<Value>, sp: Span) -> R {
    match t {
        Value::List(xs) => {
            let x = Rc::make_mut(xs).pop();
            Ok(it.option(x))
        }
        v => Err(type_err(it, "pop!", 0, "a List", v, sp)),
    }
}

fn insert_index(it: &Interp, i: i64, len: usize, sp: Span) -> R<usize> {
    let j = if i < 0 { i + len as i64 + 1 } else { i };
    if j < 0 || j > len as i64 {
        return Err(it.err(sp, "E0204", format!("insert index {} is out of bounds for a list of length {}", i, len)));
    }
    Ok(j as usize)
}

fn b_insert(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    // `s.insert(x)` on a set.
    if let (Value::Set(_), 2) = (&a[0], a.len()) {
        let Value::Set(mut m) = take_arg(&mut a, 0) else { unreachable!() };
        Rc::make_mut(&mut m).insert(take_arg(&mut a, 1), Value::Unit);
        return Ok(Value::Set(m));
    }
    if a.len() < 3 {
        return Err(it.err(sp, "E0201", "`insert` takes a position (for a list) or a key (for a map) and a value; a set takes just the value"));
    }
    let x = take_arg(&mut a, 2);
    match take_arg(&mut a, 0) {
        Value::List(xs) => {
            let i = int_arg(it, &a, 1, "insert", sp)?;
            let mut xs = list_into_vec(xs);
            let j = insert_index(it, i, xs.len(), sp)?;
            xs.insert(j, x);
            Ok(Value::list(xs))
        }
        Value::Map(mut m) => {
            Rc::make_mut(&mut m).insert(take_arg(&mut a, 1), x);
            Ok(Value::Map(m))
        }
        v => Err(type_err(it, "insert", 0, "a List, Map or Set", &v, sp)),
    }
}

fn b_update(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 3, "update", sp)?;
    let default = take_arg(&mut a, 2);
    let k = take_arg(&mut a, 1);
    match take_arg(&mut a, 0) {
        Value::Map(mut m) => {
            let cur = m.get(&k).cloned().unwrap_or(default);
            let new = it.call1(&f, cur, sp)?;
            Rc::make_mut(&mut m).insert(k, new);
            Ok(Value::Map(m))
        }
        v => Err(type_err(it, "update", 0, "a Map", &v, sp)),
    }
}

fn m_update(it: &mut Interp, t: &mut Value, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 2, "update!", sp)?;
    let default = take_arg(&mut a, 1);
    let k = take_arg(&mut a, 0);
    let cur = match &*t {
        Value::Map(m) => m.get(&k).cloned().unwrap_or(default),
        v => return Err(type_err(it, "update!", 0, "a Map", v, sp)),
    };
    // Compute first, so that an error leaves the map unchanged.
    let new = it.call1(&f, cur, sp)?;
    if let Value::Map(m) = t {
        Rc::make_mut(m).insert(k, new);
    }
    Ok(Value::Unit)
}

fn m_insert(it: &mut Interp, t: &mut Value, mut a: Vec<Value>, sp: Span) -> R {
    // `s.insert!(x)` on a set: true if x was not there before.
    if let (Value::Set(m), 1) = (&mut *t, a.len()) {
        return Ok(Value::Bool(Rc::make_mut(m).insert(take_arg(&mut a, 0), Value::Unit).is_none()));
    }
    if a.len() < 2 {
        return Err(it.err(sp, "E0201", "`insert!` takes a position (for a list) or a key (for a map) and a value; a set takes just the value"));
    }
    let x = take_arg(&mut a, 1);
    match t {
        Value::List(xs) => {
            let i = int_arg(it, &a, 0, "insert!", sp)?;
            let j = insert_index(it, i, xs.len(), sp)?;
            Rc::make_mut(xs).insert(j, x);
            Ok(Value::Unit)
        }
        Value::Map(m) => {
            Rc::make_mut(m).insert(take_arg(&mut a, 0), x);
            Ok(Value::Unit)
        }
        v => Err(type_err(it, "insert!", 0, "a List, Map or Set", v, sp)),
    }
}

fn b_remove(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    match take_arg(&mut a, 0) {
        Value::List(xs) => {
            let i = int_arg(it, &a, 1, "remove", sp)?;
            let Some(j) = norm_index(i, xs.len()) else {
                return Err(it.err(sp, "E0204", format!("index {} is out of bounds for a list of length {}", i, xs.len())));
            };
            let mut xs = list_into_vec(xs);
            xs.remove(j);
            Ok(Value::list(xs))
        }
        Value::Map(mut m) => {
            Rc::make_mut(&mut m).remove(&a[1]);
            Ok(Value::Map(m))
        }
        Value::Set(mut m) => {
            Rc::make_mut(&mut m).remove(&a[1]);
            Ok(Value::Set(m))
        }
        v => Err(type_err(it, "remove", 0, "a List, Map or Set", &v, sp)),
    }
}

fn m_remove(it: &mut Interp, t: &mut Value, a: Vec<Value>, sp: Span) -> R {
    match t {
        Value::List(xs) => {
            let i = int_arg(it, &a, 0, "remove!", sp)?;
            let Some(j) = norm_index(i, xs.len()) else {
                return Err(it.err(sp, "E0204", format!("index {} is out of bounds for a list of length {}", i, xs.len())));
            };
            Ok(Rc::make_mut(xs).remove(j))
        }
        Value::Map(m) => {
            let r = Rc::make_mut(m).remove(&a[0]);
            Ok(it.option(r))
        }
        Value::Set(m) => Ok(Value::Bool(Rc::make_mut(m).remove(&a[0]).is_some())),
        v => Err(type_err(it, "remove!", 0, "a List, Map or Set", v, sp)),
    }
}

fn b_set(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let x = take_arg(&mut a, 2);
    match take_arg(&mut a, 0) {
        Value::List(mut xs) => {
            let i = int_arg(it, &a, 1, "set", sp)?;
            let Some(j) = norm_index(i, xs.len()) else {
                return Err(it.err(sp, "E0204", format!("index {} is out of bounds for a list of length {}", i, xs.len())));
            };
            Rc::make_mut(&mut xs)[j] = x;
            Ok(Value::List(xs))
        }
        Value::Map(mut m) => {
            Rc::make_mut(&mut m).insert(take_arg(&mut a, 1), x);
            Ok(Value::Map(m))
        }
        v => Err(type_err(it, "set", 0, "a List or Map", &v, sp)),
    }
}

fn m_extend(it: &mut Interp, t: &mut Value, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let ys = items(it, v, "extend!", 1, sp)?;
    match t {
        Value::List(xs) => {
            Rc::make_mut(xs).extend(ys);
            Ok(Value::Unit)
        }
        v => Err(type_err(it, "extend!", 0, "a List", v, sp)),
    }
}

fn m_clear(it: &mut Interp, t: &mut Value, _: Vec<Value>, sp: Span) -> R {
    match t {
        Value::List(xs) => Rc::make_mut(xs).clear(),
        Value::Map(m) | Value::Set(m) => Rc::make_mut(m).clear(),
        v => return Err(type_err(it, "clear!", 0, "a List, Map or Set", v, sp)),
    }
    Ok(Value::Unit)
}

fn m_swap(it: &mut Interp, t: &mut Value, a: Vec<Value>, sp: Span) -> R {
    let i = int_arg(it, &a, 0, "swap!", sp)?;
    let j = int_arg(it, &a, 1, "swap!", sp)?;
    match t {
        Value::List(xs) => {
            let n = xs.len();
            match (norm_index(i, n), norm_index(j, n)) {
                (Some(x), Some(y)) => {
                    Rc::make_mut(xs).swap(x, y);
                    Ok(Value::Unit)
                }
                _ => Err(it.err(sp, "E0204", format!("swap indexes {} and {} are not both valid for a list of length {}", i, j, n))),
            }
        }
        v => Err(type_err(it, "swap!", 0, "a List", v, sp)),
    }
}

fn get_impl(it: &mut Interp, a: &[Value], sp: Span, name: &str) -> R<Option<Value>> {
    Ok(match (&a[0], &a[1]) {
        (Value::List(xs), Value::Int(i)) | (Value::Tuple(xs), Value::Int(i)) => norm_index(*i, xs.len()).map(|j| xs[j].clone()),
        (Value::Str(s), Value::Int(i)) => norm_index(*i, s.char_len()).map(|j| Value::str(s.char_at(j).unwrap_or(""))),
        (Value::Map(m), k) => m.get(k).cloned(),
        (Value::Record(r), Value::Str(k)) => r.get(k).cloned(),
        (Value::Range(r), Value::Int(i)) => r.nth(*i).map(Value::Int),
        (Value::List(_) | Value::Str(_) | Value::Range(_), other) => return Err(type_err(it, name, 1, "an Int index", other, sp)),
        (v, _) => return Err(type_err(it, name, 0, "a List, Str, Range or Map", v, sp)),
    })
}

fn b_get(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let r = get_impl(it, &a, sp, "get")?;
    Ok(it.option(r))
}

fn b_get_or(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let r = get_impl(it, &a, sp, "get_or")?;
    Ok(r.unwrap_or_else(|| take_arg(&mut a, 2)))
}

fn b_first(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let r = match &a[0] {
        Value::List(xs) | Value::Tuple(xs) => xs.first().cloned(),
        Value::Str(s) => s.chars().next().map(|c| Value::str(c.to_string())),
        Value::Range(r) => {
            if r.len() == Some(0) {
                None
            } else {
                Some(Value::Int(r.start))
            }
        }
        // (Sets keep the order in which elements were added.)
        Value::Set(m) => m.entries.first().map(|(k, _)| k.clone()),
        v => return Err(type_err(it, "first", 0, "a List, Str, Range or Set", v, sp)),
    };
    Ok(it.option(r))
}

fn b_last(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let r = match &a[0] {
        Value::List(xs) | Value::Tuple(xs) => xs.last().cloned(),
        Value::Str(s) => s.chars().last().map(|c| Value::str(c.to_string())),
        Value::Range(r) => match r.end {
            Some(e) if e > r.start as i128 => Some(Value::Int((e - 1) as i64)),
            Some(_) => None,
            None => return Err(it.err(sp, "E0216", "an unbounded range has no last element")),
        },
        Value::Set(m) => m.entries.last().map(|(k, _)| k.clone()),
        v => return Err(type_err(it, "last", 0, "a List, Str, Range or Set", v, sp)),
    };
    Ok(it.option(r))
}

fn b_map(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "map", sp)?;
    let v = take_arg(&mut a, 0);
    match &v {
        Value::Variant(vv) if vv.ty.id == OPTION_ID => {
            return if vv.tag == 0 {
                let r = it.call(&f, vec![vv.values[0].clone()], sp)?;
                Ok(it.some(r))
            } else {
                Ok(v)
            };
        }
        Value::Variant(vv) if vv.ty.id == RESULT_ID => {
            return if vv.tag == 0 {
                let r = it.call(&f, vec![vv.values[0].clone()], sp)?;
                Ok(it.ok(r))
            } else {
                Ok(v)
            };
        }
        Value::Map(_) => {
            return Err(it.fail(
                it.diag(sp, "E0200", "`map` over a Map is ambiguous")
                    .help("use `m.map_values(f)` to transform values, or `m.entries().map(f)` for (key, value) pairs"),
            ))
        }
        _ => {}
    }
    let xs = items(it, v, "map", 0, sp)?;
    let mut out = Vec::with_capacity(xs.len());
    for x in xs {
        out.push(it.call1(&f, x, sp)?);
    }
    Ok(Value::list(out))
}

fn b_filter(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "filter", sp)?;
    let v = take_arg(&mut a, 0);
    if let Value::Map(m) = &v {
        let two = wants_two(&f);
        let mut out = MapVal::new();
        for (k, x) in m.entries.iter() {
            let keep = if two {
                bool_result(it, &f, vec![k.clone(), x.clone()], sp, "filter")?
            } else {
                bool_result(it, &f, vec![Value::tuple(vec![k.clone(), x.clone()])], sp, "filter")?
            };
            if keep {
                out.insert(k.clone(), x.clone());
            }
        }
        return Ok(Value::Map(Rc::new(out)));
    }
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "filter", 0, sp)?;
    let mut out = Vec::new();
    for x in xs {
        if bool_result1(it, &f, x.clone(), sp, "filter")? {
            out.push(x);
        }
    }
    Ok(restring(was_str, out))
}

fn b_reduce(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "reduce", sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "reduce", 0, sp)?;
    let mut iter = xs.into_iter();
    let Some(mut acc) = iter.next() else {
        return Err(it.fail(it.diag(sp, "E0216", "`reduce` of an empty collection").help("use `fold(xs, initial, f)` to handle empty collections")));
    };
    for x in iter {
        acc = it.call2(&f, acc, x, sp)?;
    }
    Ok(acc)
}

fn b_fold(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 2, "fold", sp)?;
    let mut acc = take_arg(&mut a, 1);
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "fold", 0, sp)?;
    for x in xs {
        acc = it.call2(&f, acc, x, sp)?;
    }
    Ok(acc)
}

fn b_sum(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    if let Value::Range(r) = &v {
        if let Some(e) = r.end {
            let (s, e) = (r.start as i128, e);
            if e <= s {
                return Ok(Value::Int(0));
            }
            let total = (s + e - 1) * (e - s) / 2;
            return i64::try_from(total).map(Value::Int).map_err(|_| it.err(sp, "E0207", "integer overflow in sum"));
        }
    }
    let xs = items(it, v, "sum", 0, sp)?;
    let mut acc = Value::Int(0);
    for x in xs {
        if !matches!(x, Value::Int(_) | Value::Float(_)) {
            return Err(it.err(sp, "E0200", format!("`sum` needs numbers, but found {}", describe(&x))));
        }
        acc = it.binop(crate::ast::BinOp::Add, acc, x, sp)?;
    }
    Ok(acc)
}

fn b_product(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "product", 0, sp)?;
    let mut acc = Value::Int(1);
    for x in xs {
        if !matches!(x, Value::Int(_) | Value::Float(_)) {
            return Err(it.err(sp, "E0200", format!("`product` needs numbers, but found {}", describe(&x))));
        }
        acc = it.binop(crate::ast::BinOp::Mul, acc, x, sp)?;
    }
    Ok(acc)
}

fn extreme_by(it: &mut Interp, mut a: Vec<Value>, sp: Span, want: Ordering, name: &str) -> R {
    let f = fn_arg(it, &a, 1, name, sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, name, 0, sp)?;
    let mut best: Option<(Value, Value)> = None;
    for x in xs {
        let k = it.call1(&f, x.clone(), sp)?;
        best = Some(match best {
            None => (k, x),
            Some((bk, bx)) => {
                if cmp_values(it, &k, &bk, sp)? == want {
                    (k, x)
                } else {
                    (bk, bx)
                }
            }
        });
    }
    Ok(it.option(best.map(|(_, x)| x)))
}

fn b_min_by(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    extreme_by(it, a, sp, Ordering::Less, "min_by")
}

fn b_max_by(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    extreme_by(it, a, sp, Ordering::Greater, "max_by")
}

fn b_sort(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "sort", 0, sp)?;
    Ok(restring(was_str, sort_values(it, xs, sp)?))
}

fn m_sort(it: &mut Interp, t: &mut Value, _: Vec<Value>, sp: Span) -> R {
    // Sort a copy, so that on error the variable keeps its old value.
    let xs = match &*t {
        Value::List(xs) => xs.to_vec(),
        other => return Err(type_err(it, "sort!", 0, "a List", other, sp)),
    };
    let sorted = sort_values(it, xs, sp)?;
    *t = Value::list(sorted);
    Ok(Value::Unit)
}

fn b_sort_by(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "sort_by", sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "sort_by", 0, sp)?;
    Ok(Value::list(sort_by_key(it, xs, &f, sp)?))
}

fn m_sort_by(it: &mut Interp, t: &mut Value, a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 0, "sort_by!", sp)?;
    let v = std::mem::take(t);
    let xs = match &v {
        Value::List(xs) => xs.to_vec(),
        other => {
            let e = type_err(it, "sort_by!", 0, "a List", other, sp);
            *t = v;
            return Err(e);
        }
    };
    match sort_by_key(it, xs, &f, sp) {
        Ok(sorted) => {
            *t = Value::list(sorted);
            Ok(Value::Unit)
        }
        Err(e) => {
            *t = v;
            Err(e)
        }
    }
}

fn b_sort_with(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "sort_with", sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "sort_with", 0, sp)?;
    let sorted = merge_sort(xs, &mut |x, y| {
        let r = it.call2(&f, x.clone(), y.clone(), sp)?;
        ordering_of(it, &r, sp)
    })?;
    Ok(Value::list(sorted))
}

fn b_reverse(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    if let Value::Str(s) = &v {
        return Ok(Value::str(s.chars().rev().collect::<String>()));
    }
    let mut xs = items(it, v, "reverse", 0, sp)?;
    xs.reverse();
    Ok(Value::list(xs))
}

fn m_reverse(it: &mut Interp, t: &mut Value, _: Vec<Value>, sp: Span) -> R {
    match t {
        Value::List(xs) => {
            Rc::make_mut(xs).reverse();
            Ok(Value::Unit)
        }
        v => Err(type_err(it, "reverse!", 0, "a List", v, sp)),
    }
}

fn b_contains(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    Ok(Value::Bool(it.contains(&a[0], &a[1], sp)?))
}

fn b_index_of(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let r = match (&a[0], &a[1]) {
        (Value::List(xs), x) | (Value::Tuple(xs), x) => xs.iter().position(|y| values_equal(x, y)).map(|i| Value::Int(i as i64)),
        (Value::Str(s), Value::Str(sub)) => s.find(sub.as_str()).map(|b| Value::Int(s[..b].chars().count() as i64)),
        (Value::Str(_), other) => return Err(type_err(it, "index_of", 1, "a Str", other, sp)),
        (Value::Range(r), Value::Int(n)) => r.contains(*n).then(|| Value::Int(n - r.start)),
        (Value::Range(_), _) => None,
        (v, _) => return Err(type_err(it, "index_of", 0, "a List, Str or Range", v, sp)),
    };
    Ok(it.option(r))
}

fn b_find(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    if let (Value::Str(_), Value::Str(_)) = (&a[0], &a[1]) {
        return Err(it.fail(
            it.diag(sp, "E0200", "`find` takes a predicate function, not a Str")
                .help("to search a string, use `s.index_of(sub)` (the position, as an Option) or `s.contains(sub)`"),
        ));
    }
    let f = fn_arg(it, &a, 1, "find", sp)?;
    let v = take_arg(&mut a, 0);
    let two = matches!(v, Value::Map(_)) && wants_two(&f);
    let xs = items(it, v, "find", 0, sp)?;
    for x in xs {
        if entry_pred(it, &f, x.clone(), two, sp, "find")? {
            return Ok(it.some(x));
        }
    }
    Ok(it.none())
}

fn b_find_index(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "find_index", sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "find_index", 0, sp)?;
    for (i, x) in xs.into_iter().enumerate() {
        if bool_result1(it, &f, x, sp, "find_index")? {
            return Ok(it.some(Value::Int(i as i64)));
        }
    }
    Ok(it.none())
}

fn b_any(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = if a.len() == 2 { Some(fn_arg(it, &a, 1, "any", sp)?) } else { None };
    let v = take_arg(&mut a, 0);
    let two = matches!(v, Value::Map(_)) && f.as_ref().is_some_and(wants_two);
    let xs = items(it, v, "any", 0, sp)?;
    for x in xs {
        let b = match &f {
            Some(f) => entry_pred(it, f, x, two, sp, "any")?,
            None => match x {
                Value::Bool(b) => b,
                other => return Err(it.err(sp, "E0200", format!("`any` without a predicate needs Bools, found {}", describe(&other)))),
            },
        };
        if b {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn b_all(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = if a.len() == 2 { Some(fn_arg(it, &a, 1, "all", sp)?) } else { None };
    let v = take_arg(&mut a, 0);
    let two = matches!(v, Value::Map(_)) && f.as_ref().is_some_and(wants_two);
    let xs = items(it, v, "all", 0, sp)?;
    for x in xs {
        let b = match &f {
            Some(f) => entry_pred(it, f, x, two, sp, "all")?,
            None => match x {
                Value::Bool(b) => b,
                other => return Err(it.err(sp, "E0200", format!("`all` without a predicate needs Bools, found {}", describe(&other)))),
            },
        };
        if !b {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn b_count(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    if let (Value::Str(s), Value::Str(sub)) = (&a[0], &a[1]) {
        if sub.is_empty() {
            return Err(it.err(sp, "E0216", "cannot count occurrences of the empty string"));
        }
        return Ok(Value::Int(s.matches(sub.as_str()).count() as i64));
    }
    let probe = take_arg(&mut a, 1);
    let v = take_arg(&mut a, 0);
    let two = matches!(v, Value::Map(_)) && wants_two(&probe);
    let xs = items(it, v, "count", 0, sp)?;
    let mut n = 0;
    if probe.is_callable() {
        for x in xs {
            if entry_pred(it, &probe, x, two, sp, "count")? {
                n += 1;
            }
        }
    } else {
        n = xs.iter().filter(|x| values_equal(x, &probe)).count();
    }
    Ok(Value::Int(n as i64))
}

fn b_take(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let n = usize_arg(it, &a, 1, "take", sp)?;
    let v = take_arg(&mut a, 0);
    match v {
        Value::Str(s) => Ok(Value::str(s.chars().take(n).collect::<String>())),
        Value::Range(r) => {
            // (An endless range stops at max_int.)
            let len = r.len_u128().unwrap_or((i64::MAX as i128 + 1 - r.start as i128) as u128);
            let count = len.min(n as u128);
            if count > 100_000_000 {
                return Err(it.err(sp, "E0216", format!("`take` of {} elements would be too large", count)));
            }
            it.tick_n(count as u64, sp)?;
            Ok(Value::list((0..count as i128).map(|i| Value::Int((r.start as i128 + i) as i64)).collect()))
        }
        v => {
            let mut xs = items(it, v, "take", 0, sp)?;
            xs.truncate(n);
            Ok(Value::list(xs))
        }
    }
}

fn b_drop(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let n = usize_arg(it, &a, 1, "drop", sp)?;
    let v = take_arg(&mut a, 0);
    match v {
        Value::Str(s) => {
            let len = s.char_len();
            Ok(Value::str(s.slice_chars(n.min(len), len)))
        }
        v => {
            let xs = items(it, v, "drop", 0, sp)?;
            Ok(Value::list(xs.into_iter().skip(n).collect()))
        }
    }
}

fn b_take_while(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "take_while", sp)?;
    let v = take_arg(&mut a, 0);
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "take_while", 0, sp)?;
    let mut out = Vec::new();
    for x in xs {
        if !bool_result1(it, &f, x.clone(), sp, "take_while")? {
            break;
        }
        out.push(x);
    }
    Ok(restring(was_str, out))
}

fn b_drop_while(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "drop_while", sp)?;
    let v = take_arg(&mut a, 0);
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "drop_while", 0, sp)?;
    let mut i = 0;
    while i < xs.len() && bool_result(it, &f, vec![xs[i].clone()], sp, "drop_while")? {
        i += 1;
    }
    Ok(restring(was_str, xs[i..].to_vec()))
}

fn b_slice(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = int_arg(it, &a, 1, "slice", sp)?;
    let e = int_arg(it, &a, 2, "slice", sp)?;
    let r = Value::Range(Rc::new(RangeVal { start: s, end: Some(e as i128) }));
    match &a[0] {
        Value::List(_) | Value::Str(_) => it.index_value(a[0].clone(), r, sp),
        v => Err(type_err(it, "slice", 0, "a List or Str", v, sp)),
    }
}

fn b_zip(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let b = take_arg(&mut a, 1);
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "zip", 0, sp)?;
    let ys = items(it, b, "zip", 1, sp)?;
    Ok(Value::list(xs.into_iter().zip(ys).map(|(x, y)| Value::tuple(vec![x, y])).collect()))
}

fn b_enumerate(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "enumerate", 0, sp)?;
    Ok(Value::list(xs.into_iter().enumerate().map(|(i, x)| Value::tuple(vec![Value::Int(i as i64), x])).collect()))
}

fn b_flat_map(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "flat_map", sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "flat_map", 0, sp)?;
    let mut out = Vec::new();
    for x in xs {
        let r = it.call1(&f, x, sp)?;
        match r {
            Value::Variant(vv) if vv.ty.id == OPTION_ID => {
                if vv.tag == 0 {
                    out.push(vv.values[0].clone());
                }
            }
            other => out.extend(items(it, other, "flat_map", 1, sp)?),
        }
    }
    Ok(Value::list(out))
}

fn b_flatten(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "flatten", 0, sp)?;
    let mut out = Vec::new();
    for x in xs {
        match x {
            Value::Variant(vv) if vv.ty.id == OPTION_ID => {
                if vv.tag == 0 {
                    out.push(vv.values[0].clone());
                }
            }
            Value::List(xs) | Value::Tuple(xs) => out.extend(xs.iter().cloned()),
            other => {
                return Err(it.err(sp, "E0200", format!("`flatten` needs a list of lists (or Options), but found the element {}", describe(&other))))
            }
        }
    }
    Ok(Value::list(out))
}

fn b_join(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let sep = if a.len() == 2 { str_arg(it, &a, 1, "join", sp)?.to_string() } else { String::new() };
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "join", 0, sp)?;
    let mut s = String::new();
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            s.push_str(&sep);
        }
        write_value(&mut s, x, false);
    }
    Ok(Value::str(s))
}

fn b_unique(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "unique", 0, sp)?;
    let mut seen = MapVal::new();
    let mut out = Vec::new();
    for x in xs {
        if !seen.contains(&x) {
            seen.insert(x.clone(), Value::Unit);
            out.push(x);
        }
    }
    Ok(restring(was_str, out))
}

fn b_group_by(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "group_by", sp)?;
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "group_by", 0, sp)?;
    let mut m = MapVal::new();
    for x in xs {
        let k = it.call1(&f, x.clone(), sp)?;
        match m.get_mut(&k) {
            Some(Value::List(g)) => Rc::make_mut(g).push(x),
            _ => {
                m.insert(k, Value::list(vec![x]));
            }
        }
    }
    Ok(Value::Map(Rc::new(m)))
}

fn b_tally(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "tally", 0, sp)?;
    let mut m = MapVal::new();
    for x in xs {
        match m.get_mut(&x) {
            Some(Value::Int(n)) => *n += 1,
            _ => {
                m.insert(x, Value::Int(1));
            }
        }
    }
    Ok(Value::Map(Rc::new(m)))
}

fn b_partition(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "partition", sp)?;
    let v = take_arg(&mut a, 0);
    let two = matches!(v, Value::Map(_)) && wants_two(&f);
    let xs = items(it, v, "partition", 0, sp)?;
    let (mut yes, mut no) = (Vec::new(), Vec::new());
    for x in xs {
        if entry_pred(it, &f, x.clone(), two, sp, "partition")? {
            yes.push(x);
        } else {
            no.push(x);
        }
    }
    Ok(Value::tuple(vec![Value::list(yes), Value::list(no)]))
}

fn b_chunks(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let n = usize_arg(it, &a, 1, "chunks", sp)?;
    if n == 0 {
        return Err(it.err(sp, "E0216", "chunk size must be positive"));
    }
    let v = take_arg(&mut a, 0);
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "chunks", 0, sp)?;
    Ok(Value::list(xs.chunks(n).map(|c| restring(was_str, c.to_vec())).collect()))
}

fn b_windows(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let n = usize_arg(it, &a, 1, "windows", sp)?;
    if n == 0 {
        return Err(it.err(sp, "E0216", "window size must be positive"));
    }
    let v = take_arg(&mut a, 0);
    let was_str = matches!(v, Value::Str(_));
    let xs = items(it, v, "windows", 0, sp)?;
    Ok(Value::list(xs.windows(n).map(|c| restring(was_str, c.to_vec())).collect()))
}

fn b_repeat(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let n = usize_arg(it, &a, 1, "repeat", sp)?;
    if n > 100_000_000 {
        return Err(it.err(sp, "E0216", "repeat count is too large"));
    }
    it.tick_n(n as u64, sp)?;
    let x = take_arg(&mut a, 0);
    if let Value::Str(s) = &x {
        if s.len().saturating_mul(n) > 1 << 31 {
            return Err(it.err(sp, "E0216", "repeated string would be too large"));
        }
        return Ok(Value::str(s.repeat(n)));
    }
    Ok(Value::list(vec![x; n]))
}

fn b_each(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "each", sp)?;
    let v = take_arg(&mut a, 0);
    let two = matches!(v, Value::Map(_)) && wants_two(&f);
    let xs = items(it, v, "each", 0, sp)?;
    for x in xs {
        call_entry(it, &f, x, two, sp)?;
    }
    Ok(Value::Unit)
}

fn b_to_list(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    Ok(Value::list(items(it, v, "to_list", 0, sp)?))
}

fn b_to_map(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, "to_map", 0, sp)?;
    let mut m = MapVal::with_capacity(xs.len());
    for x in xs {
        match x {
            Value::Tuple(t) if t.len() == 2 => {
                m.insert(t[0].clone(), t[1].clone());
            }
            Value::List(t) if t.len() == 2 => {
                m.insert(t[0].clone(), t[1].clone());
            }
            other => return Err(it.err(sp, "E0200", format!("`to_map` needs (key, value) pairs, found {}", describe(&other)))),
        }
    }
    Ok(Value::Map(Rc::new(m)))
}

fn b_keys(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let m = map_arg(it, &a, 0, "keys", sp)?;
    Ok(Value::list(m.entries.iter().map(|(k, _)| k.clone()).collect()))
}

fn b_values(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let m = map_arg(it, &a, 0, "values", sp)?;
    Ok(Value::list(m.entries.iter().map(|(_, v)| v.clone()).collect()))
}

fn b_entries(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let m = map_arg(it, &a, 0, "entries", sp)?;
    Ok(Value::list(m.entries.iter().map(|(k, v)| Value::tuple(vec![k.clone(), v.clone()])).collect()))
}

fn set_arg(it: &Interp, a: &[Value], i: usize, f: &str, sp: Span) -> R<Rc<MapVal>> {
    match &a[i] {
        Value::Set(m) => Ok(m.clone()),
        v => Err(type_err(it, f, i, "a Set", v, sp)),
    }
}

fn b_to_set(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    if a.is_empty() {
        return Ok(Value::Set(Rc::new(MapVal::new())));
    }
    let v = take_arg(&mut a, 0);
    if let Value::Set(_) = v {
        return Ok(v);
    }
    let xs = items(it, v, "to_set", 0, sp)?;
    let mut m = MapVal::with_capacity(xs.len());
    for x in xs {
        m.insert(x, Value::Unit);
    }
    Ok(Value::Set(Rc::new(m)))
}

fn b_union(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let mut m = set_arg(it, &a, 0, "union", sp)?;
    let t = set_arg(it, &a, 1, "union", sp)?;
    let mm = Rc::make_mut(&mut m);
    for (k, _) in t.entries.iter() {
        mm.insert(k.clone(), Value::Unit);
    }
    Ok(Value::Set(m))
}

fn b_intersection(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = set_arg(it, &a, 0, "intersection", sp)?;
    let t = set_arg(it, &a, 1, "intersection", sp)?;
    let mut out = MapVal::new();
    for (k, _) in s.entries.iter().filter(|(k, _)| t.contains(k)) {
        out.insert(k.clone(), Value::Unit);
    }
    Ok(Value::Set(Rc::new(out)))
}

fn b_difference(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = set_arg(it, &a, 0, "difference", sp)?;
    let t = set_arg(it, &a, 1, "difference", sp)?;
    let mut out = MapVal::new();
    for (k, _) in s.entries.iter().filter(|(k, _)| !t.contains(k)) {
        out.insert(k.clone(), Value::Unit);
    }
    Ok(Value::Set(Rc::new(out)))
}

fn b_is_subset(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = set_arg(it, &a, 0, "is_subset", sp)?;
    let t = set_arg(it, &a, 1, "is_subset", sp)?;
    Ok(Value::Bool(s.entries.iter().all(|(k, _)| t.contains(k))))
}

fn b_has(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    if let Value::Set(m) = &a[0] {
        return Ok(Value::Bool(m.contains(&a[1])));
    }
    let m = map_arg(it, &a, 0, "has", sp)?;
    Ok(Value::Bool(m.contains(&a[1])))
}

fn b_merge(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let b = map_arg(it, &a, 1, "merge", sp)?;
    let v = take_arg(&mut a, 0);
    let Value::Map(mut m) = v else { return Err(type_err(it, "merge", 0, "a Map", &v, sp)) };
    let mm = Rc::make_mut(&mut m);
    for (k, x) in b.entries.iter() {
        mm.insert(k.clone(), x.clone());
    }
    Ok(Value::Map(m))
}

fn b_map_values(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let m = map_arg(it, &a, 0, "map_values", sp)?;
    let f = fn_arg(it, &a, 1, "map_values", sp)?;
    let mut out = MapVal::with_capacity(m.len());
    for (k, v) in m.entries.iter() {
        let nv = it.call1(&f, v.clone(), sp)?;
        out.insert(k.clone(), nv);
    }
    Ok(Value::Map(Rc::new(out)))
}

// ============================================================ strings

fn b_split(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "split", sp)?;
    if a.len() == 1 {
        return Ok(Value::list(s.split_whitespace().map(Value::str).collect()));
    }
    let sep = str_arg(it, &a, 1, "split", sp)?;
    let limit = if a.len() == 3 {
        let n = usize_arg(it, &a, 2, "split", sp)?;
        if n == 0 {
            return Err(it.err(sp, "E0216", "`split` limit must be at least 1"));
        }
        Some(n)
    } else {
        None
    };
    if sep.is_empty() {
        // Split into characters; with a limit, the last piece keeps the rest.
        let n = limit.unwrap_or(usize::MAX);
        let mut out = Vec::new();
        let mut rest = s;
        while out.len() + 1 < n {
            let Some(c) = rest.chars().next() else { break };
            out.push(Value::str(c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
        if !rest.is_empty() {
            out.push(Value::str(rest));
        }
        return Ok(Value::list(out));
    }
    if let Some(n) = limit {
        return Ok(Value::list(s.splitn(n, sep).map(Value::str).collect()));
    }
    Ok(Value::list(s.split(sep).map(Value::str).collect()))
}

fn b_split_once(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "split_once", sp)?;
    let sep = str_arg(it, &a, 1, "split_once", sp)?;
    if sep.is_empty() {
        return Err(it.err(sp, "E0216", "`split_once` cannot split at the empty string"));
    }
    let r = s.split_once(sep).map(|(x, y)| Value::tuple(vec![Value::str(x), Value::str(y)]));
    Ok(it.option(r))
}

fn b_strip_prefix(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "strip_prefix", sp)?;
    let p = str_arg(it, &a, 1, "strip_prefix", sp)?;
    let r = s.strip_prefix(p).map(Value::str);
    Ok(it.option(r))
}

fn b_strip_suffix(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "strip_suffix", sp)?;
    let p = str_arg(it, &a, 1, "strip_suffix", sp)?;
    let r = s.strip_suffix(p).map(Value::str);
    Ok(it.option(r))
}

fn b_lines(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "lines", sp)?;
    Ok(Value::list(s.lines().map(Value::str).collect()))
}

fn b_words(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "words", sp)?;
    Ok(Value::list(s.split_whitespace().map(Value::str).collect()))
}

fn b_chars(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "chars", sp)?;
    Ok(Value::list(s.chars().map(|c| Value::str(c.to_string())).collect()))
}

fn str_map(it: &mut Interp, a: &[Value], sp: Span, name: &str, f: fn(&str) -> String) -> R {
    let s = str_arg(it, a, 0, name, sp)?;
    Ok(Value::str(f(s)))
}

fn b_trim(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_map(it, &a, sp, "trim", |s| s.trim().to_string())
}
fn b_trim_start(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_map(it, &a, sp, "trim_start", |s| s.trim_start().to_string())
}
fn b_trim_end(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_map(it, &a, sp, "trim_end", |s| s.trim_end().to_string())
}
fn b_upper(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_map(it, &a, sp, "upper", |s| s.to_uppercase())
}
fn b_lower(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_map(it, &a, sp, "lower", |s| s.to_lowercase())
}
fn b_capitalize(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_map(it, &a, sp, "capitalize", |s| {
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    })
}

fn b_starts_with(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "starts_with", sp)?;
    let p = str_arg(it, &a, 1, "starts_with", sp)?;
    Ok(Value::Bool(s.starts_with(p)))
}

fn b_ends_with(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "ends_with", sp)?;
    let p = str_arg(it, &a, 1, "ends_with", sp)?;
    Ok(Value::Bool(s.ends_with(p)))
}

fn b_replace(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "replace", sp)?;
    let from = str_arg(it, &a, 1, "replace", sp)?;
    let to = str_arg(it, &a, 2, "replace", sp)?;
    if from.is_empty() {
        return Err(it.err(sp, "E0216", "`replace` cannot search for the empty string"));
    }
    Ok(Value::str(s.replace(from, to)))
}

fn pad(it: &mut Interp, a: &[Value], sp: Span, name: &str, left: bool) -> R {
    let s = match &a[0] {
        Value::Str(s) => s.to_string(),
        v => display(v),
    };
    let w = usize_arg(it, a, 1, name, sp)?;
    let fill = if a.len() == 3 { str_arg(it, a, 2, name, sp)?.to_string() } else { " ".into() };
    if fill.chars().count() != 1 {
        return Err(it.err(sp, "E0216", format!("`{}` fill must be a single character, got {}", name, repr(&Value::str(fill)))));
    }
    let n = s.chars().count();
    if n >= w {
        return Ok(Value::str(s));
    }
    if w > 1 << 26 {
        return Err(it.err(sp, "E0216", format!("`{}` width {} is too large", name, w)));
    }
    let padding = fill.repeat(w - n);
    Ok(Value::str(if left { padding + &s } else { s + &padding }))
}

fn b_pad_left(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    pad(it, &a, sp, "pad_left", true)
}

fn b_pad_right(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    pad(it, &a, sp, "pad_right", false)
}

fn str_pred(it: &mut Interp, a: &[Value], sp: Span, name: &str, f: fn(char) -> bool) -> R {
    let s = str_arg(it, a, 0, name, sp)?;
    Ok(Value::Bool(!s.is_empty() && s.chars().all(f)))
}

fn b_is_digit(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_pred(it, &a, sp, "is_digit", |c| c.is_ascii_digit())
}
fn b_is_alpha(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_pred(it, &a, sp, "is_alpha", |c| c.is_alphabetic())
}
fn b_is_alnum(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_pred(it, &a, sp, "is_alnum", |c| c.is_alphanumeric())
}
fn b_is_space(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    str_pred(it, &a, sp, "is_space", |c| c.is_whitespace())
}
fn b_is_upper(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "is_upper", sp)?;
    Ok(Value::Bool(s.chars().any(|c| c.is_alphabetic()) && !s.chars().any(|c| c.is_lowercase())))
}
fn b_is_lower(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "is_lower", sp)?;
    Ok(Value::Bool(s.chars().any(|c| c.is_alphabetic()) && !s.chars().any(|c| c.is_uppercase())))
}

// ============================================================ Option and Result

fn opt_parts(v: &Value) -> Option<(bool, bool, Option<Value>)> {
    // (is_option, is_present, payload)
    match v {
        Value::Variant(vv) if vv.ty.id == OPTION_ID => Some((true, vv.tag == 0, vv.values.first().cloned())),
        Value::Variant(vv) if vv.ty.id == RESULT_ID => Some((false, vv.tag == 0, vv.values.first().cloned())),
        _ => None,
    }
}

fn need_opt(it: &Interp, v: &Value, name: &str, sp: Span) -> R<(bool, bool, Option<Value>)> {
    opt_parts(v).ok_or_else(|| type_err(it, name, 0, "an Option or Result", v, sp))
}

fn b_unwrap(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let (is_opt, present, payload) = need_opt(it, &a[0], "unwrap", sp)?;
    if present {
        return Ok(payload.unwrap());
    }
    let msg = if is_opt { "called `unwrap` on `None`".to_string() } else { format!("called `unwrap` on `Err({})`", short_repr(&payload.unwrap())) };
    Err(it.fail(it.diag(sp, "E0210", msg).help("handle the missing case with `match`, `unwrap_or(default)`, or `?`")))
}

fn b_expect(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let (is_opt, present, payload) = need_opt(it, &a[0], "expect", sp)?;
    if present {
        return Ok(payload.unwrap());
    }
    let msg = display(&a[1]);
    let detail = if is_opt { String::new() } else { format!(": {}", display(&payload.unwrap())) };
    Err(it.err(sp, "E0210", format!("{}{}", msg, detail)))
}

fn b_unwrap_or(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    let (_, present, payload) = need_opt(it, &a[0], "unwrap_or", sp)?;
    Ok(if present { payload.unwrap() } else { take_arg(&mut a, 1) })
}

fn b_unwrap_or_else(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let (is_opt, present, payload) = need_opt(it, &a[0], "unwrap_or_else", sp)?;
    let f = fn_arg(it, &a, 1, "unwrap_or_else", sp)?;
    if present {
        return Ok(payload.unwrap());
    }
    if is_opt {
        it.call(&f, vec![], sp)
    } else {
        it.call1(&f, payload.unwrap(), sp)
    }
}

fn b_is_some(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(OPTION_ID) {
        Some((tag, _)) => Ok(Value::Bool(tag == 0)),
        None => Err(type_err(it, "is_some", 0, "an Option", &a[0], sp)),
    }
}

fn b_is_none(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(OPTION_ID) {
        Some((tag, _)) => Ok(Value::Bool(tag == 1)),
        None => Err(type_err(it, "is_none", 0, "an Option", &a[0], sp)),
    }
}

fn b_is_ok(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(RESULT_ID) {
        Some((tag, _)) => Ok(Value::Bool(tag == 0)),
        None => Err(type_err(it, "is_ok", 0, "a Result", &a[0], sp)),
    }
}

fn b_is_err(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(RESULT_ID) {
        Some((tag, _)) => Ok(Value::Bool(tag == 1)),
        None => Err(type_err(it, "is_err", 0, "a Result", &a[0], sp)),
    }
}

fn b_and_then(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let (_, present, payload) = need_opt(it, &a[0], "and_then", sp)?;
    let f = fn_arg(it, &a, 1, "and_then", sp)?;
    if present {
        it.call1(&f, payload.unwrap(), sp)
    } else {
        Ok(a[0].clone())
    }
}

fn b_map_err(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let f = fn_arg(it, &a, 1, "map_err", sp)?;
    match a[0].as_variant(RESULT_ID) {
        Some((1, Some(e))) => {
            let e = e.clone();
            let r = it.call1(&f, e, sp)?;
            Ok(it.err_val(r))
        }
        Some(_) => Ok(a[0].clone()),
        None => Err(type_err(it, "map_err", 0, "a Result", &a[0], sp)),
    }
}

fn b_ok_or(it: &mut Interp, mut a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(OPTION_ID) {
        Some((0, Some(v))) => {
            let v = v.clone();
            Ok(it.ok(v))
        }
        Some(_) => {
            let e = take_arg(&mut a, 1);
            Ok(it.err_val(e))
        }
        None => Err(type_err(it, "ok_or", 0, "an Option", &a[0], sp)),
    }
}

fn b_err(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(RESULT_ID) {
        Some((1, Some(e))) => {
            let e = e.clone();
            Ok(it.some(e))
        }
        Some(_) => Ok(it.none()),
        None => Err(type_err(it, "err", 0, "a Result", &a[0], sp)),
    }
}

fn b_unwrap_err(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(RESULT_ID) {
        Some((1, Some(e))) => Ok(e.clone()),
        Some(_) => Err(it.err(sp, "E0210", format!("called `unwrap_err` on {}", short_repr(&a[0])))),
        None => Err(type_err(it, "unwrap_err", 0, "a Result", &a[0], sp)),
    }
}

fn collect_variants(it: &mut Interp, mut a: Vec<Value>, sp: Span, type_id: u32, name: &str) -> R {
    let v = take_arg(&mut a, 0);
    let xs = items(it, v, name, 0, sp)?;
    let mut out = Vec::with_capacity(xs.len());
    for x in xs {
        match &x {
            Value::Variant(vv) if vv.ty.id == type_id => {
                if vv.tag == 0 {
                    out.push(vv.values[0].clone());
                } else {
                    return Ok(x);
                }
            }
            other => {
                let want = if type_id == OPTION_ID { "Options" } else { "Results" };
                return Err(it.err(sp, "E0200", format!("`{}` needs a list of {}, but found {}", name, want, describe(other))));
            }
        }
    }
    Ok(if type_id == OPTION_ID { it.some(Value::list(out)) } else { it.ok(Value::list(out)) })
}

fn b_collect_ok(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    collect_variants(it, a, sp, RESULT_ID, "collect_ok")
}

fn b_collect_some(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    collect_variants(it, a, sp, OPTION_ID, "collect_some")
}

fn b_ok(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    match a[0].as_variant(RESULT_ID) {
        Some((0, Some(v))) => {
            let v = v.clone();
            Ok(it.some(v))
        }
        Some(_) => Ok(it.none()),
        None => Err(type_err(it, "ok", 0, "a Result", &a[0], sp)),
    }
}

// ============================================================ JSON

fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn to_json(v: &Value, indent: usize, level: usize, out: &mut String) -> Result<(), String> {
    let nl = |out: &mut String, lvl: usize| {
        if indent > 0 {
            out.push('\n');
            out.push_str(&" ".repeat(indent * lvl));
        }
    };
    let sep = if indent > 0 { ": " } else { ":" };
    match v {
        Value::Unit => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(format!("{} cannot be represented in JSON", format_float(*f)));
            }
            out.push_str(&format_float(*f));
        }
        Value::Str(s) => json_escape(s, out),
        // A set becomes an array of its elements.
        Value::Set(m) => {
            let xs: Vec<Value> = m.entries.iter().map(|(k, _)| k.clone()).collect();
            return to_json(&Value::list(xs), indent, level, out);
        }
        Value::List(xs) | Value::Tuple(xs) => {
            if xs.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, level + 1);
                to_json(x, indent, level + 1, out)?;
            }
            nl(out, level);
            out.push(']');
        }
        Value::Map(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            // JSON object keys are strings: Str and Int keys are allowed (Ints
            // become their decimal text), as long as no two become the same.
            let mut seen = std::collections::HashSet::new();
            out.push('{');
            for (i, (k, x)) in m.entries.iter().enumerate() {
                let key = match k {
                    Value::Str(s) => s.to_string(),
                    Value::Int(n) => n.to_string(),
                    other => return Err(format!("JSON object keys must be Str or Int, but this map has the key {}", short_repr(other))),
                };
                if !seen.insert(key.clone()) {
                    return Err(format!("two keys of this map both become the JSON key \"{}\"", key));
                }
                if i > 0 {
                    out.push(',');
                }
                nl(out, level + 1);
                json_escape(&key, out);
                out.push_str(sep);
                to_json(x, indent, level + 1, out)?;
            }
            nl(out, level);
            out.push('}');
        }
        Value::Record(r) => {
            if r.names.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push('{');
            for (i, (k, x)) in r.names.iter().zip(&r.values).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                nl(out, level + 1);
                json_escape(k, out);
                out.push_str(sep);
                to_json(x, indent, level + 1, out)?;
            }
            nl(out, level);
            out.push('}');
        }
        Value::Variant(vv) => {
            if vv.ty.id == OPTION_ID {
                return match vv.values.first() {
                    Some(x) => to_json(x, indent, level, out),
                    None => {
                        out.push_str("null");
                        Ok(())
                    }
                };
            }
            if vv.values.is_empty() {
                json_escape(&vv.name(), out);
                return Ok(());
            }
            let (fields, _, named) = vv.ty.fields_of(vv.tag);
            out.push('{');
            nl(out, level + 1);
            json_escape(&vv.name(), out);
            out.push_str(sep);
            if named {
                out.push('{');
                for (i, (k, x)) in fields.iter().zip(&vv.values).enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    nl(out, level + 2);
                    json_escape(k, out);
                    out.push_str(sep);
                    to_json(x, indent, level + 2, out)?;
                }
                nl(out, level + 1);
                out.push('}');
            } else if vv.values.len() == 1 {
                to_json(&vv.values[0], indent, level + 1, out)?;
            } else {
                to_json(&Value::list(vv.values.clone()), indent, level + 1, out)?;
            }
            nl(out, level);
            out.push('}');
        }
        other => return Err(format!("{} cannot be represented in JSON", describe(other))),
    }
    Ok(())
}

fn b_to_json(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let indent = if a.len() == 2 { usize_arg(it, &a, 1, "to_json", sp)? } else { 0 };
    let mut out = String::new();
    to_json(&a[0], indent.min(16), 0, &mut out).map_err(|m| it.err(sp, "E0216", m))?;
    Ok(Value::str(out))
}

/// Whether `t` is a number in JSON's grammar: `-?(0|[1-9][0-9]*)`, then
/// optionally `.` and digits, then optionally `e`, a sign and digits.
fn json_number(t: &str) -> bool {
    let b = t.strip_prefix('-').unwrap_or(t).as_bytes();
    let digits = |i: usize| b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut i = digits(0);
    if i == 0 {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        let n = digits(i + 1);
        if n == 0 {
            return false;
        }
        i += 1 + n;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let n = digits(i);
        if n == 0 {
            return false;
        }
        i += n;
    }
    i == b.len()
}

struct JsonParser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && matches!(self.s[self.pos], b' ' | b'\n' | b'\r' | b'\t') {
            self.pos += 1;
        }
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        let before = &self.s[..self.pos.min(self.s.len())];
        let line = before.iter().filter(|b| **b == b'\n').count() + 1;
        let col = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        Err(format!("{} at line {}, column {}", msg, line, col))
    }

    fn value(&mut self, it: &Interp, depth: usize) -> Result<Value, String> {
        if depth > 500 {
            return self.err("JSON nested too deeply");
        }
        self.ws();
        let Some(&c) = self.s.get(self.pos) else { return self.err("unexpected end of JSON") };
        match c {
            b'{' => {
                self.pos += 1;
                let mut m = MapVal::new();
                self.ws();
                if self.s.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Value::Map(Rc::new(m)));
                }
                loop {
                    self.ws();
                    if self.s.get(self.pos) != Some(&b'"') {
                        return self.err("expected a string key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b':') {
                        return self.err("expected `:`");
                    }
                    self.pos += 1;
                    let v = self.value(it, depth + 1)?;
                    m.insert(Value::str(k), v);
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Value::Map(Rc::new(m)));
                        }
                        _ => return self.err("expected `,` or `}`"),
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                let mut xs = Vec::new();
                self.ws();
                if self.s.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Value::list(xs));
                }
                loop {
                    xs.push(self.value(it, depth + 1)?);
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Value::list(xs));
                        }
                        _ => return self.err("expected `,` or `]`"),
                    }
                }
            }
            b'"' => Ok(Value::str(self.string()?)),
            b't' if self.s[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            b'f' if self.s[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            b'n' if self.s[self.pos..].starts_with(b"null") => {
                self.pos += 4;
                Ok(it.none())
            }
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                if c == b'-' {
                    self.pos += 1;
                }
                let mut float = false;
                while self.pos < self.s.len() {
                    match self.s[self.pos] {
                        b'0'..=b'9' => {}
                        b'.' | b'e' | b'E' | b'+' | b'-' => float = true,
                        _ => break,
                    }
                    self.pos += 1;
                }
                let text = std::str::from_utf8(&self.s[start..self.pos]).unwrap_or("");
                let digits = text.trim_start_matches('-');
                if digits.len() > 1 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit() {
                    return self.err("numbers may not have leading zeros");
                }
                if !json_number(text) {
                    self.pos = start;
                    return self.err(&format!("invalid number `{}`", text));
                }
                if !float {
                    if let Ok(n) = text.parse::<i64>() {
                        return Ok(Value::Int(n));
                    }
                }
                // Integers beyond Int's range become Floats; numbers beyond
                // Float's range are errors, as for `parse_float`.
                match text.parse::<f64>() {
                    Ok(f) if f.is_finite() => Ok(Value::Float(f)),
                    Ok(_) => self.err("number too large"),
                    Err(_) => Err(format!("invalid number `{}`", text)),
                }
            }
            _ => self.err("unexpected character"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.s.get(self.pos) else { return self.err("unterminated string") };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    let Some(&e) = self.s.get(self.pos + 1) else { return self.err("unterminated escape") };
                    self.pos += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.pos..self.pos + 4).unwrap_or(b"")).unwrap_or("");
                            let mut code = u32::from_str_radix(hex, 16).map_err(|_| "invalid \\u escape".to_string())?;
                            self.pos += 4;
                            if (0xD800..0xDC00).contains(&code) && self.s.get(self.pos..self.pos + 2) == Some(b"\\u") {
                                let hex2 = std::str::from_utf8(self.s.get(self.pos + 2..self.pos + 6).unwrap_or(b"")).unwrap_or("");
                                if let Ok(low) = u32::from_str_radix(hex2, 16) {
                                    if (0xDC00..0xE000).contains(&low) {
                                        code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                        self.pos += 6;
                                    }
                                }
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return self.err("invalid escape"),
                    }
                }
                _ => {
                    // Copy the run of text up to the next quote or escape
                    // (ASCII, so the run ends on a character boundary).
                    let run = self.s[self.pos..].iter().position(|&b| b == b'"' || b == b'\\').unwrap_or(self.s.len() - self.pos);
                    let text = std::str::from_utf8(&self.s[self.pos..self.pos + run]).map_err(|_| "invalid UTF-8".to_string())?;
                    out.push_str(text);
                    self.pos += run;
                }
            }
        }
    }
}

fn b_parse_json(it: &mut Interp, a: Vec<Value>, sp: Span) -> R {
    let s = str_arg(it, &a, 0, "parse_json", sp)?.to_string();
    let mut p = JsonParser { s: s.as_bytes(), pos: 0 };
    let r = p.value(it, 0).and_then(|v| {
        p.ws();
        if p.pos != p.s.len() {
            p.err("unexpected trailing characters")
        } else {
            Ok(v)
        }
    });
    Ok(match r {
        Ok(v) => it.ok(v),
        Err(m) => it.err_val(Value::str(m)),
    })
}
