// trace:EPIC-9 | ai:claude
fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let result = if args.peek().map(String::as_str) == Some("contradictions") {
        quizdom::run_contradictions(args, &mut std::io::stdout())
    } else {
        quizdom::run_cli(args, std::io::stdin(), std::io::stdout())
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
