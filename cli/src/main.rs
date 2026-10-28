use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TaskOperation, TaskRequest, TaskStatus,
    client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{io::Write, process::ExitCode, time::Duration};
use tokio::io::AsyncReadExt;
use zeroize::Zeroizing;
mod foreign_import;
mod host_ca;
mod mixed_results;
mod mixed_retry;
mod mixed_rollout;
mod output_analysis;
mod output_archive;
mod output_merge;
mod output_search;
mod output_workspace;
mod profile_batch;
mod scripts;
mod sftp;
mod ssh_providers;
mod task_output_transport;
mod terminal_policies;
mod terminal_tools;
mod tunnels;
mod workspace;

const USAGE: &str = "Usage:
  cshell-cli script inspect <script.json>
  cshell-cli script permissions <script.json> <capabilities.json>
  cshell-cli script run-offline <script.json> <capabilities.json> <parameters.json> --confirm
  cshell-cli script live-preview <script.json> <capabilities.json> <scope.json> <new-review.json>
  cshell-cli script live-start <review.json> <parameters.json> --confirm
  cshell-cli script live-get <run-id>
  cshell-cli script live-list
  cshell-cli script live-revoke <run-id>
  cshell-cli script recover --confirm
  cshell-cli script recording-new <new-recording.json>
  cshell-cli script recording-inspect <recording.json>
  cshell-cli script recording-render <recording.json> <new-script.json>
  cshell-cli foreign-import inspect-ppk <key.ppk> [--passphrase-stdin]
  cshell-cli foreign-import convert-putty <input.reg> <new-profile.json> [default-account]
  cshell-cli foreign-import convert-securecrt <session.ini> <new-profile.json> [default-account]
  cshell-cli foreign-import convert-xshell <session.xsh> <new-profile.json> [default-account]
  cshell-cli foreign-import convert-mobaxterm <sessions.mxtsessions> <new-profile.json> [default-account] [--windows-1252]
  cshell-cli foreign-import preview-mobaxterm <sessions.mxtsessions> <new-review.json> <stop|skip|replace> [default-account] [--windows-1252]
  cshell-cli foreign-import preview-xshell <session.xsh> <new-review.json> <stop|skip|replace> [default-account]
  cshell-cli foreign-import preview-securecrt <session.ini> <new-review.json> <stop|skip|replace> [default-account]
  cshell-cli foreign-import preview-putty <input.reg> <new-review.json> <stop|skip|replace> [default-account]
  cshell-cli foreign-import apply <review.json> --confirm
  cshell-cli foreign-import convert-securecrt-dir <sessions-directory> <new-profile.json> [default-account]
  cshell-cli foreign-import preview-securecrt-dir <sessions-directory> <new-review.json> <stop|skip|replace> [default-account]
  cshell-cli foreign-import convert-xshell-dir <sessions-directory> <new-profile.json> [default-account]
  cshell-cli foreign-import preview-xshell-dir <sessions-directory> <new-review.json> <stop|skip|replace> [default-account]
  cshell-cli foreign-import convert-manifest <manifest.json> <new-profile.json> [default-account]
  cshell-cli foreign-import preview-manifest <manifest.json> <new-review.json> <stop|skip|replace> [default-account]
  cshell-cli profile-batch reload
  cshell-cli terminal-policies get
  cshell-cli workspace get
  cshell-cli workspace export <new-layout.json>
  cshell-cli workspace preview <layout.json> <new-review.json>
  cshell-cli workspace restore <review.json> --confirm
  cshell-cli terminal-policies launch <resolved.json> <rows> <cols> --confirm
  cshell-cli terminal-policies preview <proposal.json> <new-preview.json>
  cshell-cli terminal-policies apply <preview.json> --confirm
  cshell-cli terminal-policies resolve <context.json> <policy-rev> <profile-rev> <host-rev>
  cshell-cli profile-batch inspect <profile-id> [revision]
  cshell-cli profile-batch preview <batch.json> <new-preview.json>
  cshell-cli profile-batch apply <preview.json> --confirm
  cshell-cli tools get
  cshell-cli tools save <revision> <catalog.json> --confirm
  cshell-cli tools arm <session> <revision> [--allow-text] --confirm
  cshell-cli tools disarm <session>
  cshell-cli tools events <after-id>
  cshell-cli tools highlights-import <revision> <sets.json> --confirm
  cshell-cli tools highlights-export <new-file>
  cshell-cli tools hex|journal <session> <sequence> [offset]
  cshell-cli tools export-ansi <session> <first-sequence> <count> <new-file>
  cshell-cli tools convert-xcs <input.xcs> <new-theme.json>
  cshell-cli tools themes-import <revision> <themes.json|themes.toml|colors.xcs> --confirm
  cshell-cli tools themes-export <new-file>
  cshell-cli providers get
  cshell-cli providers check <requirements.json>
  cshell-cli ca get
  cshell-cli ca preview <new|authority-id> <catalog-revision> <draft.json>
  cshell-cli ca preview-remove <authority-id> <catalog-revision>
  cshell-cli ca apply <preview.json> <exact-SHA256-fingerprint> --confirm
  cshell-cli ca inspect-certificate <public-certificate-file> [host port]
  cshell-cli tunnel get
  cshell-cli tunnel create <catalog-revision> <definition.json> --confirm
  cshell-cli tunnel update <tunnel-id> <catalog-revision> <definition.json> --confirm
  cshell-cli tunnel start <tunnel-id> <catalog-revision> <live-session-id> --confirm
  cshell-cli tunnel stop <tunnel-id> <catalog-revision> --confirm
  cshell-cli tunnel remove <tunnel-id> <catalog-revision> --confirm
  cshell-cli tunnel configure-dedicated <tunnel-id> <catalog-revision> <reference-session-id> <configuration.json> --confirm
  cshell-cli tunnel start-dedicated <tunnel-id> <catalog-revision> --confirm
  cshell-cli tunnel clear-dedicated <tunnel-id> <catalog-revision> --confirm
  cshell-cli version
  cshell-cli status
  cshell-cli sftp queue-get <session-id>
  cshell-cli sftp queue-add <session-id> <revision> <upload|download> <absolute-local-file> <absolute-remote-file> --confirm
  cshell-cli sftp queue-add-verified <session-id> <revision> <upload|download> <absolute-local-file> <absolute-remote-file> --confirm
  cshell-cli sftp queue-start-verified <session-id> <revision> --confirm
  cshell-cli sftp queue-resume <session-id> <revision> <item-id> --confirm
  cshell-cli sftp queue-adopt <session-id> <revision> <item-id> --confirm
  cshell-cli sftp queue-cleanup <session-id> <revision> <item-id> --confirm
  cshell-cli sftp queue-start <session-id> <revision> --confirm
  cshell-cli sftp queue-pause <session-id> <revision> --confirm
  cshell-cli sftp queue-cancel <session-id> <revision> <item-id|all> --confirm
  cshell-cli sftp queue-remove <session-id> <revision> <item-id> --confirm
  cshell-cli sftp local-list <session-id> [absolute-directory]
  cshell-cli sftp remote-list <session-id> <directory>
  cshell-cli sftp local-browse <session-id> [absolute-directory]
  cshell-cli sftp remote-browse <session-id> <absolute-directory>
  cshell-cli sftp browse-next <session-id> <cursor-id> <revision> <local|remote> <directory>
  cshell-cli sftp browse-close <session-id> <cursor-id> <revision> <local|remote> <directory>
  cshell-cli sftp transfer-get <session-id> <transfer-id>
  cshell-cli sftp transfer-cancel <session-id> <transfer-id> --confirm
  cshell-cli sftp peer-get <session-id>
  cshell-cli sftp browse <session-id> <local|remote> <directory> <nofollow|follow>
  cshell-cli sftp queue-add-managed <session-id> <revision> <draft.json> --confirm
  cshell-cli sftp queue-add-many <session-id> <revision> <drafts.json> --confirm
  cshell-cli sftp queue-edit <session-id> <revision> <item-id> <draft.json> --confirm
  cshell-cli sftp queue-configure <session-id> <revision> <1..4> --confirm
  cshell-cli sftp queue-start-managed <session-id> <revision> --confirm
  cshell-cli sftp queue-start-one <session-id> <revision> <item-id> --confirm
  cshell-cli sftp queue-migrate <old-session> <old-revision> <new-session> <new-revision> <peer.json> --confirm
  cshell-cli sftp file-inspect <session-id> <local|remote> <inspect|delete> <path>
  cshell-cli sftp file-inspect <session-id> <local|remote> <mkdir|chmod> <path> <octal-mode>
  cshell-cli sftp file-inspect <session-id> <local|remote> <rename|symlink> <path> <target>
  cshell-cli sftp file-get <session-id> <job-id>
  cshell-cli sftp file-apply <session-id> <job-id> <revision> --confirm
  cshell-cli sftp file-cancel <session-id> <job-id> <revision> --confirm
  cshell-cli sftp edit-open <session-id> <remote-file> [nofollow|follow]
  cshell-cli sftp edit-get <session-id> <edit-id>
  cshell-cli sftp edit-launch <session-id> <edit-id> <revision> <absolute-editor> <args.json> --confirm
  cshell-cli sftp edit-save <session-id> <edit-id> <edit-revision> <queue-revision> <text-file|--working> --exclusive --confirm
  cshell-cli sftp edit-close <session-id> <edit-id> <revision> --confirm
  cshell-cli sftp edit-rebind <new-session-id> <edit-id> <revision> --confirm
  cshell-cli sftp workspace-list <session-id> <offset> <current|all>
  cshell-cli sftp workspace-forget <session-id> <record-id> <revision> <file|edit> [--discard-private] --confirm
  cshell-cli task output-workspace-request <scope-array.json> <index|search|diff|bundle|retention-preview> [query]
  cshell-cli task output-workspace-scopes <task|mixed> <status.json> <zero-based-step|all> <stdout|stderr|both>
  cshell-cli task output-workspace <request.json>
  cshell-cli task output-workspace-get <job.json>
  cshell-cli task output-workspace-cancel <job.json>
  cshell-cli task output-workspace-page <job.json> <row-offset>
  cshell-cli task output-workspace-export <job.json> <absolute-new-file>
  cshell-cli task output-index-read <scope.json> <index-entry.json> <line-offset>
  cshell-cli task output-retention-apply <plan.json> --confirm
  cshell-cli task output-retention-receipt <plan.json>
  cshell-cli task output-container-inspect <container-file>
  cshell-cli task output-container-extract <container-file> <absolute-new-directory> --confirm
  cshell-cli task template-list
  cshell-cli task template <template-slug>
  cshell-cli task create <definition.json> <selection.json> <host-revision> <profile-revision>
  cshell-cli task get <run-id>
  cshell-cli task review <run-id>
  cshell-cli task results <run-id>
  cshell-cli task export <run-id> <revision> <json|csv> <absolute-new-file>
  cshell-cli task compare <run-id> <revision> <left-host> <right-host> <step> <stdout|stderr> <offset>
  cshell-cli task upload-manifest <absolute-source> <absolute-remote-destination> <name>
  cshell-cli task upload-create <definition.json> <selection.json> <host-revision> <profile-revision>
  cshell-cli task upload-get <run-id>
  cshell-cli task upload-list [after-run-id]
  cshell-cli task upload-start <run-id> --confirm
  cshell-cli task upload-cancel <run-id>
  cshell-cli task upload-probe <run-id> <host-id>
  cshell-cli task upload-resume <run-id> <host-id> <target-revision> --confirm
  cshell-cli task upload-cleanup <run-id> <host-id> <target-revision> --confirm
  cshell-cli task upload-adopt <run-id> <host-id> <target-revision> --confirm
  cshell-cli task download-definition <absolute-remote-source> <absolute-local-directory> <local-name> <name>
  cshell-cli task download-create <definition.json> <selection.json> <host-revision> <profile-revision>
  cshell-cli task download-get <run-id>
  cshell-cli task download-list [after-run-id]
  cshell-cli task download-start <run-id> --confirm
  cshell-cli task download-cancel <run-id>
  cshell-cli task download-probe <run-id> <host-id>
  cshell-cli task download-resume <run-id> <host-id> <target-revision> --confirm
  cshell-cli task download-cleanup <run-id> <host-id> <target-revision> --confirm
  cshell-cli task download-adopt <run-id> <host-id> <target-revision> --confirm
  cshell-cli task mixed-create <definition.json> <selection.json> <host-revision> <profile-revision>
  cshell-cli task mixed-review <run-id>
  cshell-cli task mixed-get <run-id> [after-host-id]
  cshell-cli task mixed-list [after-run-id]
  cshell-cli task mixed-confirmation <run-id>
  cshell-cli task mixed-start <confirmation.json> --confirm
  cshell-cli task mixed-cancel <run-id>
  cshell-cli task mixed-retry-confirmation <review.json> <status-page.json> <host-id[,host-id...]> [--confirm-unknown] [--confirm-files]
  cshell-cli task mixed-retry <confirmation.json> --confirm
  cshell-cli task mixed-rollout-get <run-id>
  cshell-cli task mixed-rollout-confirmation <run-id>
  cshell-cli task mixed-rollout-approve <control.json> --confirm
  cshell-cli task mixed-output <run-id> <host-id> <step> <stdout|stderr> <offset> [--raw]
  cshell-cli task mixed-file-probe <run-id> <host-id> <step> <parent-revision>
  cshell-cli task mixed-file-resume <run-id> <host-id> <step> <parent-revision> --confirm
  cshell-cli task mixed-file-cleanup <run-id> <host-id> <step> <parent-revision> --confirm
  cshell-cli task mixed-file-adopt <run-id> <host-id> <step> <parent-revision> --confirm
  cshell-cli task mixed-file-status <run-id>
  cshell-cli task mixed-results <run-id>
  cshell-cli task mixed-result-page <manifest.json> [after-host-id]
  cshell-cli task mixed-export <run-id> <json|csv> <absolute-new-file>
  cshell-cli task output-analysis-request <task|mixed> <run-id> <left-host> <left-revision> <step> <stdout|stderr> [right-host right-revision]
  cshell-cli task output-analyze <request.json>
  cshell-cli task output-analysis-get <job.json>
  cshell-cli task output-analysis-cancel <job.json>
  cshell-cli task output-search-request <single-target-analysis-request.json> <utf8|hex> <pattern>
  cshell-cli task output-search <request.json>
  cshell-cli task output-search-get <job.json>
  cshell-cli task output-search-cancel <job.json>
  cshell-cli task output-search-next-request <request.json> <completed-job.json>
  cshell-cli task output-merge-request <single-target-stdout-analysis-request.json>
  cshell-cli task output-merge <request.json>
  cshell-cli task output-merge-next-request <page.json>
  cshell-cli task output-archive <single-target-stdout-analysis-request.json>
  cshell-cli task output-archive-get <job.json>
  cshell-cli task output-archive-cancel <job.json>
  cshell-cli task output-archive-export <completed-job.json> <absolute-new-file>
  cshell-cli task output-archive-inspect <archive.cshout>
  cshell-cli task list [after-run-id]
  cshell-cli task start <run-id> <revision> --confirm
  cshell-cli task cancel <run-id>
  cshell-cli task approve <run-id> <revision> <gate-id> --confirm
  cshell-cli task retry <run-id> <revision> <host-id[,host-id...]> --confirm [--confirm-unknown]
  cshell-cli task output <run-id> <host-id> <step> <stdout|stderr> <offset> [--raw]

Create saves a frozen plan and prints it for review. Start executes that saved plan.
Mutations are sent once. After an unknown reply, inspect the saved run before acting.
Downloads write on the daemon computer into an existing directory, as <host-id>-<local-name>.
Probe/Cleanup/Adopt launch background jobs; query upload-get/download-get for results.
Mixed child definitions use concurrency 1; the parent controls host concurrency.
Mixed retry creates a new Pending run from step one; it does not resume old receipts or start execution.
Previously dispatched commands require original idempotent declarations; repair uncertain files in the original run first.
File replay can encounter existing destinations and rereads download sources; original temporary files remain with their original run.
Review the frozen plan, then save mixed-confirmation output and pass it to mixed-start.
Mixed status pages contain at most 64 targets; their pending revisions cover the whole run.
Rollout policy is optional: batch_size, canary_count, approve_after_canary, pause_between_batches, batch_delay_ms, failure_limit.
Use mixed-rollout-confirmation for advanced plans; approve consumes a saved gate only and never starts the next batch.
Mixed file repair steps are zero-based and bind the parent's target revision.
Query mixed-file-status after a repair launch; continue the parent separately after reviewing its new pending set.
Mixed results capture all stopped targets, including Unknown, without command/path/credential/output bodies.
Save mixed-results JSON as manifest.json for readonly pages. Captures expire after ten minutes or eviction.
Mixed export captures and verifies all pages before publishing a new file on the CLI computer.
Output analysis steps are zero-based. Task scopes use the whole run revision; mixed scopes use each parent's target revision.
Save output-analysis-request JSON for launch, and output-analyze JSON as job.json for query/cancel.
Full analysis hashes/compares stored local bytes only; it cannot prove remote success or complete output delivery.
Search matches literal overlapping bytes; save request JSON explicitly (it contains the query bytes).
Search replies contain query length/hash, not query bytes or output bodies. Save job JSON for get/cancel.
Next-request prints a digest-bound continuation for explicit saving and launch; every page hashes the whole stored stream.
Merge reads stdout/stderr in per-command daemon recording order. Old output without events.bin has no merge chronology.
Merge page JSON contains metadata and bytes; save it explicitly for next-request. Seal/coverage/truncation do not prove remote success.
Archive capture preserves raw stdout/stderr and available journal bytes in CSHOUT01. Job JSON contains metadata only.
Archive export downloads the specified completed capture, checks all bytes/container/EOF, then publishes a new client file.
Archives can contain sensitive output. Inspect is offline, validates without extracting or executing, and does not prove remote success.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
fn id(value: &str) -> Result<Vec<u8>, &'static str> {
    let id = uuid::Uuid::parse_str(value).map_err(|_| "invalid UUID")?;
    if id.is_nil() {
        return Err("nil UUID is not allowed");
    }
    Ok(id.as_bytes().to_vec())
}
fn number(value: &str) -> Result<u64, &'static str> {
    value.parse().map_err(|_| "invalid unsigned number")
}
async fn document(path: &str) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "cannot open task document")?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "cannot read task document")?;
    if bytes.len() > 1024 * 1024 {
        return Err("task document exceeds 1 MiB");
    }
    Ok(bytes)
}
async fn request(args: &[String]) -> Result<(TaskRequest, bool), &'static str> {
    let mut request = TaskRequest::default();
    let mut raw = false;
    match args.first().map(String::as_str) {
        Some("mixed-file-status") if args.len() == 2 => {
            request.operation = TaskOperation::MixedFileStatus as i32;
            request.run_id = id(&args[1])?;
        }
        Some("mixed-file-probe") if args.len() == 5 => {
            request.operation = TaskOperation::MixedFileProbe as i32;
            request.run_id = id(&args[1])?;
            request.host_id = id(&args[2])?;
            request.step = u32::try_from(number(&args[3])?).map_err(|_| "step is out of range")?;
            request.expected_revision = number(&args[4])?;
            if request.expected_revision == 0 {
                return Err("file inspection requires a parent target revision");
            }
        }
        Some("mixed-file-resume" | "mixed-file-cleanup" | "mixed-file-adopt")
            if args.len() == 6 && args[5] == "--confirm" =>
        {
            request.operation = match args[0].as_str() {
                "mixed-file-resume" => TaskOperation::MixedFileResume,
                "mixed-file-cleanup" => TaskOperation::MixedFileCleanup,
                _ => TaskOperation::MixedFileAdopt,
            } as i32;
            request.run_id = id(&args[1])?;
            request.host_id = id(&args[2])?;
            request.step = u32::try_from(number(&args[3])?).map_err(|_| "step is out of range")?;
            request.expected_revision = number(&args[4])?;
            request.confirmed = true;
            if request.expected_revision == 0 {
                return Err("file repair requires a parent target revision");
            }
        }
        Some("mixed-create") if args.len() == 5 => {
            request.operation = TaskOperation::MixedCreate as i32;
            request.definition_json = document(&args[1]).await?.to_vec();
            request.selection_json = document(&args[2]).await?.to_vec();
            request.host_revision = number(&args[3])?;
            request.profile_revision = number(&args[4])?;
        }
        Some("mixed-review" | "mixed-cancel") if args.len() == 2 => {
            request.operation = if args[0] == "mixed-review" {
                TaskOperation::MixedReview
            } else {
                TaskOperation::MixedCancel
            } as i32;
            request.run_id = id(&args[1])?;
        }
        Some("mixed-get") if args.len() == 2 || args.len() == 3 => {
            request.operation = TaskOperation::MixedGet as i32;
            request.run_id = id(&args[1])?;
            request.limit = 64;
            if args.len() == 3 {
                request.host_id = id(&args[2])?;
            }
        }
        Some("mixed-confirmation") if args.len() == 2 => {
            request.operation = TaskOperation::MixedGet as i32;
            request.run_id = id(&args[1])?;
            request.limit = 64;
        }
        Some("mixed-list") if args.len() <= 2 => {
            request.operation = TaskOperation::MixedList as i32;
            request.limit = 64;
            if args.len() == 2 {
                request.run_id = id(&args[1])?;
            }
        }
        Some("mixed-start") if args.len() == 3 && args[2] == "--confirm" => {
            let bytes = document(&args[1]).await?;
            let confirmation = cshell_application::decode_mixed_confirmation(&bytes)
                .map_err(|_| "invalid mixed start confirmation")?;
            request.operation = TaskOperation::MixedStart as i32;
            request.run_id = confirmation.run_id.as_uuid().as_bytes().to_vec();
            request.expected_targets_json = bytes.to_vec();
            request.confirmed = true;
        }
        Some("mixed-output") if args.len() == 6 || args.len() == 7 && args[6] == "--raw" => {
            request.operation = TaskOperation::MixedOutput as i32;
            request.run_id = id(&args[1])?;
            request.host_id = id(&args[2])?;
            request.step = u32::try_from(number(&args[3])?).map_err(|_| "step is out of range")?;
            request.stderr = match args[4].as_str() {
                "stdout" => false,
                "stderr" => true,
                _ => return Err("select stdout or stderr"),
            };
            request.offset = number(&args[5])?;
            request.limit = 64 * 1024;
            raw = args.len() == 7;
        }
        Some("upload-create" | "download-create") if args.len() == 5 => {
            request.operation = if args[0] == "download-create" {
                TaskOperation::DownloadCreate
            } else {
                TaskOperation::UploadCreate
            } as i32;
            request.definition_json = document(&args[1]).await?.to_vec();
            request.selection_json = document(&args[2]).await?.to_vec();
            request.host_revision = number(&args[3])?;
            request.profile_revision = number(&args[4])?;
        }
        Some("upload-get" | "upload-cancel" | "download-get" | "download-cancel")
            if args.len() == 2 =>
        {
            request.operation = match args[0].as_str() {
                "upload-get" => TaskOperation::UploadGet,
                "upload-cancel" => TaskOperation::UploadCancel,
                "download-get" => TaskOperation::DownloadGet,
                _ => TaskOperation::DownloadCancel,
            } as i32;
            request.run_id = id(&args[1])?;
        }
        Some("upload-start" | "download-start") if args.len() == 3 && args[2] == "--confirm" => {
            request.operation = if args[0] == "download-start" {
                TaskOperation::DownloadStart
            } else {
                TaskOperation::UploadStart
            } as i32;
            request.run_id = id(&args[1])?;
            request.confirmed = true;
        }
        Some("upload-probe" | "download-probe") if args.len() == 3 => {
            request.operation = if args[0] == "download-probe" {
                TaskOperation::DownloadProbe
            } else {
                TaskOperation::UploadProbe
            } as i32;
            request.run_id = id(&args[1])?;
            request.host_id = id(&args[2])?;
        }
        Some(
            "upload-resume" | "upload-cleanup" | "upload-adopt" | "download-resume"
            | "download-cleanup" | "download-adopt",
        ) if args.len() == 5 && args[4] == "--confirm" => {
            request.operation = match args[0].as_str() {
                "upload-resume" => TaskOperation::UploadResume,
                "upload-cleanup" => TaskOperation::UploadCleanup,
                "upload-adopt" => TaskOperation::UploadAdopt,
                "download-resume" => TaskOperation::DownloadResume,
                "download-cleanup" => TaskOperation::DownloadCleanup,
                _ => TaskOperation::DownloadAdopt,
            } as i32;
            request.run_id = id(&args[1])?;
            request.host_id = id(&args[2])?;
            request.expected_revision = number(&args[3])?;
            request.confirmed = true;
            if request.expected_revision == 0 {
                return Err("file recovery requires a nonzero target revision");
            }
        }
        Some("create") if args.len() == 5 => {
            request.operation = TaskOperation::Create as i32;
            request.definition_json = document(&args[1]).await?.to_vec();
            request.selection_json = document(&args[2]).await?.to_vec();
            request.host_revision = number(&args[3])?;
            request.profile_revision = number(&args[4])?;
        }
        Some("get" | "review" | "results" | "cancel") if args.len() == 2 => {
            request.operation = match args[0].as_str() {
                "get" => TaskOperation::Get,
                "review" => TaskOperation::Review,
                "results" => TaskOperation::Results,
                _ => TaskOperation::Cancel,
            } as i32;
            request.run_id = id(&args[1])?;
        }
        Some("export") if args.len() == 5 && matches!(args[3].as_str(), "json" | "csv") => {
            request.operation = TaskOperation::Results as i32;
            request.run_id = id(&args[1])?;
            request.expected_revision = number(&args[2])?;
            if request.expected_revision == 0 || !std::path::Path::new(&args[4]).is_absolute() {
                return Err("export requires an exact revision and an absolute new file path");
            }
        }
        Some("compare") if args.len() == 8 => {
            request.operation = TaskOperation::Compare as i32;
            request.run_id = id(&args[1])?;
            request.expected_revision = number(&args[2])?;
            request.host_id = id(&args[3])?;
            request.compare_host_id = id(&args[4])?;
            request.step = u32::try_from(number(&args[5])?).map_err(|_| "step is out of range")?;
            request.stderr = match args[6].as_str() {
                "stdout" => false,
                "stderr" => true,
                _ => return Err("select stdout or stderr"),
            };
            request.offset = number(&args[7])?;
            request.limit = 64 * 1024;
            if request.expected_revision == 0 || request.host_id == request.compare_host_id {
                return Err("comparison requires an exact revision and two distinct hosts");
            }
        }
        Some("list" | "upload-list" | "download-list") if args.len() <= 2 => {
            request.operation = match args[0].as_str() {
                "upload-list" => TaskOperation::UploadList,
                "download-list" => TaskOperation::DownloadList,
                _ => TaskOperation::List,
            } as i32;
            request.limit = 64;
            if args.len() == 2 {
                request.run_id = id(&args[1])?;
            }
        }
        Some("start") if args.len() == 4 && args[3] == "--confirm" => {
            request.operation = TaskOperation::Start as i32;
            request.run_id = id(&args[1])?;
            request.expected_revision = number(&args[2])?;
            request.confirmed = true;
        }
        Some("approve") if args.len() == 5 && args[4] == "--confirm" => {
            request.operation = TaskOperation::Approve as i32;
            request.run_id = id(&args[1])?;
            request.expected_revision = number(&args[2])?;
            request.gate_id = id(&args[3])?;
            request.confirmed = true;
        }
        Some("retry")
            if (args.len() == 5 || args.len() == 6 && args[5] == "--confirm-unknown")
                && args[4] == "--confirm" =>
        {
            request.operation = TaskOperation::Retry as i32;
            request.run_id = id(&args[1])?;
            request.expected_revision = number(&args[2])?;
            request.retry_hosts = args[3].split(',').map(id).collect::<Result<_, _>>()?;
            if request.retry_hosts.len() > 1000 {
                return Err("too many retry targets");
            }
            request.confirmed = true;
            request.confirm_unknown = args.len() == 6;
        }
        Some("output") if args.len() == 6 || args.len() == 7 && args[6] == "--raw" => {
            request.operation = TaskOperation::Output as i32;
            request.run_id = id(&args[1])?;
            request.host_id = id(&args[2])?;
            request.step = u32::try_from(number(&args[3])?).map_err(|_| "step is out of range")?;
            request.stderr = match args[4].as_str() {
                "stdout" => false,
                "stderr" => true,
                _ => return Err("select stdout or stderr"),
            };
            request.offset = number(&args[5])?;
            request.limit = 64 * 1024;
            raw = args.len() == 7;
        }
        _ => return Err(USAGE),
    }
    Ok((request, raw))
}
async fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "script") {
        return scripts::run(&args[1..]).await;
    }
    if args.len() == 1 && matches!(args[0].as_str(), "version" | "--version" | "-V") {
        println!("cshell-cli {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        println!("{USAGE}");
        return Ok(());
    }
    if args[0] == "task"
        && args
            .get(1)
            .is_some_and(|command| command == "template-list" || command == "template")
    {
        match args[1].as_str() {
            "template-list" if args.len() == 2 => {
                let templates: Vec<_> = cshell_application::BuiltinTaskTemplate::ALL.into_iter()
                    .map(|template| serde_json::json!({"slug": template.slug(), "name": template.label()})).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&templates)
                        .map_err(|_| "cannot encode task templates")?
                );
            }
            "template" if args.len() == 3 => {
                let template = cshell_application::BuiltinTaskTemplate::parse(&args[2])
                    .ok_or("unknown task template; use task template-list")?;
                let definition = template.definition();
                cshell_application::validate_task_definition(&definition)
                    .map_err(|_| "invalid built-in task template")?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&definition)
                        .map_err(|_| "cannot encode task template")?
                );
            }
            _ => return Err(USAGE),
        }
        return Ok(());
    }
    if args.len() == 5 && args[0] == "task" && args[1] == "upload-manifest" {
        return upload_manifest(&args[2], &args[3], &args[4]).await;
    }
    if args.len() == 6 && args[0] == "task" && args[1] == "download-definition" {
        return download_definition(&args[2], &args[3], &args[4], &args[5]);
    }
    let is_status = args == ["status"];
    if args[0] == "sftp" {
        return sftp::run(&args[1..]).await;
    }
    if args[0] == "tunnel" {
        return tunnels::run(&args[1..]).await;
    }
    if args[0] == "ca" {
        return host_ca::run(&args[1..]).await;
    }
    if args[0] == "providers" {
        return ssh_providers::run(&args[1..]).await;
    }
    if args[0] == "tools" {
        return terminal_tools::run(&args[1..]).await;
    }
    if args[0] == "foreign-import" {
        return foreign_import::run(&args[1..]).await;
    }
    if args[0] == "workspace" {
        return workspace::run(&args[1..]).await;
    }
    if args[0] == "terminal-policies" {
        return terminal_policies::run(&args[1..]).await;
    }
    if args[0] == "profile-batch" {
        return profile_batch::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            command.starts_with("output-workspace")
                || command.starts_with("output-retention")
                || command.starts_with("output-container")
                || command == "output-index-read"
        })
    {
        return output_workspace::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(
                command.as_str(),
                "mixed-rollout-get" | "mixed-rollout-confirmation" | "mixed-rollout-approve"
            )
        })
    {
        return mixed_rollout::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(command.as_str(), "mixed-retry-confirmation" | "mixed-retry")
        })
    {
        return mixed_retry::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(
                command.as_str(),
                "output-archive"
                    | "output-archive-get"
                    | "output-archive-cancel"
                    | "output-archive-export"
                    | "output-archive-inspect"
            )
        })
    {
        return output_archive::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(
                command.as_str(),
                "output-merge-request" | "output-merge" | "output-merge-next-request"
            )
        })
    {
        return output_merge::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(
                command.as_str(),
                "output-search-request"
                    | "output-search"
                    | "output-search-get"
                    | "output-search-cancel"
                    | "output-search-next-request"
            )
        })
    {
        return output_search::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(
                command.as_str(),
                "output-analysis-request"
                    | "output-analyze"
                    | "output-analysis-get"
                    | "output-analysis-cancel"
            )
        })
    {
        return output_analysis::run(&args[1..]).await;
    }
    if args[0] == "task"
        && args.get(1).is_some_and(|command| {
            matches!(
                command.as_str(),
                "mixed-results" | "mixed-result-page" | "mixed-export"
            )
        })
    {
        return mixed_results::run(&args[1..]).await;
    }
    let (task, raw) = if is_status {
        (None, false)
    } else if args[0] == "task" {
        let (task, raw) = request(&args[1..]).await?;
        (Some(task), raw)
    } else {
        return Err(USAGE);
    };
    let expected_run = task.as_ref().map(|t| t.run_id.clone()).unwrap_or_default();
    let expected_mixed = task
        .as_ref()
        .filter(|task| {
            TaskOperation::try_from(task.operation).is_ok_and(|operation| operation.is_mixed())
        })
        .cloned();
    let expected_revision = task.as_ref().map_or(0, |task| task.expected_revision);
    let expected_comparison = task
        .as_ref()
        .filter(|task| task.operation == TaskOperation::Compare as i32)
        .cloned();
    let export = if args.len() == 6 && args[0] == "task" && args[1] == "export" {
        Some((
            std::path::PathBuf::from(&args[5]),
            if args[4] == "csv" {
                cshell_application::TaskResultExportFormat::Csv
            } else {
                cshell_application::TaskResultExportFormat::Json
            },
        ))
    } else {
        None
    };
    let operation = task.as_ref().map(|t| t.operation);
    let advanced_publication = task.as_ref().is_some_and(|query| {
        cshell_application::definition_uses_file_publication(&query.definition_json)
    });
    let advanced_mixed = task.as_ref().is_some_and(|query| {
        let document = if query.operation == TaskOperation::MixedCreate as i32 {
            &query.definition_json
        } else {
            &query.expected_targets_json
        };
        serde_json::from_slice::<serde_json::Value>(document)
            .ok()
            .is_some_and(|value| {
                if query.operation == TaskOperation::MixedCreate as i32 {
                    value
                        .get("policy")
                        .and_then(|policy| policy.get("rollout"))
                        .is_some_and(|value| !value.is_null())
                } else {
                    query.operation == TaskOperation::MixedStart as i32
                        && value
                            .get("control_revision")
                            .is_some_and(|value| !value.is_null())
                }
            })
    });
    let output_offset = task.as_ref().map_or(0, |t| t.offset);
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "no authenticated daemon discovery record")?;
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        #[cfg(windows)]
        let mut stream = cshell_ipc::transport::connect(&discovery.endpoint.to_string_lossy())
            .await
            .map_err(|_| "cannot connect to daemon")?;
        #[cfg(unix)]
        let mut stream = cshell_ipc::transport::connect(std::path::Path::new(&discovery.endpoint))
            .await
            .map_err(|_| "cannot connect to daemon")?;
        let mut handshake = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        handshake.feature_bits = if is_status {
            features::TERMINAL_CONTROL
        } else {
            features::TASK_CONTROL | features::TASK_PLAN_CONTROL | features::TASK_RESULTS | features::TASK_UPLOADS | features::TASK_DOWNLOADS | features::MIXED_TASK_CONTROL | features::MIXED_FILE_REPAIR|features::MIXED_ROLLOUT|features::FILE_PUBLICATION
        };
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if advanced_publication && negotiated.feature_bits & features::FILE_PUBLICATION == 0 { return Err("daemon does not support file publication policy"); }
        if !is_status && negotiated.feature_bits & features::TASK_CONTROL == 0 {
            return Err("daemon does not support task control");
        }
        if matches!(operation, Some(value) if value == TaskOperation::Review as i32 || value == TaskOperation::Retry as i32)
            && negotiated.feature_bits & features::TASK_PLAN_CONTROL == 0
        {
            return Err("daemon does not support task plan review/retry");
        }
        if matches!(operation, Some(value) if value == TaskOperation::Results as i32 || value == TaskOperation::Compare as i32)
            && negotiated.feature_bits & features::TASK_RESULTS == 0
        { return Err("daemon does not support task results/comparison"); }
        if operation.is_some_and(|value| TaskOperation::try_from(value).is_ok_and(|operation| operation.is_upload()))
            && negotiated.feature_bits & features::TASK_UPLOADS == 0
        { return Err("daemon does not support durable batch uploads"); }
        if operation.is_some_and(|value| TaskOperation::try_from(value).is_ok_and(|operation| operation.is_download()))
            && negotiated.feature_bits & features::TASK_DOWNLOADS == 0 { return Err("daemon does not support durable batch downloads"); }
        if operation.is_some_and(|value| TaskOperation::try_from(value).is_ok_and(|operation| operation.is_mixed()))
            && negotiated.feature_bits & features::MIXED_TASK_CONTROL == 0 { return Err("daemon does not support mixed task control"); }
        if operation.is_some_and(|value|TaskOperation::try_from(value).is_ok_and(|operation|operation.is_mixed_repair()))&&negotiated.feature_bits&features::MIXED_FILE_REPAIR==0{return Err("daemon does not support parent-bound file repair");}
        if advanced_mixed&&negotiated.feature_bits&features::MIXED_ROLLOUT==0{return Err("daemon does not support mixed rollout policy/control");}
        let payload = match task {
            Some(task) => envelope::Payload::TaskRequest(task),
            None => envelope::Payload::SessionListRequest(Default::default()),
        };
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(payload),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "request send failed; inspect state before another mutation")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "reply unavailable; inspect state before another mutation")?;
        if response.request_id != 2 {
            return Err("daemon reply identity mismatch");
        }
        Ok(response)
    })
    .await
    .map_err(|_| "daemon request timed out; inspect state before another mutation")??;
    match response.payload {
        Some(envelope::Payload::SessionListResponse(list)) if is_status => {
            println!(
                "daemon connected; {} terminal sessions",
                list.sessions.len()
            );
        }
        Some(envelope::Payload::TaskResponse(result)) if !is_status => {
            if TaskStatus::try_from(result.status).map_err(|_| "unknown task status")?
                != TaskStatus::Ok
            {
                eprintln!(
                    "task request rejected: {:?}",
                    TaskStatus::try_from(result.status).map_err(|_| "unknown task status")?
                );
                return Err("reload task/catalog state before another mutation");
            }
            if !expected_run.is_empty()
                && operation != Some(TaskOperation::List as i32)
                && operation != Some(TaskOperation::UploadList as i32)
                && operation != Some(TaskOperation::DownloadList as i32)
                && operation != Some(TaskOperation::MixedList as i32)
                && operation != Some(TaskOperation::Retry as i32)
                && result.run_id != expected_run
            {
                return Err("task reply identity mismatch");
            }
            if let Some(expected) = &expected_mixed {
                let operation = TaskOperation::try_from(expected.operation)
                    .map_err(|_| "invalid mixed operation")?;
                if operation != TaskOperation::MixedOutput
                    && (result.revision != 0
                        || !result.output.is_empty()
                        || result.output_length != 0)
                {
                    return Err("unexpected mixed response fields");
                }
                match operation {
                    TaskOperation::MixedCreate | TaskOperation::MixedReview => {
                        let snapshot =
                            cshell_application::decode_mixed_snapshot(&result.document_json)
                                .map_err(|_| "invalid mixed frozen plan")?;
                        if snapshot.id().as_uuid().as_bytes().as_slice() != result.run_id {
                            return Err("mixed plan identity mismatch");
                        }
                        if operation == TaskOperation::MixedCreate {
                            let definition: cshell_application::MixedTaskDefinition =
                                serde_json::from_slice(&expected.definition_json)
                                    .map_err(|_| "invalid submitted mixed definition")?;
                            let selection =
                                cshell_application::decode_host_selection(&expected.selection_json)
                                    .map_err(|_| "invalid submitted mixed selection")?;
                            if snapshot.definition() != &definition
                                || snapshot.targets().selection() != &selection
                                || snapshot.targets().host_revision() != expected.host_revision
                                || snapshot.targets().profile_revision()
                                    != expected.profile_revision
                            {
                                return Err(
                                    "saved mixed plan differs from the submitted definition/selection/revisions",
                                );
                            }
                        }
                    }
                    TaskOperation::MixedGet => {
                        let page = cshell_application::decode_mixed_status(&result.document_json)
                            .map_err(|_| "invalid mixed status page")?;
                        let after = page
                            .after
                            .map(|host| host.as_uuid().as_bytes().to_vec())
                            .unwrap_or_default();
                        if page.run_id.as_uuid().as_bytes().as_slice() != result.run_id
                            || after != expected.host_id
                            || page.limit != expected.limit as usize
                        {
                            return Err("mixed status page identity/cursor mismatch");
                        }
                        if args
                            .get(1)
                            .is_some_and(|command| command == "mixed-confirmation")
                        {
                            let confirmation = page
                                .confirmation()
                                .map_err(|_| "mixed run is active or has no pending targets")?;
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&confirmation)
                                    .map_err(|_| "invalid mixed confirmation")?
                            );
                            return Ok(());
                        }
                    }
                    TaskOperation::MixedList => {
                        let rows = cshell_application::decode_mixed_list(&result.document_json)
                            .map_err(|_| "invalid mixed run list")?;
                        if !expected.run_id.is_empty()
                            && rows.iter().any(|row| {
                                row.id.as_uuid().as_bytes().as_slice() <= expected.run_id.as_slice()
                            })
                        {
                            return Err("mixed list cursor mismatch");
                        }
                    }
                    TaskOperation::MixedOutput => {
                        let context = cshell_application::decode_mixed_output(
                            &result.document_json,
                            &result.output,
                        )
                        .map_err(|_| "invalid mixed output context")?;
                        if context.run_id.as_uuid().as_bytes().as_slice() != expected.run_id
                            || context.host_id.as_uuid().as_bytes().as_slice() != expected.host_id
                            || context.step != expected.step
                            || context.stderr != expected.stderr
                            || context.offset != expected.offset
                            || context.length != result.output_length
                            || context.target_revision != result.revision
                        {
                            return Err("mixed output scope mismatch");
                        }
                        if raw {
                            std::io::stdout()
                                .write_all(&result.output)
                                .map_err(|_| "cannot write output")?;
                        } else {
                            println!(
                                "{}",
                                serde_json::json!({"context":context,"bytes":result.output})
                            );
                        }
                        return Ok(());
                    }
                    TaskOperation::MixedFileStatus => {
                        let document =
                            cshell_application::decode_mixed_repair(&result.document_json)
                                .map_err(|_| "invalid mixed repair status")?;
                        if document.run_id.as_uuid().as_bytes().as_slice() != expected.run_id {
                            return Err("mixed repair identity mismatch");
                        }
                    }
                    TaskOperation::MixedStart
                    | TaskOperation::MixedCancel
                    | TaskOperation::MixedFileProbe
                    | TaskOperation::MixedFileResume
                    | TaskOperation::MixedFileCleanup
                    | TaskOperation::MixedFileAdopt => {
                        if !result.document_json.is_empty() {
                            return Err("unexpected mixed acknowledgement body");
                        }
                    }
                    _ => return Err("unexpected mixed operation"),
                }
            }
            if matches!(operation, Some(value) if value == TaskOperation::Create as i32 || value == TaskOperation::Retry as i32)
                && (result.run_id.len() != 16
                    || result.revision != 1
                    || result.run_id == expected_run)
            {
                return Err("invalid saved task reply");
            }
            if matches!(operation, Some(value) if value == TaskOperation::UploadGet as i32 || value == TaskOperation::UploadCreate as i32)
            {
                let document = cshell_application::decode_upload_status(&result.document_json)
                    .map_err(|_| "invalid upload status document")?;
                if document.id.as_uuid().as_bytes().as_slice() != result.run_id {
                    return Err("upload document identity mismatch");
                }
            }
            if operation == Some(TaskOperation::UploadList as i32) {
                let rows = cshell_application::decode_upload_list(&result.document_json)
                    .map_err(|_| "invalid upload list document")?;
                if !expected_run.is_empty()
                    && rows.iter().any(|row| {
                        row.id.as_uuid().as_bytes().as_slice() <= expected_run.as_slice()
                    })
                {
                    return Err("upload list cursor mismatch");
                }
            }
            if matches!(operation, Some(value) if value == TaskOperation::DownloadGet as i32 || value == TaskOperation::DownloadCreate as i32)
            {
                let document = cshell_application::decode_download_status(&result.document_json)
                    .map_err(|_| "invalid download status document")?;
                if document.id.as_uuid().as_bytes().as_slice() != result.run_id {
                    return Err("download document identity mismatch");
                }
            }
            if operation == Some(TaskOperation::DownloadList as i32) {
                let rows = cshell_application::decode_download_list(&result.document_json)
                    .map_err(|_| "invalid download list document")?;
                if !expected_run.is_empty()
                    && rows.iter().any(|row| {
                        row.id.as_uuid().as_bytes().as_slice() <= expected_run.as_slice()
                    })
                {
                    return Err("download list cursor mismatch");
                }
            }
            if operation == Some(TaskOperation::Results as i32) {
                let document = cshell_application::decode_task_results(&result.document_json)
                    .map_err(|_| "invalid result snapshot")?;
                if document.run_id.as_uuid().as_bytes().as_slice() != result.run_id
                    || document.meta.revision != result.revision
                    || expected_revision != 0 && result.revision != expected_revision
                {
                    return Err("result snapshot identity/revision mismatch");
                }
                if let Some((path, format)) = export {
                    tokio::task::spawn_blocking(move || {
                        cshell_ipc::publish_export_file(&path, |file| {
                            cshell_application::write_task_result_export(&document, format, file)
                        })
                    })
                    .await
                    .map_err(|_| "export worker failed; inspect the destination")?
                    .map_err(|_| "export failed; inspect the destination before trying again")?;
                    println!("result snapshot exported without overwriting an existing file");
                    return Ok(());
                }
            }
            if let Some(expected) = expected_comparison {
                let comparison = cshell_application::decode_task_comparison(&result.document_json)
                    .map_err(|_| "invalid output comparison")?;
                if comparison.run_id.as_uuid().as_bytes().as_slice() != expected.run_id
                    || comparison.revision != result.revision
                    || comparison.revision != expected.expected_revision
                    || comparison.left_host.as_uuid().as_bytes().as_slice() != expected.host_id
                    || comparison.right_host.as_uuid().as_bytes().as_slice()
                        != expected.compare_host_id
                    || comparison.step != expected.step
                    || comparison.stderr != expected.stderr
                    || comparison.offset != expected.offset
                {
                    return Err("comparison context mismatch");
                }
            }
            if raw {
                std::io::stdout()
                    .write_all(&result.output)
                    .map_err(|_| "cannot write output")?;
            } else if operation == Some(TaskOperation::Output as i32) {
                println!(
                    "{}",
                    serde_json::json!({ "offset": output_offset, "next_offset": output_offset.saturating_add(result.output.len() as u64), "length": result.output_length, "bytes": result.output })
                );
            } else if !result.document_json.is_empty() {
                let value: serde_json::Value = serde_json::from_slice(&result.document_json)
                    .map_err(|_| "invalid task document reply")?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value)
                        .map_err(|_| "invalid task document reply")?
                );
            } else if matches!(operation, Some(value) if value == TaskOperation::UploadProbe as i32 || value == TaskOperation::UploadCleanup as i32 || value == TaskOperation::UploadAdopt as i32
                || value == TaskOperation::DownloadProbe as i32 || value == TaskOperation::DownloadCleanup as i32 || value == TaskOperation::DownloadAdopt as i32)
            {
                println!(
                    "recovery job accepted; query the corresponding upload-get/download-get to observe its result"
                );
            } else {
                println!("request accepted; query the run to observe its saved state");
            }
        }
        _ => return Err("unexpected daemon reply"),
    }
    Ok(())
}
fn download_definition(
    remote: &str,
    directory: &str,
    local_name: &str,
    name: &str,
) -> Result<(), &'static str> {
    let definition = cshell_application::BatchDownloadDefinition {
        publication_policy: None,
        name: name.into(),
        remote_source: remote.into(),
        local_directory: directory.into(),
        local_name: local_name.into(),
        maximum_bytes: cshell_application::MAX_DOWNLOAD_FILE_BYTES,
        concurrency: 5,
        queue_timeout_ms: 300_000,
        connect_timeout_ms: 30_000,
        transfer_timeout_ms: 3_600_000,
    };
    definition
        .validate()
        .map_err(|_| "invalid download name, paths or policy")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&definition)
            .map_err(|_| "cannot encode download definition")?
    );
    Ok(())
}
async fn upload_manifest(source: &str, remote: &str, name: &str) -> Result<(), &'static str> {
    if !std::path::Path::new(source).is_absolute()
        || !cshell_application::upload_remote_path(remote)
        || name.is_empty()
        || name.len() > 128
        || name.chars().any(char::is_control)
    {
        return Err("invalid upload manifest name or absolute paths");
    }
    let source = source.to_owned();
    let remote = remote.to_owned();
    let name = name.to_owned();
    let definition = tokio::task::spawn_blocking(move || {
        let fingerprint = cshell_ipc::fingerprint_upload_source(
            std::path::Path::new(&source),
            cshell_application::MAX_UPLOAD_FILE_BYTES,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .map_err(|_| "upload source hashing failed; inspect the source file")?;
        Ok::<_, &'static str>(cshell_application::BatchUploadDefinition {
            publication_policy: None,
            name,
            local_source: source,
            remote_destination: remote,
            size: fingerprint.size,
            sha256: fingerprint.sha256,
            concurrency: 20,
            queue_timeout_ms: 300_000,
            connect_timeout_ms: 30_000,
            transfer_timeout_ms: 3_600_000,
        })
    })
    .await
    .map_err(|_| "upload source hash worker failed")??;
    definition
        .validate()
        .map_err(|_| "invalid upload manifest")?;
    println!(
        "{}",
        serde_json::to_string_pretty(&definition).map_err(|_| "cannot encode upload manifest")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn start_requires_explicit_confirmation_and_arguments_are_strict()
    -> Result<(), &'static str> {
        let run = uuid::Uuid::now_v7().to_string();
        assert!(
            request(&["start".into(), run.clone(), "1".into()])
                .await
                .is_err()
        );
        let (start, _) =
            request(&["start".into(), run.clone(), "1".into(), "--confirm".into()]).await?;
        assert!(start.confirmed);
        assert!(
            request(&["cancel".into(), run, "--confirm".into()])
                .await
                .is_err()
        );
        assert!(id(&uuid::Uuid::nil().to_string()).is_err());
        Ok(())
    }
}
