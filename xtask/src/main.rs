use std::env;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let command = env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    let result = match command.as_str() {
        "check" => check(),
        "phase0-bench" => phase0_bench(),
        "pipeline-bench" => pipeline_bench(),
        "ipc-fanout-bench" => ipc_fanout_bench(),
        "atlas-pressure-bench" => atlas_pressure_bench(),
        "echo-latency-bench" => echo_latency_bench(),
        "native-ci-bench" => native_ci_bench(),
        "journal-capacity-bench" => journal_capacity_bench(),
        "journal-crash-matrix" => journal_crash_matrix(),
        "terminal-corpus" => terminal_corpus(),
        "visual-corpus" => visual_corpus(),
        "log-corpus" => log_corpus(),
        "log-window-e2e" => log_window_e2e(),
        "session-log" => session_log(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        unknown => Err(format!("unknown xtask command: {unknown}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn check() -> Result<(), String> {
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run(
        "cargo",
        &["test", "--workspace", "--all-targets", "--all-features"],
    )?;
    run("cargo", &["test", "--workspace", "--doc", "--all-features"])?;
    Ok(())
}

fn phase0_bench() -> Result<(), String> {
    run(
        "cargo",
        &[
            "bench",
            "--package",
            "cshell-output-store",
            "--bench",
            "journal_throughput",
        ],
    )?;
    run(
        "cargo",
        &[
            "bench",
            "--package",
            "cshell-render",
            "--bench",
            "glyph_geometry",
        ],
    )?;
    run(
        "cargo",
        &[
            "bench",
            "--package",
            "cshell-render",
            "--bench",
            "log_geometry",
        ],
    )
}

fn pipeline_bench() -> Result<(), String> {
    run_with_env(
        "cargo",
        &[
            "bench",
            "--package",
            "cshelld",
            "--bench",
            "pipeline_throughput",
        ],
        &[
            ("CSHELL_PIPELINE_SECONDS", "60"),
            ("CSHELL_PIPELINE_MIB_PER_SECOND", "55"),
            ("CSHELL_ENFORCE_PERF", "1"),
        ],
    )
}

fn ipc_fanout_bench() -> Result<(), String> {
    run_with_env(
        "cargo",
        &["bench", "--package", "cshelld", "--bench", "ipc_fanout"],
        &[
            ("CSHELL_IPC_FANOUT_CLIENTS", "100"),
            ("CSHELL_IPC_FANOUT_SECONDS", "60"),
            ("CSHELL_ENFORCE_PERF", "1"),
        ],
    )
}

fn atlas_pressure_bench() -> Result<(), String> {
    run_with_env(
        "cargo",
        &[
            "bench",
            "--package",
            "cshell-render",
            "--bench",
            "atlas_pressure",
        ],
        &[
            ("CSHELL_ATLAS_CHURN", "20000"),
            ("CSHELL_ENFORCE_PERF", "1"),
        ],
    )
}

fn echo_latency_bench() -> Result<(), String> {
    run_with_env(
        "cargo",
        &["bench", "--package", "cshelld", "--bench", "echo_latency"],
        &[
            ("CSHELL_ECHO_SAMPLES", "80"),
            ("CSHELL_ECHO_WARMUP", "10"),
            ("CSHELL_REQUIRE_GPU", "1"),
            ("CSHELL_ENFORCE_PERF", "1"),
        ],
    )
}

fn native_ci_bench() -> Result<(), String> {
    echo_latency_bench()?;
    run_with_env(
        "cargo",
        &["bench", "--package", "cshelld", "--bench", "ipc_fanout"],
        &[
            ("CSHELL_IPC_FANOUT_CLIENTS", "100"),
            ("CSHELL_IPC_FANOUT_SECONDS", "15"),
            ("CSHELL_ENFORCE_PERF", "1"),
        ],
    )?;
    atlas_pressure_bench()?;
    run_with_env(
        "cargo",
        &[
            "bench",
            "--package",
            "cshell-output-store",
            "--bench",
            "journal_throughput",
        ],
        &[("CSHELL_SCALE_MIB", "1024"), ("CSHELL_ENFORCE_PERF", "1")],
    )
}

fn journal_capacity_bench() -> Result<(), String> {
    run_with_env(
        "cargo",
        &[
            "bench",
            "--package",
            "cshell-output-store",
            "--bench",
            "journal_throughput",
        ],
        &[("CSHELL_SCALE_MIB", "102400"), ("CSHELL_ENFORCE_PERF", "1")],
    )
}

fn journal_crash_matrix() -> Result<(), String> {
    run(
        "cargo",
        &[
            "test",
            "--package",
            "cshell-output-store",
            "forced_process_termination_recovers_committed_journal_matrix",
            "--",
            "--nocapture",
        ],
    )
}

fn terminal_corpus() -> Result<(), String> {
    run(
        "cargo",
        &[
            "test",
            "--package",
            "cshell-terminal",
            "--test",
            "xterm_cell_corpus",
            "--all-features",
        ],
    )
}

fn visual_corpus() -> Result<(), String> {
    run(
        "cargo",
        &["run", "--package", "cshell-gui", "--", "--visual-corpus"],
    )
}

fn log_corpus() -> Result<(), String> {
    run(
        "cargo",
        &["run", "--package", "cshell-gui", "--", "--log-corpus"],
    )
}

fn log_window_e2e() -> Result<(), String> {
    run(
        "cargo",
        &[
            "run",
            "--release",
            "--package",
            "cshell-gui",
            "--",
            "--log-window-e2e",
        ],
    )
}

fn session_log() -> Result<(), String> {
    run(
        "cargo",
        &["run", "--package", "cshell-gui", "--", "--session-log"],
    )
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    run_with_env(program, args, &[])
}

fn run_with_env(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<(), String> {
    println!("+ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .status()
        .map_err(|error| format!("cannot start {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn print_help() {
    println!("CShell development tasks");
    println!("  cargo xtask check         full-feature format, lint, test and doctest gate");
    println!("  cargo xtask phase0-bench  run the output journal throughput probe");
    println!("  cargo xtask pipeline-bench run the 60-second full output pipeline gate");
    println!("  cargo xtask ipc-fanout-bench run the 100-client IPC resource gate");
    println!("  cargo xtask atlas-pressure-bench run the full dynamic glyph-atlas gate");
    println!("  cargo xtask echo-latency-bench run PTY/SSH-to-GPU percentile gates");
    println!("  cargo xtask native-ci-bench run bounded four-platform resource gates");
    println!("  cargo xtask journal-capacity-bench run the 100-GiB journal capacity gate");
    println!("  cargo xtask journal-crash-matrix run forced-termination recovery cases");
    println!("  cargo xtask terminal-corpus run fragmented ANSI/OSC/xterm cell golden cases");
    println!("  cargo xtask visual-corpus launch the terminal Unicode/color visual corpus");
    println!("  cargo xtask log-corpus    launch the GPU LogSurface visual corpus");
    println!("  cargo xtask log-window-e2e run automated scroll/resize/window-present gate");
    println!("  cargo xtask session-log   launch the live daemon-backed session log");
}
