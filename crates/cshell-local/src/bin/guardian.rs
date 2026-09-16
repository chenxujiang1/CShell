#[cfg(unix)]
fn main() {
    match cshell_local::run_pty_guardian_from_args() {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {
            eprintln!("cshell-pty-guardian requires its private guardian arguments");
            std::process::exit(2);
        }
        Err(error) => {
            eprintln!("cshell-pty-guardian failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("cshell-pty-guardian is only available on Unix platforms");
    std::process::exit(2);
}
