use cshell_application::*;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, ProfileImportPolicy, ProfileOperation as Op,
    ProfileRequest, ProfileResponse, ProfileStatus, RuntimePaths, client_handshake, envelope,
    features, read_envelope, write_envelope,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    time::Duration,
};
use zeroize::Zeroizing;
const MAX_REVIEW: usize = 2 * 1024 * 1024;
#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    Stop,
    Skip,
    Replace,
}
impl Policy {
    fn parse(text: &str) -> Result<Self, &'static str> {
        match text {
            "stop" => Ok(Self::Stop),
            "skip" => Ok(Self::Skip),
            "replace" => Ok(Self::Replace),
            _ => Err("policy must be stop, skip or replace"),
        }
    }
    fn application(self) -> ImportConflictPolicy {
        match self {
            Self::Stop => ImportConflictPolicy::Fail,
            Self::Skip => ImportConflictPolicy::Skip,
            Self::Replace => ImportConflictPolicy::Replace,
        }
    }
    fn wire(self) -> i32 {
        (match self {
            Self::Stop => ProfileImportPolicy::Fail,
            Self::Skip => ProfileImportPolicy::Skip,
            Self::Replace => ProfileImportPolicy::Replace,
        }) as i32
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    kind: String,
    name: String,
    action: String,
    details: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    format: String,
    version: u32,
    source: String,
    document: ProfileImportDocument,
    document_sha256: [u8; 32],
    summary: ForeignProfileImportSummary,
    policy: Policy,
    revision: u64,
    can_commit: bool,
    change_count: usize,
    rows: Vec<Row>,
}
fn rows(preview: &ProfileImportPreview) -> Vec<Row> {
    preview
        .items
        .iter()
        .map(|row| Row {
            kind: match row.kind {
                ImportItemKind::Folder => "folder",
                ImportItemKind::Profile => "profile",
                ImportItemKind::Theme => "theme",
            }
            .into(),
            name: row.name.clone(),
            action: match row.action {
                ImportAction::Create => "create",
                ImportAction::Skip => "skip",
                ImportAction::Replace => "replace",
                ImportAction::Conflict => "conflict",
            }
            .into(),
            details: row.details.clone(),
        })
        .collect()
}
fn read(path: &str, limit: usize) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open import input")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect import input")?
        .is_file()
    {
        return Err("import input must be a regular file");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read import input")?;
    if bytes.len() > limit {
        return Err("import input limit");
    }
    Ok(bytes)
}
fn json(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)
}
fn publish(path: &str, bytes: &[u8]) -> Result<(), &'static str> {
    let path = std::path::Path::new(path);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "cannot stage import output")?;
    file.write_all(bytes)
        .map_err(|_| "import output write failed")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "import output sync failed")?;
    file.persist_noclobber(path)
        .map_err(|_| "output exists or publication failed")?;
    Ok(())
}
#[derive(Clone, Copy)]
enum Source {
    Putty,
    SecureCrt,
    Xshell,
    MobaXterm(MobaXtermEncoding),
    Batch(cshell_import::ForeignBatchInput),
}
fn convert(
    path: &str,
    account: &str,
    source: Source,
) -> Result<ForeignProfileImportResult, &'static str> {
    if let Source::Batch(input) = source {
        return cshell_import::load_foreign_batch(
            std::path::Path::new(path),
            input,
            account,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .map_err(|error| {
            eprintln!("{error}");
            "migration batch refused"
        });
    }
    let bytes = read(path, MAX_PROFILE_IMPORT_BYTES)?;
    match source {
        Source::Batch(_) => Err("invalid migration dispatch"),
        Source::Putty => import_putty_registry(
            &bytes,
            PuttyImportOptions {
                default_username: account.trim(),
            },
        )
        .map(Into::into)
        .map_err(|error| {
            eprintln!("{error}");
            "PuTTY metadata conversion refused"
        }),
        Source::MobaXterm(encoding) => import_mobaxterm_sessions(
            &bytes,
            MobaXtermImportOptions {
                default_username: account.trim(),
                encoding,
            },
        )
        .map_err(|error| {
            eprintln!("{error}");
            "MobaXterm metadata conversion refused"
        }),
        Source::SecureCrt | Source::Xshell => {
            let name = std::path::Path::new(path)
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or("Session filename must be UTF-8")?;
            if matches!(source, Source::Xshell) {
                return import_xshell_session(
                    &bytes,
                    XshellImportOptions {
                        session_name: name,
                        default_username: account.trim(),
                    },
                )
                .map_err(|error| {
                    eprintln!("{error}");
                    "Xshell metadata conversion refused"
                });
            }
            import_securecrt_ini(
                &bytes,
                SecureCrtImportOptions {
                    session_name: name,
                    default_username: account.trim(),
                },
            )
            .map_err(|error| {
                eprintln!("{error}");
                "SecureCRT metadata conversion refused"
            })
        }
    }
}

pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if let Some(op) = args.first() {
        let batch = match op.as_str() {
            "convert-securecrt-dir" | "preview-securecrt-dir" => {
                Some(cshell_import::ForeignBatchInput::SecureCrtDirectory)
            }
            "convert-xshell-dir" | "preview-xshell-dir" => {
                Some(cshell_import::ForeignBatchInput::XshellDirectory)
            }
            "convert-manifest" | "preview-manifest" => {
                Some(cshell_import::ForeignBatchInput::Manifest)
            }
            _ => None,
        };
        if let Some(batch) = batch {
            let source = Source::Batch(batch);
            return match args {
                [op, input, output] if op.starts_with("convert-") => {
                    convert_file(input, output, "", source)
                }
                [op, input, output, account]
                    if op.starts_with("convert-") && !account.starts_with("--") =>
                {
                    convert_file(input, output, account, source)
                }
                [op, input, output, policy] if op.starts_with("preview-") => {
                    preview_file(input, output, "", Policy::parse(policy)?, source).await
                }
                [op, input, output, policy, account]
                    if op.starts_with("preview-") && !account.starts_with("--") =>
                {
                    preview_file(input, output, account, Policy::parse(policy)?, source).await
                }
                _ => Err(crate::USAGE),
            };
        }
    }
    if let Some(op) = args.first()
        && matches!(op.as_str(), "convert-mobaxterm" | "preview-mobaxterm")
    {
        return run_mobaxterm(args).await;
    }
    if let [op, path] = args
        && op == "inspect-ppk"
    {
        return inspect_ppk_file(path, false).await;
    }
    if let [op, path, flag] = args
        && op == "inspect-ppk"
        && flag == "--passphrase-stdin"
    {
        return inspect_ppk_file(path, true).await;
    }
    match args {
        [op, input, output] if op == "convert-putty" => {
            convert_file(input, output, "", Source::Putty)?
        }
        [op, input, output, account] if op == "convert-putty" => {
            convert_file(input, output, account, Source::Putty)?
        }
        [op, input, output, policy] if op == "preview-putty" => {
            preview_file(input, output, "", Policy::parse(policy)?, Source::Putty).await?
        }
        [op, input, output, policy, account] if op == "preview-putty" => {
            preview_file(
                input,
                output,
                account,
                Policy::parse(policy)?,
                Source::Putty,
            )
            .await?
        }
        [op, input, output] if op == "convert-securecrt" => {
            convert_file(input, output, "", Source::SecureCrt)?
        }
        [op, input, output, account] if op == "convert-securecrt" => {
            convert_file(input, output, account, Source::SecureCrt)?
        }
        [op, input, output, policy] if op == "preview-securecrt" => {
            preview_file(input, output, "", Policy::parse(policy)?, Source::SecureCrt).await?
        }
        [op, input, output, policy, account] if op == "preview-securecrt" => {
            preview_file(
                input,
                output,
                account,
                Policy::parse(policy)?,
                Source::SecureCrt,
            )
            .await?
        }
        [op, input, output] if op == "convert-xshell" => {
            convert_file(input, output, "", Source::Xshell)?
        }
        [op, input, output, account] if op == "convert-xshell" => {
            convert_file(input, output, account, Source::Xshell)?
        }
        [op, input, output, policy] if op == "preview-xshell" => {
            preview_file(input, output, "", Policy::parse(policy)?, Source::Xshell).await?
        }
        [op, input, output, policy, account] if op == "preview-xshell" => {
            preview_file(
                input,
                output,
                account,
                Policy::parse(policy)?,
                Source::Xshell,
            )
            .await?
        }
        [op, input, confirm] if op == "apply" && confirm == "--confirm" => apply(input).await?,
        _ => return Err(crate::USAGE),
    }
    Ok(())
}
async fn run_mobaxterm(args: &[String]) -> Result<(), &'static str> {
    // Encoding is an explicit trailing flag; never retry a failed Unicode read
    // under a different codepage or obtain a default account from this machine.
    let (args, encoding) = if args.last().is_some_and(|s| s == "--windows-1252") {
        (&args[..args.len() - 1], MobaXtermEncoding::Windows1252)
    } else {
        (args, MobaXtermEncoding::Unicode)
    };
    let source = Source::MobaXterm(encoding);
    match args {
        [op, input, output] if op == "convert-mobaxterm" => convert_file(input, output, "", source),
        [op, input, output, account] if op == "convert-mobaxterm" && !account.starts_with("--") => {
            convert_file(input, output, account, source)
        }
        [op, input, output, policy] if op == "preview-mobaxterm" => {
            preview_file(input, output, "", Policy::parse(policy)?, source).await
        }
        [op, input, output, policy, account]
            if op == "preview-mobaxterm" && !account.starts_with("--") =>
        {
            preview_file(input, output, account, Policy::parse(policy)?, source).await
        }
        _ => Err(crate::USAGE),
    }
}
fn convert_file(
    input: &str,
    output: &str,
    account: &str,
    source: Source,
) -> Result<(), &'static str> {
    let converted = convert(input, account, source)?;
    let bytes = serde_json::to_vec_pretty(&converted.document)
        .map_err(|_| "converted import encoding failed")?;
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err("converted import limit");
    }
    publish(output, &bytes)?;
    let notices = converted.summary.notices();
    println!(
        "{}",
        serde_json::json!({"summary":converted.summary,"notices":notices})
    );
    Ok(())
}
async fn preview_file(
    input: &str,
    output: &str,
    account: &str,
    policy: Policy,
    source: Source,
) -> Result<(), &'static str> {
    let converted = convert(input, account, source)?;
    let snapshot = catalog(query(request(Op::List)).await?)?;
    let (_, preview) = plan_profile_import(&snapshot, &converted.document, policy.application())
        .map_err(|_| "import planning failed")?;
    let document_sha256 = converted
        .document
        .fingerprint()
        .map_err(|_| "converted document invalid")?;
    let review = Review {
        format: "cshell.foreign-profile-review".into(),
        version: 1,
        source: converted.summary.source().into(),
        document: converted.document,
        document_sha256,
        summary: converted.summary,
        policy,
        revision: snapshot.revision,
        can_commit: preview.can_commit,
        change_count: preview.change_count,
        rows: rows(&preview),
    };
    let bytes = serde_json::to_vec_pretty(&review).map_err(|_| "import review encoding failed")?;
    if bytes.len() > MAX_REVIEW {
        return Err("import review limit");
    }
    publish(output, &bytes)?;
    println!(
        "{}",
        serde_json::json!({"revision":review.revision,"can_commit":review.can_commit,"changes":review.change_count,"notices":review.summary.notices()})
    );
    Ok(())
}
fn validate_review(review: &Review) -> Result<(), &'static str> {
    if review.format != "cshell.foreign-profile-review"
        || review.version != 1
        || review.source != review.summary.source()
        || review.rows.len() > 2048
        || !review.summary.validate(&review.document)
        || review
            .document
            .fingerprint()
            .map_err(|_| "invalid review document")?
            != review.document_sha256
    {
        return Err("invalid or edited import review; preview again");
    }
    Ok(())
}

async fn apply(input: &str) -> Result<(), &'static str> {
    let bytes = read(input, MAX_REVIEW)?;
    let review: Review =
        serde_json::from_slice(json(&bytes)).map_err(|_| "invalid import review")?;
    validate_review(&review)?;
    let snapshot = catalog(query(request(Op::List)).await?)?;
    if snapshot.revision != review.revision {
        return Err("catalog changed; create a new preview");
    }
    let (changes, preview) =
        plan_profile_import(&snapshot, &review.document, review.policy.application())
            .map_err(|_| "import replan failed")?;
    if !preview.can_commit
        || preview.change_count == 0
        || preview.can_commit != review.can_commit
        || preview.change_count != review.change_count
        || rows(&preview) != review.rows
    {
        return Err("review scope/effect changed or has no changes; preview again");
    }
    let mut expected = ProfileCatalog::from_snapshot(snapshot).map_err(|_| "catalog invalid")?;
    expected
        .apply_batch(review.revision, &changes)
        .map_err(|_| "import effect invalid")?;
    let mut outgoing = request(Op::CommitImport);
    outgoing.expected_revision = review.revision;
    outgoing.import_json =
        serde_json::to_vec(&review.document).map_err(|_| "import encoding failed")?;
    outgoing.import_policy = review.policy.wire();
    let committed = catalog(query(outgoing).await?)?;
    if normalize(committed.clone()) != normalize(expected.snapshot()) {
        return Err(
            "committed import receipt differs from reviewed catalog; outcome unknown, do not replay",
        );
    }
    println!(
        "{}",
        serde_json::json!({"revision":committed.revision,"changes":changes.len(),"notices":review.summary.notices()})
    );
    Ok(())
}
fn request(operation: Op) -> ProfileRequest {
    let mut request = ProfileRequest::default();
    request.operation = operation as i32;
    request
}
fn normalize(mut snapshot: CatalogSnapshot) -> CatalogSnapshot {
    snapshot.folders.sort_by_key(|f| f.id);
    snapshot.profiles.sort_by_key(|p| p.id);
    snapshot.ssh_connections.sort_by_key(|c| c.profile_id);
    snapshot.local_connections.sort_by_key(|c| c.profile_id);
    snapshot.themes.sort_by(|a, b| a.name.cmp(&b.name));
    snapshot
}
fn catalog(response: ProfileResponse) -> Result<CatalogSnapshot, &'static str> {
    let data = response.catalog.ok_or("Profile catalog receipt missing")?;
    if response.revision != data.revision {
        return Err("Profile catalog revision mismatch");
    }
    let snapshot = CatalogSnapshot {
        revision: data.revision,
        defaults: data.defaults.ok_or("Profile defaults missing")?.into(),
        folders: data
            .folders
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid folder receipt")?,
        profiles: data
            .profiles
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid Profile receipt")?,
        ssh_connections: data
            .ssh_connections
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid SSH receipt")?,
        local_connections: data
            .local_connections
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid Local receipt")?,
        themes: data
            .themes
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<_, _>>()
            .map_err(|_| "invalid theme receipt")?,
    };
    ProfileCatalog::from_snapshot(snapshot.clone()).map_err(|_| "invalid catalog receipt")?;
    Ok(snapshot)
}
async fn inspect_ppk_file(path: &str, from_stdin: bool) -> Result<(), &'static str> {
    let bytes = read(path, cshell_ssh::MAX_PPK_BYTES)?;
    let encoded = Zeroizing::new(
        std::str::from_utf8(&bytes)
            .map_err(|_| "PPK input must be UTF-8")?
            .to_owned(),
    );
    let passphrase = if from_stdin {
        // Literal passphrase, never a process argument or environment variable.
        // Accept a single final LF/CRLF from a pipe, without trimming spaces.
        let mut bytes = Zeroizing::new(Vec::new());
        std::io::stdin()
            .take(16 * 1024 + 3)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read passphrase input")?;
        if bytes.ends_with(b"\n") {
            bytes.pop();
            if bytes.ends_with(b"\r") {
                bytes.pop();
            }
        }
        if bytes.len() > 16 * 1024 || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
            return Err("passphrase input must be one bounded line");
        }
        Some(Zeroizing::new(
            std::str::from_utf8(&bytes)
                .map_err(|_| "passphrase input must be UTF-8")?
                .to_owned(),
        ))
    } else {
        None
    };
    let facts = tokio::task::spawn_blocking(move || {
        cshell_ssh::inspect_ppk(&encoded, passphrase.as_ref().map(|p| p.as_str()))
    })
    .await
    .map_err(|_| "PPK inspection worker unavailable")?
    .map_err(|error| {
        eprintln!("{error}");
        "PPK inspection refused"
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(&facts).map_err(|_| "PPK metadata encoding failed")?
    );
    Ok(())
}
async fn query(request: ProfileRequest) -> Result<ProfileResponse, &'static str> {
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "authenticated daemon discovery missing")?;
    tokio::time::timeout(Duration::from_secs(40), async {
        #[cfg(windows)]
        let mut stream = cshell_ipc::transport::connect(&discovery.endpoint.to_string_lossy())
            .await
            .map_err(|_| "daemon connection failed")?;
        #[cfg(unix)]
        let mut stream = cshell_ipc::transport::connect(std::path::Path::new(&discovery.endpoint))
            .await
            .map_err(|_| "daemon connection failed")?;
        let mut hello = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        hello.feature_bits = features::PROFILE_CONTROL
            | features::PROFILE_CONNECTION_IMPORT
            | features::LOCAL_PROFILE
            | features::LOCAL_LAUNCH_OPTIONS
            | features::THEME_TRANSFER
            | features::SSH_PROFILE_AUTH
            | features::SSH_PROFILE_ROUTE;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits
            & (features::PROFILE_CONTROL | features::PROFILE_CONNECTION_IMPORT)
            != (features::PROFILE_CONTROL | features::PROFILE_CONNECTION_IMPORT)
        {
            return Err("daemon lacks Profile connection import capability");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ProfileRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "import outcome unknown; read and preview again, do not replay")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "import reply outcome unknown; do not replay commit")?;
        if response.request_id != 2 {
            return Err("import reply identity mismatch");
        }
        let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
            return Err("import reply type mismatch");
        };
        if response.detail.len() > 4096 {
            return Err("import reply detail limit");
        }
        if response.status != ProfileStatus::Ok as i32 {
            eprintln!("{}", response.detail);
            return Err("import refused or uncertain; read and preview again");
        }
        Ok(response)
    })
    .await
    .map_err(|_| "import operation timed out; outcome unknown, do not replay commit")?
}
