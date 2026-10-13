mod host;

fn main() {
    if let Err(e) = host::execute() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
