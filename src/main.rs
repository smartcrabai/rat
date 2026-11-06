fn main() {
    if let Err(error) = rat::cli::run() {
        eprintln!("rat: {error:#}");
        std::process::exit(1);
    }
}
