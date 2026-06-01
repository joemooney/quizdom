// trace:EPIC-9 | ai:claude
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("contradictions") => quizdom::run_contradictions(args, &mut std::io::stdout()),
        // trace:STORY-72 | ai:claude
        Some("curate") => quizdom::run_curate(args, &mut std::io::stdout()),
        // trace:STORY-77 | ai:claude
        Some("session") if args.get(1).map(String::as_str) == Some("show") => {
            quizdom::run_session_show(args, &mut std::io::stdout())
        }
        _ => quizdom::run_cli(args, std::io::stdin(), std::io::stdout()),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
