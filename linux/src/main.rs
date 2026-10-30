//! Shared Clipboard for Linux (Wayland and X11): `sharedclip daemon` is the background service; the other commands talk to
//! it over its control socket.

mod bluetooth;
mod clip;
mod daemon;
mod x11;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

const USAGE: &str = "usage: sharedclip <command>
  daemon                run the background service (systemctl --user enable --now sharedclip)
  pair                  show this device's pairing link and QR code, and wait for a device
  pair <link>           pair with the device showing <link>
  pair --nearby [<n>]   list devices found nearby, or pair with number <n> by comparing a code
  devices               list the paired devices
  unpair <id or name>   forget a device
  history               the last items
  pause | resume        stop or restart syncing
  --version             print the version";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => println!("sharedclip {}", env!("CARGO_PKG_VERSION")),
        Some("daemon") => daemon::run(),
        Some("pair" | "devices" | "unpair" | "history" | "pause" | "resume") => {
            std::process::exit(request(&args.join(" ")))
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

/// The daemon's control socket.
fn socket_path() -> PathBuf {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        eprintln!("sharedclip: XDG_RUNTIME_DIR is not set");
        std::process::exit(1);
    };
    PathBuf::from(dir).join("sharedclip.sock")
}

/// Sends one request to the daemon and prints the answer, asking the user when it wants a confirmation.
fn request(line: &str) -> i32 {
    let Ok(mut sock) = UnixStream::connect(socket_path()) else {
        eprintln!("sharedclip: the daemon isn't running; start it with: systemctl --user start sharedclip");
        return 1;
    };
    let mut status = 0;
    let reader = sock.try_clone().map(BufReader::new);
    let result = writeln!(sock, "{line}").and(reader).and_then(|reader| {
        for line in reader.lines() {
            let line = line?;
            if let Some((id, prompt)) = line.strip_prefix("confirm ").and_then(|r| r.split_once(' ')) {
                print!("{prompt}");
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                let yes = matches!(answer.trim(), "y" | "Y" | "yes");
                writeln!(sock, "{} {id}", if yes { "y" } else { "n" })?;
            } else {
                status |= i32::from(line.starts_with("error: "));
                println!("{line}");
            }
        }
        Ok(())
    });
    if let Err(err) = result {
        eprintln!("sharedclip: {err}");
        return 1;
    }
    status
}
