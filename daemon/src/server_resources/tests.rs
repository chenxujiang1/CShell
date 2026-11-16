#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
mod ssh;

const SAMPLE: &str = "CSHELL-RESOURCES-1\ncpu 100 0 50 800 20 10 10 10 0 0\ncpu 120 0 60 860 20 10 15 15 0 0\nMemTotal: 100000 kB\nMemAvailable: 40000 kB\nSwapTotal: 10000 kB\nSwapFree: 8000 kB\nload 0.10 1.20 2.300 1/100 1234\nuptime 12345.67 10000.00\nEND-CSHELL-RESOURCES\n";

#[test]
fn parses_cpu_deltas_and_memory_without_including_guest_twice() {
    let sample = parse::parse(SAMPLE.as_bytes()).unwrap();
    assert_eq!(sample.cpu_basis_points, Some(4000));
    assert_eq!(sample.memory_available_kib, 40000);
    assert_eq!(sample.load_milli, [100, 1200, 2300]);
    assert_eq!(sample.uptime_seconds, 12345);
    let with_guests = SAMPLE.replace("10 10 0 0\n", "10 10 9999 9999\n");
    assert_eq!(
        parse::parse(with_guests.as_bytes())
            .unwrap()
            .cpu_basis_points,
        Some(4000)
    );
}

#[test]
fn rejects_missing_duplicate_overflow_and_noncanonical_output() {
    for bad in [
        SAMPLE.replace("MemAvailable: 40000", "MemTotal: 40000"),
        SAMPLE.replace("MemAvailable: 40000", "MemAvailable: 100001"),
        SAMPLE.replace("MemTotal: 100000", "MemTotal: 0"),
        SAMPLE.replace("load 0.10", "load NaN"),
        SAMPLE.replace("load 0.10", "load 999999999999999.00"),
        SAMPLE.replace("SwapFree: 8000", "SwapFree: 10001"),
        SAMPLE.replace("MemTotal: 100000", "MemTotal: 18446744073709551616"),
        format!("banner\n{SAMPLE}"),
        format!("{SAMPLE}extra\n"),
        "x".repeat(4097),
    ] {
        assert!(parse::parse(bad.as_bytes()).is_none());
    }
    let mut sample = parse::parse(SAMPLE.as_bytes()).unwrap();
    assert!(!sample.validate(SessionId::new()));
    sample.cpu_basis_points = Some(10001);
    assert!(!sample.validate(sample.session));
}

#[test]
fn unavailable_cpu_is_not_falsely_reported_as_zero() {
    let zero = SAMPLE.replace(
        "cpu 120 0 60 860 20 10 15 15 0 0",
        "cpu 100 0 50 800 20 10 10 10 0 0",
    );
    assert_eq!(
        parse::parse(zero.as_bytes()).unwrap().cpu_basis_points,
        None
    );
    let rollback = SAMPLE.replace("cpu 120", "cpu 90");
    assert_eq!(
        parse::parse(rollback.as_bytes()).unwrap().cpu_basis_points,
        None
    );
}

#[tokio::test]
async fn resources_request_cannot_smuggle_commands_or_unrelated_options() {
    use cshell_ipc::{
        TerminalToolsOperation as Op, TerminalToolsRequest as Request,
        TerminalToolsStatus as Status,
    };
    let directory = tempfile::tempdir().unwrap();
    let local = Arc::new(crate::LocalSessionRegistry::new(directory.path(), 16).unwrap());
    let ssh = Arc::new(SshSessionRegistry::default());
    let service = crate::terminal_tools_ipc::TerminalToolsService::new();
    let base = Request {
        operation: Op::ServerResources as i32,
        session_id: SessionId::new().as_uuid().as_bytes().to_vec(),
        ..Default::default()
    };
    let forbidden = [
        Request {
            document_json: b"arbitrary command".to_vec(),
            ..base.clone()
        },
        Request {
            cursor: 1,
            ..base.clone()
        },
        Request {
            confirmed: true,
            ..base.clone()
        },
        Request {
            allow_text: true,
            ..base.clone()
        },
        Request {
            expected_revision: 1,
            ..base.clone()
        },
        Request {
            offset: 1,
            ..base.clone()
        },
        Request {
            session_id: vec![0; 16],
            ..base.clone()
        },
    ];
    for request in forbidden {
        assert_eq!(
            service.handle(&request, &local, &ssh, None).await.status,
            Status::Invalid as i32
        );
    }
    assert_eq!(
        service.handle(&base, &local, &ssh, None).await.status,
        Status::Unavailable as i32
    );
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn samples_actual_linux_proc_without_creating_files() {
    let result = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(SCRIPT)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert!(parse::parse(&result.stdout).is_some());
}
