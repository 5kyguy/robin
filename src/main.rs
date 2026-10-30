fn main() {
    match robin::cli::run(std::env::args_os().skip(1)) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("robin: {err}");
            std::process::exit(1);
        }
    }
}
