//! Keep one crash report. Never serialize panic payloads or application state:
//! either may contain password input or credential request data.
use std::{backtrace::Backtrace, io::Write, path::PathBuf};

pub fn install() {
    let directory = std::env::var_os("CSHELL_LOG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CSHELL_RUNTIME_DIR").map(PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join("cshell"));
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or_else(
            || "unknown".into(),
            |location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            },
        );
        let backtrace = Backtrace::force_capture();
        let report = format!(
            "CShell Rust panic\nversion: {}\ntime: {:?}\nprocess: {}\nlocation: {location}\n{backtrace}\n",
            env!("CARGO_PKG_VERSION"),
            std::time::SystemTime::now(),
            std::process::id()
        );
        if std::fs::create_dir_all(&directory).is_ok()
            && let Ok(mut file) = std::fs::File::create(directory.join("gui-crash.log"))
        {
            let _ = file.write_all(report.as_bytes());
            let _ = file.sync_all();
        }
        eprintln!(
            "CShell panic at {location}; diagnostic: {}",
            directory.join("gui-crash.log").display()
        );
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn crash_probe_child() {
        if std::env::var_os("CSHELL_CRASH_PROBE").is_some() {
            super::install();
            panic!("PRIVATE-PASSWORD-PROBE");
        }
    }

    #[test]
    fn panic_report_records_location_without_password_payload() {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::crash_probe_child",
                "--nocapture",
            ])
            .env("CSHELL_CRASH_PROBE", "1")
            .env("CSHELL_LOG_DIR", directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let report = std::fs::read_to_string(directory.path().join("gui-crash.log")).unwrap();
        assert!(report.contains("diagnostics.rs:"));
        assert!(!report.contains("PRIVATE-PASSWORD-PROBE"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-PASSWORD-PROBE"));
    }
}
