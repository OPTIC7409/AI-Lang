#!/usr/bin/env python3
"""Build the Cogito playground: compile the interpreter to WebAssembly and
write web/dist/index.html (plus cogito.wasm next to it).

    python3 web/build.py            # page loads ./cogito.wasm
    python3 web/build.py --inline   # one self-contained page (wasm in base64)
"""
import base64, json, pathlib, shutil, subprocess, sys

WEB = pathlib.Path(__file__).resolve().parent
ROOT = WEB.parent

# (title, file, action, hint)
EXAMPLES = [
    ("A tour of Cogito", "web/examples/tour.cog", "run",
     "Records, enums, pattern matching and Results. Press Test to run the test and the property at the bottom."),
    ("Contracts find a bug", "web/examples/contracts.cog", "verify",
     "Verify generated inputs until the promise broke, then shrank the failure to a small counterexample. Fix it as the comment says."),
    ("Errors explain themselves", "web/examples/errors.cog", "run",
     "The type error is found before the program runs, so nothing is printed until it is fixed."),
    ("Reading input", "web/examples/stdin.cog", "run",
     "This program reads the Standard input box below the editor."),
    ("Bank transfers", "examples/bank.cog", "run",
     "Contracts on money transfers. Press Verify to check them, or Test to check that money is conserved."),
    ("Bounded stack", "examples/stack.cog", "verify",
     "Contracts that describe how each operation changes the stack, using old(...)."),
    ("Shapes", "examples/shapes.cog", "run", "Enums, exhaustive matching and overloading."),
    ("Calculator", "examples/calculator.cog", "run", "A tokenizer, parser and evaluator for arithmetic, written in Cogito."),
    ("Game of Life", "examples/life.cog", "run", "Conway's Game of Life on a small torus."),
    ("Eight queens", "examples/queens.cog", "run", "The N-queens puzzle, by backtracking."),
    ("Binary search tree", "examples/bst.cog", "test", "A generic tree whose invariants are checked by properties."),
    ("Sorting algorithms", "examples/sorting.cog", "test", "Classic sorts, each checked against the built-in sort by property tests."),
    ("Prime sieve", "examples/primes.cog", "run", "The sieve of Eratosthenes, checked against trial division."),
    ("Word frequencies", "examples/words.cog", "run", "Strings, maps and pipelines."),
    ("JSON inventory", "examples/inventory.cog", "run", "Parsing JSON and validating it with Result."),
    ("FizzBuzz", "examples/fizzbuzz.cog", "run", "FizzBuzz with a match on a tuple."),
]
STDIN = {"web/examples/stdin.cog": "3.5\n4\nseven\n10.25\n"}


def main():
    inline = "--inline" in sys.argv
    subprocess.run(["cargo", "build", "--release"], cwd=WEB, check=True)
    wasm = WEB / "target/wasm32-unknown-unknown/release/cogito_web.wasm"
    dist = WEB / "dist"
    dist.mkdir(exist_ok=True)
    examples = [
        {"title": t, "code": (ROOT / f).read_text(), "action": a, "hint": h, **({"stdin": STDIN[f]} if f in STDIN else {})}
        for t, f, a, h in EXAMPLES
    ]
    page = (WEB / "playground.html").read_text()
    if inline:
        loader = "window.COGITO_WASM_BASE64 = %s;" % json.dumps(base64.b64encode(wasm.read_bytes()).decode())
    else:
        loader = 'window.COGITO_WASM_URL = "cogito.wasm";'
        shutil.copy(wasm, dist / "cogito.wasm")
    # `</` cannot appear inside a <script> element.
    data = json.dumps(examples, ensure_ascii=False).replace("</", "<\\/")
    page = page.replace("/*WASM_URL*/", loader).replace("/*EXAMPLES*/", "window.COGITO_EXAMPLES = %s;" % data)
    (dist / "index.html").write_text(page)
    print("wrote", dist / "index.html", "(%d KB)" % (len(page) // 1024))


if __name__ == "__main__":
    main()
