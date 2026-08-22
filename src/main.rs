#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = fero::run() {
        eprintln!("Fero konnte nicht starten: {error}");
        std::process::exit(1);
    }
}
