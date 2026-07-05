use mojo_lite::{check, lex, parse, Evaluator};
use std::io::Read;
use std::process::ExitCode;

/// mojo-lite doubles as a small **syntax-analysis tool**. With no arguments it
/// runs the built-in demo; otherwise the first argument selects a pipeline stage
/// to run over a file (or stdin), so you can inspect the tokens or the AST:
///
/// ```text
/// mojo-lite lex   [FILE]   # the token stream, one per line
/// mojo-lite parse [FILE]   # the parsed AST (pretty-printed)
/// mojo-lite check [FILE]   # lex + parse + type-check; report ok / the error
/// mojo-lite run   [FILE]   # the full pipeline; print output + final bindings
/// mojo-lite demo           # the built-in showcase (also the no-arg default)
/// ```
///
/// A `FILE` of `-`, or its absence, reads from standard input.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, file) = (args.first().map(String::as_str), args.get(1).map(String::as_str));
    match command {
        None | Some("demo") => {
            run_demo();
            ExitCode::SUCCESS
        }
        Some("lex") => stage("lex", file, run_lex),
        Some("parse") => stage("parse", file, run_parse),
        Some("check") => stage("check", file, run_check),
        Some("run") => stage("run", file, run_source),
        Some("-h" | "--help" | "help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command '{}'\n", other);
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprint!(
        "mojo-lite — a lexer/parser/checker/evaluator for a subset of Mojo\n\n\
         usage: mojo-lite [COMMAND] [FILE]\n\n\
         commands:\n\
         \x20 lex   [FILE]   print the token stream (one per line)\n\
         \x20 parse [FILE]   print the parsed AST\n\
         \x20 check [FILE]   type-check and report ok or the first error\n\
         \x20 run   [FILE]   evaluate and print output + final bindings\n\
         \x20 demo           run the built-in showcase (default)\n\n\
         FILE defaults to '-' (standard input).\n"
    );
}

/// Read the named file, or standard input when it is absent or `-`.
fn read_source(file: Option<&str>) -> std::io::Result<String> {
    match file {
        None | Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
        Some(path) => std::fs::read_to_string(path),
    }
}

/// Run one stage over the source, turning any I/O or stage error into a non-zero
/// exit code with a message on stderr.
fn stage(name: &str, file: Option<&str>, f: fn(&str) -> Result<(), String>) -> ExitCode {
    let source = match read_source(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: cannot read input: {}", name, e);
            return ExitCode::FAILURE;
        }
    };
    match f(&source) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} error: {}", name, e);
            ExitCode::FAILURE
        }
    }
}

/// `lex`: print every token, one per line.
fn run_lex(source: &str) -> Result<(), String> {
    let tokens = lex(source).map_err(|e| e.to_string())?;
    for tok in tokens {
        println!("{:?}", tok);
    }
    Ok(())
}

/// `parse`: print the parsed AST.
fn run_parse(source: &str) -> Result<(), String> {
    let program = parse(source).map_err(|e| e.to_string())?;
    println!("{:#?}", program);
    Ok(())
}

/// `check`: lex + parse + type-check; report success or the first error.
fn run_check(source: &str) -> Result<(), String> {
    let program = parse(source).map_err(|e| e.to_string())?;
    check(&program).map_err(|e| e.to_string())?;
    println!("ok");
    Ok(())
}

/// `run`: the full pipeline — evaluate, then print captured output + bindings.
fn run_source(source: &str) -> Result<(), String> {
    let program = parse(source).map_err(|e| e.to_string())?;
    check(&program).map_err(|e| e.to_string())?;
    let mut evaluator = Evaluator::new();
    evaluator.eval_program(&program).map_err(|e| e.to_string())?;
    let output = evaluator.output();
    if !output.is_empty() {
        print!("{}", output);
    }
    for (n, v) in evaluator.global_bindings() {
        println!("{} = {}", n, v);
    }
    Ok(())
}

fn run_demo() {
    let programs = vec![
        (
            "Variables: annotated and inferred",
            "var a: Int = 1\nvar ok: Bool = True\nvar greeting: String = \"hello\"\nvar count = 10\nvar ratio = 3.5\nvar names = [\"ada\", \"alan\"]\n",
        ),
        (
            "Operators and precedence",
            "var a: Int = 1 + 2 * 3\nvar b: Int = (1 + 2) * 3\nvar c: Bool = a < b and not False\nvar d: Int = -a + 10\n",
        ),
        (
            "Functions with arithmetic",
            "def add(x: Int, y: Int) -> Int:\n    return x + y\n\ndef square(n: Int) -> Int:\n    return n * n\n\nvar sum: Int = add(3, 4)\nvar sq: Int = square(add(1, 2))\n",
        ),
        (
            "Lexical capture (downward funarg)",
            "def adder(n: Int) -> Int:\n    def add_n(x: Int) -> Int:\n        return x + n\n    return add_n(100)\n\nvar c: Int = adder(42)\n",
        ),
        (
            "Shadowing",
            "var x: Int = 1\ndef shadowed() -> Int:\n    var x: Int = 99\n    return x\n\nvar outer_x: Int = x\nvar inner_x: Int = shadowed()\n",
        ),
        (
            "Conditionals (if/elif/else)",
            "def sign(n: Int) -> Int:\n    if n > 0:\n        return 1\n    elif n < 0:\n        return -1\n    else:\n        return 0\n\nvar pos: Int = sign(7)\nvar neg: Int = sign(-4)\n",
        ),
        (
            "for over range() with continue",
            "def first_at_least(threshold: Int, n: Int) -> Int:\n    for i in range(n):\n        if i < threshold:\n            continue\n        return i\n    return -1\n\nvar r: Int = first_at_least(4, 10)\n",
        ),
        (
            "Structs: fields, methods, construction",
            "@fieldwise_init\nstruct Point:\n    var x: Int\n    var y: Int\n\n    def sum(self) -> Int:\n        return self.x + self.y\n\n    def scaled(self, k: Int) -> Int:\n        return self.sum() * k\n\nvar p: Point = Point(3, 4)\nvar s: Int = p.sum()\nvar sc: Int = p.scaled(10)\nvar px: Int = p.x\n",
        ),
        (
            "Numbers: literal coercion and // % ** operators",
            "var u: UInt = 0\nu = u + 1\nvar half: Float64 = 1 / 2\nvar f: Float64 = 3\nvar fdiv: Int = -7 // 2\nvar md: Int = -7 % 2\nvar pw: Int = 2 ** 10\n",
        ),
        (
            "Mutation: accumulate a sum in a loop",
            "def sum_to(n: Int) -> Int:\n    var total: Int = 0\n    for i in range(n):\n        total = total + i\n    return total\n\nvar s: Int = sum_to(5)\n",
        ),
        (
            "Mutation: while loop driven by reassignment",
            "var x: Int = 0\nwhile x < 5:\n    x = x + 1\n",
        ),
        (
            "Generics: a generic Pair[T] and a generic function",
            "@fieldwise_init\nstruct Pair[T: Copyable & Movable]:\n    var left: Self.T\n    var right: Self.T\n\ndef first[T: Copyable & Movable](p: Pair[T]) -> T:\n    return p.left\n\nvar pi: Pair[Int] = Pair(3, 4)\nvar pf: Pair[Float64] = Pair(1.5, 2.5)\nvar a: Int = first(pi)\nvar b: Float64 = pf.right\n",
        ),
        (
            "Traits: a trait, a conforming struct, and a bounded generic",
            "trait Quackable:\n    def quack(self) -> String:\n        ...\n\n@fieldwise_init\nstruct Duck(Quackable):\n    var name: String\n\n    def quack(self) -> String:\n        return \"Quack!\"\n\ndef make_it_quack[T: Quackable](x: T) -> String:\n    return x.quack()\n\nvar donald: Duck = Duck(\"Donald\")\nvar sound: String = make_it_quack(donald)\n",
        ),
        (
            "Value parameters + comptime (FixedBuffer[size])",
            "comptime DEFAULT = 4 * 2\n\n@fieldwise_init\nstruct FixedBuffer[size: Int]:\n    var tag: Int\n\n    def capacity(self) -> Int:\n        return Self.size\n\ndef doubled[n: Int]() -> Int:\n    return n * 2\n\nvar small: FixedBuffer[DEFAULT] = FixedBuffer[DEFAULT](0)\nvar big: FixedBuffer[2 ** 5] = FixedBuffer[32](1)\nvar cap: Int = big.capacity()\nvar d: Int = doubled[21]()\n",
        ),
        (
            "SIMD: elementwise vectors, lane read/write, bit-accurate lanes",
            "var v: SIMD[DType.int32, 4] = SIMD[DType.int32, 4](1, 2, 3, 4)\nvar doubled: SIMD[DType.int32, 4] = v + v\nvar mask: SIMD[DType.bool, 4] = v < doubled\nvar third: Int32 = doubled[2]\nv[0] = 100\nv[3] += 10\nvar wrapped: SIMD[DType.int8, 2] = SIMD[DType.int8, 2](100) + SIMD[DType.int8, 2](100)\n",
        ),
        (
            "Float64 is SIMD[DType.float64, 1] (unified)",
            "var s: Float64 = 2.5\nvar fv: SIMD[DType.float64, 4] = SIMD[DType.float64, 4](1.0, 2.0, 3.0, 4.0)\nvar scaled: SIMD[DType.float64, 4] = fv * s\nfv[0] = s\nvar first: Float64 = fv[0]\n",
        ),
        (
            "List: build, mutate, iterate (value semantics)",
            "def squares(n: Int) -> List[Int]:\n    var xs: List[Int] = List[Int]()\n    for i in range(n):\n        xs.append(i * i)\n    return xs\n\nvar sq: List[Int] = squares(6)\nsq[0] = 100\nvar total: Int = 0\nfor x in sq:\n    total = total + x\nvar count: Int = len(sq)\nvar top: Int = sq.pop()\n",
        ),
        (
            "Tuple: heterogeneous, compile-time index",
            "def scan_stats() -> Tuple[Int, Int]:\n    var total_points = 512\n    var num_scans = 4\n    return (total_points, num_scans)\n\nvar stats = scan_stats()\nvar points: Int = stats[0]\nvar scans: Int = stats[1]\nvar mixed = (1, 2.5, \"ready\")\n",
        ),
        (
            "Augmented assignment (+= -= *= //= etc.)",
            "def factorial(n: Int) -> Int:\n    var acc: Int = 1\n    for i in range(1, n + 1):\n        acc *= i\n    return acc\n\nvar f: Int = factorial(5)\nvar xs: List[Int] = [10, 20, 30]\nxs[1] += 5\nvar msg: String = \"hi\"\nmsg += \"!\"\n",
        ),
        (
            "Member-write: mut self methods and place assignment",
            "@fieldwise_init\nstruct Account:\n    var balance: Int\n\n    def deposit(mut self, amount: Int):\n        self.balance = self.balance + amount\n\n    def withdraw(mut self, amount: Int):\n        self.balance = self.balance - amount\n\nvar acc: Account = Account(100)\nacc.deposit(50)\nacc.withdraw(30)\nvar bal: Int = acc.balance\nacc.balance = bal * 2\n\n@fieldwise_init\nstruct Portfolio:\n    var accounts: List[Account]\n\nvar p: Portfolio = Portfolio([Account(10), Account(20)])\np.accounts[0].deposit(5)\np.accounts.append(Account(30))\n",
        ),
        (
            "print: the output facility",
            "def fizzbuzz(n: Int):\n    for i in range(1, n):\n        if i % 15 == 0:\n            print(i, \"fizzbuzz\")\n        elif i % 3 == 0:\n            print(i, \"fizz\")\n        elif i % 5 == 0:\n            print(i, \"buzz\")\n\nfizzbuzz(16)\n",
        ),
        (
            "Exceptions: raise / try / except / else / finally",
            "def checked_div(a: Int, b: Int) raises -> Int:\n    if b == 0:\n        raise \"division by zero\"\n    return a // b\n\nvar result: Int = 0\nvar status: String = \"\"\ntry:\n    result = checked_div(10, 0)\nexcept e:\n    status = \"caught\"\nelse:\n    status = \"ok\"\nfinally:\n    status = status + \"!\"\n",
        ),
        (
            "Escaping closure is rejected",
            "def make() -> Int:\n    def helper() -> Int:\n        return 1\n    return helper\n\nvar bad: Int = make()\n",
        ),
        (
            "break outside a loop is rejected",
            "break\n",
        ),
        (
            "Default and keyword arguments, called from main()",
            "def my_pow(base: Int, exp: Int = 2) -> Int:\n    return base ** exp\n\ndef main():\n    print(my_pow(3))\n    print(my_pow(base=2, exp=10))\n",
        ),
    ];

    for (name, source) in programs {
        println!("=== {} ===", name);
        println!("{}", source);

        let program = match parse(source) {
            Ok(stmts) => stmts,
            Err(err) => {
                println!("parse error: {}\n", err);
                continue;
            }
        };

        if let Err(err) = check(&program) {
            println!("type error: {}\n", err);
            continue;
        }

        let mut evaluator = Evaluator::new();
        match evaluator.eval_program(&program) {
            Ok(()) => {
                let output = evaluator.output();
                if !output.is_empty() {
                    print!("output:\n{}", output);
                }
                println!("bindings:");
                for (n, v) in evaluator.global_bindings() {
                    println!("  {} = {}", n, v);
                }
            }
            Err(err) => println!("runtime error: {}", err),
        }
        println!();
    }
}
