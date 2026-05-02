fn main() {
    let want_version = std::env::args().any(|a| a == "--version" || a == "-V");
    if want_version {
        println!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
        return;
    }
    eprintln!(
        "{} {} (skeleton; nothing implemented yet)",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION")
    );
}
