//! Shared Clipboard for Linux (Wayland).

#[allow(dead_code)]
mod clip;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => println!("sharedclip {}", env!("CARGO_PKG_VERSION")),
        _ => eprintln!("usage: sharedclip --version"),
    }
}
