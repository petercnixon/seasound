fn main() {
    if let Err(err) = seasound_ui::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
