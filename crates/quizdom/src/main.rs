// trace:EPIC-9 | ai:claude
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // trace:TASK-199 | ai:claude — top-level `--help`/`-h` (and bare `help`)
    // prints the full command list on stdout and exits 0; each subcommand
    // keeps its own `-h`.
    if matches!(
        args.first().map(String::as_str),
        Some("--help" | "-h" | "help")
    ) {
        println!("{}", quizdom::top_level_usage());
        return;
    }
    let result = match args.first().map(String::as_str) {
        Some("contradictions") => quizdom::run_contradictions(args, &mut std::io::stdout()),
        // trace:STORY-72 | ai:claude
        Some("curate") => quizdom::run_curate(args, &mut std::io::stdout()),
        // trace:STORY-205 | ai:claude
        Some("db-init") => quizdom::run_db_init(args, &mut std::io::stdout()),
        // trace:STORY-206 | ai:claude
        Some("db-migrate") => quizdom::run_db_migrate(args, &mut std::io::stdout()),
        // trace:STORY-261 | ai:claude — the TASK-243 durability path.
        Some("db-backup") => quizdom::run_db_backup(args, &mut std::io::stdout()),
        Some("db-restore") => quizdom::run_db_restore(args, &mut std::io::stdout()),
        // trace:STORY-87 | ai:claude
        Some("question") if args.get(1).map(String::as_str) == Some("add") => {
            quizdom::run_question_add(args, std::io::stdin(), std::io::stdout())
        }
        // trace:STORY-77 | ai:claude
        Some("session") if args.get(1).map(String::as_str) == Some("show") => {
            quizdom::run_session_show(args, &mut std::io::stdout())
        }
        // trace:STORY-128 | ai:claude
        Some("session") if args.get(1).map(String::as_str) == Some("synopsis") => {
            quizdom::run_session_synopsis(args, &mut std::io::stdout())
        }
        _ => quizdom::run_cli(args, std::io::stdin(), std::io::stdout()),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
