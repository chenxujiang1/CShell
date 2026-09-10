use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("version" | "--version" | "-V") => {
            println!("cshell-cli {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("status") => {
            println!("daemon status transport is not wired yet (P0-020 in progress)");
            ExitCode::SUCCESS
        }
        _ => {
            println!("Usage: cshell-cli <status|version>");
            ExitCode::SUCCESS
        }
    }
}
