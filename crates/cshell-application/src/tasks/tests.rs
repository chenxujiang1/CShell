use super::*;
use crate::{CatalogSnapshot, HostCatalogSnapshot, freeze_hosts};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
};

#[cfg(windows)]
#[test]
fn native_windows_shells_preserve_arguments_environment_streams_and_exit_codes()
-> Result<(), Box<dyn Error>> {
    use std::{os::windows::process::CommandExt, process::Command};
    let original = fixture(1)?;
    let mut definition = original.definition().clone();
    definition.shell = RemoteShellProfile::PowerShell;
    let value = "a'; Write-Output INJECTED; 'b";
    definition.variables.insert("VALUE".into(), value.into());
    definition.steps[0].command = TaskCommand::Invocation {
        program: "Write-Output".into(),
        arguments: vec![TaskArgument::Variable {
            name: "VALUE".into(),
        }],
    };
    let execute = |command: &str| -> Result<std::process::Output, std::io::Error> {
        let payload = command
            .strip_prefix("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ")
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "not an encoded fixture command",
                )
            })?;
        Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                payload,
            ])
            .creation_flags(0x08000000)
            .output()
    };
    let output = execute(&compile_task_commands(&definition)?[0])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), value);
    definition.steps[0].command=TaskCommand::Script {text:"[Console]::OutputEncoding=[Text.UTF8Encoding]::new(); Write-Output '\u{6570}\u{636e}\u{1f600}'; exit 17".into()};
    let output = execute(&compile_task_commands(&definition)?[0])?;
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        "\u{6570}\u{636e}\u{1f600}"
    );
    definition.shell = RemoteShellProfile::Cmd;
    definition
        .environment
        .insert("CSHELL_TASK_FIXTURE".into(), "alpha beta".into());
    definition.steps[0].command = TaskCommand::Script {
        text: "echo %CSHELL_TASK_FIXTURE% & echo STDERR_MARKER 1>&2 & exit /b 19".into(),
    };
    let output = execute(&compile_task_commands(&definition)?[0])?;
    assert_eq!(
        output.status.code(),
        Some(19),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "alpha beta");
    assert_eq!(String::from_utf8(output.stderr)?.trim(), "STDERR_MARKER");
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_posix_invocation_keeps_untrusted_values_in_one_argument() -> Result<(), Box<dyn Error>> {
    let original = fixture(1)?;
    let mut definition = original.definition().clone();
    let value = "a'; printf INJECTED; '\n\u{6570}\u{636e}";
    definition.variables.insert("VALUE".into(), value.into());
    definition.steps[0].command = TaskCommand::Invocation {
        program: "printf".into(),
        arguments: vec![
            TaskArgument::Literal { text: "%s".into() },
            TaskArgument::Variable {
                name: "VALUE".into(),
            },
        ],
    };
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &compile_task_commands(&definition)?[0]])
        .output()?;
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout)?, value);
    Ok(())
}

pub(super) fn fixture(count: usize) -> Result<TaskSnapshot, Box<dyn Error>> {
    let id = ProfileId::new();
    let profiles = CatalogSnapshot {
        revision: 1,
        profiles: vec![ProfileRecord {
            id,
            name: "template".into(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: TerminalOverrides::default(),
        }],
        ssh_connections: vec![SshConnectionRecord {
            options: Default::default(),
            profile_id: id,
            host: "template.test".into(),
            port: 22,
            username: "operator".into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: SshAgentBackend::OpenSsh,
            agent_identity: None,
            route: SshRoute::Direct,
        }],
        ..Default::default()
    };
    let hosts = HostCatalogSnapshot {
        revision: 1,
        catalog: HostCatalog {
            hosts: (0..count)
                .map(|i| HostRecord {
                    id: HostId::new(),
                    name: format!("host-{i}"),
                    address: format!("host-{i}.test"),
                    port: 22,
                    profile_id: id,
                    tags: BTreeSet::new(),
                    labels: BTreeMap::new(),
                    platform_hint: HostPlatformHint::Unknown,
                })
                .collect(),
            ..Default::default()
        },
    };
    let frozen = freeze_hosts(
        &hosts,
        &profiles,
        HostSelection::Selector {
            expression: "all".into(),
        },
        1000,
    )?;
    let definition = TaskDefinition {
        id: TaskId::new(),
        name: "check".into(),
        shell: RemoteShellProfile::Posix,
        steps: vec![TaskStep {
            name: "check".into(),
            command: TaskCommand::Script {
                text: "printf PRIVATE_BODY".into(),
            },
            success_codes: BTreeSet::from([0]),
            idempotent: true,
        }],
        variables: BTreeMap::new(),
        environment: BTreeMap::new(),
        policy: TaskPolicy::default(),
    };
    Ok(TaskSnapshot::new(
        definition,
        frozen,
        "operator".into(),
        1000,
    )?)
}
fn apply(record: &mut TaskRunRecord, action: TaskRunAction, now: u64) -> Result<(), TaskError> {
    let patch = record.prepare(action, now)?;
    record.apply_committed(&patch, patch.after().revision)
}

#[test]
fn immutable_snapshot_and_deterministic_canary_batches_validate_restore_and_hide_bodies()
-> Result<(), Box<dyn Error>> {
    let original = fixture(1000)?;
    let mut definition = original.definition().clone();
    definition.policy.canary_count = 2;
    definition.policy.batch_size = Some(300);
    let frozen = TaskSnapshot::new(
        definition,
        original.targets().clone(),
        "operator".into(),
        1000,
    )?;
    assert_eq!(
        frozen.batches().iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 300, 300, 300, 98]
    );
    assert_eq!(
        decode_task_snapshot(&encode_task_snapshot(&frozen)?)?,
        frozen
    );
    assert!(!format!("{frozen:?}").contains("PRIVATE_BODY"));
    assert!(!format!("{:?}", frozen.definition()).contains("PRIVATE_BODY"));
    let mut document: serde_json::Value = serde_json::from_slice(&encode_task_snapshot(&frozen)?)?;
    document["commands"][0] = "altered".into();
    assert!(decode_task_snapshot(&serde_json::to_vec(&document)?).is_err());
    assert!(decode_task_snapshot(&vec![0; MAX_TASK_SNAPSHOT_BYTES + 1]).is_err());
    let mut reordered: serde_json::Value = serde_json::from_slice(&encode_task_snapshot(&frozen)?)?;
    reordered["targets"]["hosts"]
        .as_array_mut()
        .ok_or("targets")?
        .reverse();
    assert!(decode_task_snapshot(&serde_json::to_vec(&reordered)?).is_err());
    Ok(())
}

#[test]
fn sent_command_timeouts_cancel_disconnect_and_restart_are_unknown_and_retry_is_a_new_run()
-> Result<(), Box<dyn Error>> {
    for reason in [
        TaskStopReason::UserCancelled,
        TaskStopReason::StepTimeout,
        TaskStopReason::TotalTimeout,
        TaskStopReason::TransportLost,
        TaskStopReason::OutputFailure,
        TaskStopReason::DaemonRestarted,
    ] {
        let mut record = TaskRunRecord::new(fixture(2)?)?;
        let host = record.targets()[0].host_id;
        apply(&mut record, TaskRunAction::Start, 1001)?;
        apply(&mut record, TaskRunAction::Connect { host }, 1002)?;
        apply(
            &mut record,
            TaskRunAction::StepIntent { host, step: 0 },
            1003,
        )?;
        apply(
            &mut record,
            TaskRunAction::AbortTarget { host, reason },
            1004,
        )?;
        assert_eq!(
            record.target(host).map(|t| t.state),
            Some(TargetState::Unknown)
        );
        assert_eq!(
            record.target(host).map(|t| t.steps[0].exit_code),
            Some(None)
        );
        let ids = BTreeSet::from([host]);
        assert_eq!(
            TaskSnapshot::retry(&record, &ids, false, "operator".into(), 1005),
            Err(TaskError::UnsafeRetry)
        );
        let retry = TaskSnapshot::retry(&record, &ids, true, "operator".into(), 1005)?;
        assert_ne!(retry.id(), record.id());
        assert_eq!(retry.retry_of(), Some(record.id()));
        assert_eq!(
            retry.targets().targets(),
            &record.snapshot().targets().targets()[..1]
        );
        assert_eq!(retry.commands(), record.snapshot().commands());
        assert_eq!(record.targets()[0].state, TargetState::Unknown);
    }
    Ok(())
}

#[test]
fn recovery_preserves_queued_targets_and_gates_and_never_retries_inflight_targets()
-> Result<(), Box<dyn Error>> {
    let mut record = TaskRunRecord::new(fixture(3)?)?;
    let first = record.targets()[0].host_id;
    let second = record.targets()[1].host_id;
    apply(&mut record, TaskRunAction::Start, 1001)?;
    apply(&mut record, TaskRunAction::Connect { host: first }, 1002)?;
    apply(&mut record, TaskRunAction::Connect { host: second }, 1002)?;
    apply(
        &mut record,
        TaskRunAction::StepIntent {
            host: first,
            step: 0,
        },
        1003,
    )?;
    apply(&mut record, TaskRunAction::Recover, 1004)?;
    assert_eq!(record.meta().state, TaskState::Queued);
    assert_eq!(
        record.targets().iter().map(|t| t.state).collect::<Vec<_>>(),
        vec![
            TargetState::Unknown,
            TargetState::Unknown,
            TargetState::Pending
        ]
    );
    apply(&mut record, TaskRunAction::Start, 1005)?;
    assert_eq!(
        record.prepare(TaskRunAction::Connect { host: first }, 1006),
        Err(TaskError::Transition)
    );
    let mut definition = record.snapshot().definition().clone();
    definition.policy.canary_count = 1;
    definition.policy.approve_after_canary = true;
    let mut gated = TaskRunRecord::new(TaskSnapshot::new(
        definition,
        record.snapshot().targets().clone(),
        "operator".into(),
        1000,
    )?)?;
    apply(&mut gated, TaskRunAction::Start, 1001)?;
    apply(&mut gated, TaskRunAction::Connect { host: first }, 1002)?;
    apply(
        &mut gated,
        TaskRunAction::StepIntent {
            host: first,
            step: 0,
        },
        1003,
    )?;
    apply(
        &mut gated,
        TaskRunAction::StepExited {
            host: first,
            step: 0,
            exit_code: 0,
        },
        1004,
    )?;
    apply(&mut gated, TaskRunAction::AdvanceBatch, 1005)?;
    let gate = gated.meta().gate.clone().ok_or("missing gate")?;
    assert_eq!(
        gated.prepare(TaskRunAction::Connect { host: second }, 1006),
        Err(TaskError::Transition)
    );
    assert_eq!(
        gated.prepare(TaskRunAction::ApproveGate(TaskGateId::new()), 1006),
        Err(TaskError::Transition)
    );
    apply(&mut gated, TaskRunAction::Recover, 1006)?;
    assert_eq!(gated.meta().gate, Some(gate.clone()));
    apply(&mut gated, TaskRunAction::ApproveGate(gate.id), 1007)?;
    assert_eq!(
        gated.prepare(TaskRunAction::ApproveGate(gate.id), 1008),
        Err(TaskError::Transition)
    );
    Ok(())
}

#[test]
fn failures_stop_canaries_and_cancel_pre_dispatch_without_inventing_remote_termination()
-> Result<(), Box<dyn Error>> {
    let original = fixture(3)?;
    let mut definition = original.definition().clone();
    definition.policy.canary_count = 1;
    let mut record = TaskRunRecord::new(TaskSnapshot::new(
        definition,
        original.targets().clone(),
        "operator".into(),
        1000,
    )?)?;
    let host = record.targets()[0].host_id;
    apply(&mut record, TaskRunAction::Start, 1001)?;
    apply(&mut record, TaskRunAction::Connect { host }, 1002)?;
    apply(
        &mut record,
        TaskRunAction::StepIntent { host, step: 0 },
        1003,
    )?;
    apply(
        &mut record,
        TaskRunAction::StepExited {
            host,
            step: 0,
            exit_code: 7,
        },
        1004,
    )?;
    apply(&mut record, TaskRunAction::AdvanceBatch, 1005)?;
    assert_eq!(
        record.meta().stop_reason,
        Some(TaskStopReason::CanaryFailed)
    );
    assert!(
        record.targets()[1..]
            .iter()
            .all(|t| t.state == TargetState::Cancelled)
    );
    apply(&mut record, TaskRunAction::Finish, 1006)?;
    assert_eq!(record.meta().state, TaskState::Failed);
    let mut before = TaskRunRecord::new(fixture(2)?)?;
    apply(
        &mut before,
        TaskRunAction::Stop(TaskStopReason::UserCancelled),
        1001,
    )?;
    apply(&mut before, TaskRunAction::Finish, 1002)?;
    assert_eq!(before.meta().state, TaskState::Cancelled);
    Ok(())
}

#[test]
fn cancellation_keeps_prior_failure_visible_and_restore_rejects_fabricated_execution()
-> Result<(), Box<dyn Error>> {
    let mut record = TaskRunRecord::new(fixture(3)?)?;
    let host = record.targets()[0].host_id;
    apply(&mut record, TaskRunAction::Start, 1001)?;
    apply(&mut record, TaskRunAction::Connect { host }, 1002)?;
    apply(
        &mut record,
        TaskRunAction::StepIntent { host, step: 0 },
        1003,
    )?;
    apply(
        &mut record,
        TaskRunAction::StepExited {
            host,
            step: 0,
            exit_code: 7,
        },
        1004,
    )?;
    apply(
        &mut record,
        TaskRunAction::Stop(TaskStopReason::UserCancelled),
        1005,
    )?;
    apply(&mut record, TaskRunAction::Finish, 1006)?;
    assert_eq!(record.meta().state, TaskState::PartialFailed);
    assert_eq!(record.targets()[0].steps[0].exit_code, Some(7));
    let mut meta = record.meta().clone();
    meta.state = TaskState::Cancelled;
    assert!(
        TaskRunRecord::restore(record.snapshot().clone(), meta, record.targets().to_vec()).is_err()
    );
    let mut targets = record.targets().to_vec();
    targets[0].steps[0].started_at_ms = None;
    assert!(
        TaskRunRecord::restore(record.snapshot().clone(), record.meta().clone(), targets).is_err()
    );
    let mut targets = record.targets().to_vec();
    targets[0].steps[0].started_at_ms = Some(1001);
    assert!(
        TaskRunRecord::restore(record.snapshot().clone(), record.meta().clone(), targets).is_err()
    );
    Ok(())
}

#[test]
fn concurrency_budget_is_enforced_and_restored_gate_must_match_the_frozen_policy()
-> Result<(), Box<dyn Error>> {
    let original = fixture(3)?;
    let mut definition = original.definition().clone();
    definition.policy.concurrency = 1;
    definition.policy.canary_count = 2;
    definition.policy.approve_after_canary = true;
    let mut record = TaskRunRecord::new(TaskSnapshot::new(
        definition,
        original.targets().clone(),
        "operator".into(),
        1000,
    )?)?;
    let first = record.targets()[0].host_id;
    let second = record.targets()[1].host_id;
    apply(&mut record, TaskRunAction::Start, 1001)?;
    apply(&mut record, TaskRunAction::Connect { host: first }, 1002)?;
    assert_eq!(
        record.prepare(TaskRunAction::Connect { host: second }, 1003),
        Err(TaskError::Transition)
    );
    apply(
        &mut record,
        TaskRunAction::StepIntent {
            host: first,
            step: 0,
        },
        1003,
    )?;
    apply(
        &mut record,
        TaskRunAction::StepExited {
            host: first,
            step: 0,
            exit_code: 0,
        },
        1004,
    )?;
    apply(&mut record, TaskRunAction::Connect { host: second }, 1005)?;
    apply(
        &mut record,
        TaskRunAction::StepIntent {
            host: second,
            step: 0,
        },
        1006,
    )?;
    apply(
        &mut record,
        TaskRunAction::StepExited {
            host: second,
            step: 0,
            exit_code: 0,
        },
        1007,
    )?;
    apply(&mut record, TaskRunAction::AdvanceBatch, 1008)?;
    let mut meta = record.meta().clone();
    meta.gate.as_mut().ok_or("missing gate")?.kind = TaskGateKind::BetweenBatches;
    assert!(
        TaskRunRecord::restore(record.snapshot().clone(), meta, record.targets().to_vec()).is_err()
    );
    let mut targets = record.targets().to_vec();
    targets[0].state = TargetState::Pending;
    targets[0].started_at_ms = None;
    targets[0].finished_at_ms = None;
    targets[0].steps = vec![TaskStepRun {
        state: TargetState::Pending,
        started_at_ms: None,
        finished_at_ms: None,
        exit_code: None,
        reason: None,
    }];
    assert!(
        TaskRunRecord::restore(record.snapshot().clone(), record.meta().clone(), targets).is_err()
    );
    Ok(())
}

#[test]
fn policy_interpolation_corrupt_states_and_stale_commit_acknowledgements_are_rejected()
-> Result<(), Box<dyn Error>> {
    let original = fixture(1)?;
    let mut definition = original.definition().clone();
    definition.steps[0].command = TaskCommand::Invocation {
        program: "printf".into(),
        arguments: vec![TaskArgument::Variable {
            name: "VALUE".into(),
        }],
    };
    assert_eq!(
        compile_task_commands(&definition),
        Err(TaskError::MissingVariable)
    );
    definition
        .variables
        .insert("VALUE".into(), "x'; echo PRIVATE_INJECTION".into());
    let compiled = compile_task_commands(&definition)?;
    assert!(compiled[0].contains("exec /bin/sh -c"));
    definition.shell = RemoteShellProfile::Cmd;
    assert_eq!(
        compile_task_commands(&definition),
        Err(TaskError::UnsafeCmdValue)
    );
    definition.policy.concurrency = 0;
    assert_eq!(
        validate_task_definition(&definition),
        Err(TaskError::Invalid)
    );
    let mut record = TaskRunRecord::new(original)?;
    let patch = record.prepare(TaskRunAction::Start, 1001)?;
    assert_eq!(
        record.apply_committed(&patch, 100),
        Err(TaskError::Transition)
    );
    assert_eq!(record.meta().revision, 1);
    apply(&mut record, TaskRunAction::Start, 1001)?;
    assert_eq!(
        record.apply_committed(&patch, patch.after().revision),
        Err(TaskError::Transition)
    );
    assert_eq!(
        record.prepare(TaskRunAction::Recover, 999),
        Err(TaskError::Clock)
    );
    Ok(())
}
