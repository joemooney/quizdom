fn main() {
    if let Err(error) = quizdom::run_cli(
        std::env::args().skip(1),
        std::io::stdin(),
        std::io::stdout(),
    ) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
