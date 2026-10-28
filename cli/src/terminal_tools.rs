use cshell_application::*;
use cshell_domain::SessionId;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, TerminalToolsOperation as Op,
    TerminalToolsRequest as Request, TerminalToolsStatus as Status, client_handshake, envelope,
    features, read_envelope, write_envelope,
};
use std::{
    io::{Read, Write},
    time::Duration,
};
fn id(value: &str) -> Result<SessionId, &'static str> {
    let id: SessionId = serde_json::from_value(serde_json::Value::String(value.into()))
        .map_err(|_| "invalid session UUID")?;
    if id.as_uuid().is_nil() {
        Err("nil session UUID")
    } else {
        Ok(id)
    }
}
fn number(value: &str) -> Result<u64, &'static str> {
    value.parse().map_err(|_| "invalid numeric argument")
}
fn read(path: &str, maximum: usize) -> Result<Vec<u8>, &'static str> {
    let file = std::fs::File::open(path).map_err(|_| "cannot open input file")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect input")?
        .is_file()
    {
        return Err("input must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read input")?;
    if bytes.len() > maximum {
        return Err("input file limit");
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
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| "cannot stage export")?;
    temporary
        .write_all(bytes)
        .map_err(|_| "export write failed")?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| "export sync failed")?;
    temporary
        .persist_noclobber(path)
        .map_err(|_| "export exists or publication failed")?;
    Ok(())
}
pub(crate) async fn exchange(
    payload: envelope::Payload,
    capabilities: u64,
) -> Result<envelope::Payload, &'static str> {
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
        hello.feature_bits = capabilities;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & capabilities != capabilities {
            return Err("daemon lacks the required terminal capability");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(payload),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "terminal request outcome unknown; reload, do not replay save/arm")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "terminal reply unavailable; reload before changing state")?;
        if reply.request_id != 2 {
            return Err("terminal reply identity mismatch");
        }
        reply.payload.ok_or("terminal reply missing")
    })
    .await
    .map_err(|_| "terminal request timed out; do not replay save/arm")?
}
async fn query(request: &Request) -> Result<TerminalToolsReply, &'static str> {
    let payload = exchange(
        envelope::Payload::TerminalToolsRequest(request.clone()),
        features::TERMINAL_TOOLS,
    )
    .await?;
    let envelope::Payload::TerminalToolsResponse(reply) = payload else {
        return Err("unexpected terminal tools reply");
    };
    if reply.detail.len() > 1024 {
        return Err("terminal tools detail limit");
    }
    if reply.status != Status::Ok as i32 {
        return Err("terminal tools refused or unavailable; reload state");
    }
    let saved = if request.operation == Op::Save as i32 {
        Some(decode_terminal_tools(&request.document_json)?)
    } else {
        None
    };
    let session = if request.session_id.is_empty() {
        None
    } else {
        Some(SessionId::from_bytes(
            request
                .session_id
                .as_slice()
                .try_into()
                .map_err(|_| "invalid session scope")?,
        ))
    };
    let expect = match Op::try_from(request.operation).map_err(|_| "invalid terminal operation")? {
        Op::ShellHistory => TerminalToolsExpectation::Shell {session:session.ok_or("missing session scope")?,id:request.cursor},
        Op::Get => TerminalToolsExpectation::View { after: 0 },
        Op::Poll => TerminalToolsExpectation::View {
            after: request.cursor,
        },
        Op::Save => TerminalToolsExpectation::Saved(saved.as_ref().ok_or("missing save scope")?),
        Op::Arm => TerminalToolsExpectation::Armed {
            session: session.ok_or("missing session scope")?,
            revision: request.expected_revision,
            allow_text: request.allow_text,
        },
        Op::Disarm => TerminalToolsExpectation::Disarmed(session.ok_or("missing session scope")?),
        Op::RawJournal => TerminalToolsExpectation::Raw {
            session: session.ok_or("missing session scope")?,
            sequence: request.cursor,
            offset: request.offset,
        },
    };
    decode_terminal_tools_reply(&reply.document_json, expect)
}
async fn catalog() -> Result<TerminalToolsView, &'static str> {
    let TerminalToolsReply::View(view) = query(&Request::default()).await? else {
        return Err("catalog reply missing");
    };
    Ok(view)
}
async fn theme(
    request: cshell_ipc::ProfileRequest,
) -> Result<cshell_ipc::ProfileResponse, &'static str> {
    let envelope::Payload::ProfileResponse(response) = exchange(
        envelope::Payload::ProfileRequest(request),
        features::PROFILE_CONTROL | features::THEME_TRANSFER,
    )
    .await?
    else {
        return Err("unexpected Theme reply");
    };
    if response.detail.len() > 1024 || response.status != cshell_ipc::ProfileStatus::Ok as i32 {
        return Err("Theme request refused or uncertain");
    }
    Ok(response)
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    let request = match args {
        [op] if op == "get" => Request::default(),
        [op, after] if op == "events" => Request {
            operation: Op::Poll as i32,
            cursor: number(after)?,
            ..Default::default()
        },
        [op, rev, path, confirm] if op == "save" && confirm == "--confirm" => {
            let proposed = decode_terminal_tools(json(&read(path, MAX_TERMINAL_TOOLS_BYTES)?))?;
            let revision = number(rev)?;
            if proposed.revision != revision {
                return Err("document revision must match the requested revision");
            }
            Request {
                operation: Op::Save as i32,
                expected_revision: revision,
                document_json: serde_json::to_vec(&proposed)
                    .map_err(|_| "cannot encode terminal tools")?,
                confirmed: true,
                ..Default::default()
            }
        }
        [op, session, rev, confirm] if op == "arm" && confirm == "--confirm" => Request {
            operation: Op::Arm as i32,
            session_id: id(session)?.as_uuid().as_bytes().to_vec(),
            expected_revision: number(rev)?,
            confirmed: true,
            ..Default::default()
        },
        [op, session, rev, permission, confirm]
            if op == "arm" && permission == "--allow-text" && confirm == "--confirm" =>
        {
            Request {
                operation: Op::Arm as i32,
                session_id: id(session)?.as_uuid().as_bytes().to_vec(),
                expected_revision: number(rev)?,
                allow_text: true,
                confirmed: true,
                ..Default::default()
            }
        }
        [op, session] if op == "disarm" => Request {
            operation: Op::Disarm as i32,
            session_id: id(session)?.as_uuid().as_bytes().to_vec(),
            ..Default::default()
        },
        [op, session, sequence] if op == "hex" || op == "journal" => Request {
            operation: Op::RawJournal as i32,
            session_id: id(session)?.as_uuid().as_bytes().to_vec(),
            cursor: number(sequence)?,
            ..Default::default()
        },
        [op, session, sequence, offset] if op == "hex" || op == "journal" => Request {
            operation: Op::RawJournal as i32,
            session_id: id(session)?.as_uuid().as_bytes().to_vec(),
            cursor: number(sequence)?,
            offset: number(offset)?,
            ..Default::default()
        },
        [op, path] if op == "highlights-export" => {
            let view = catalog().await?;
            let bytes = serde_json::to_vec_pretty(&HighlightDocument {
                format: "cshell.highlight-sets".into(),
                version: 1,
                sets: view.catalog.highlights,
            })
            .map_err(|_| "cannot encode HighlightSet")?;
            publish(path, &bytes)?;
            return Ok(());
        }
        [op, rev, path, confirm] if op == "highlights-import" && confirm == "--confirm" => {
            let imported = HighlightDocument::parse(json(&read(path, MAX_TERMINAL_TOOLS_BYTES)?))?;
            let mut view = catalog().await?;
            let revision = number(rev)?;
            if view.catalog.revision != revision {
                return Err("HighlightSet import revision conflict");
            }
            view.catalog.highlights = imported.sets;
            view.catalog.validate()?;
            Request {
                operation: Op::Save as i32,
                expected_revision: revision,
                document_json: serde_json::to_vec(&view.catalog)
                    .map_err(|_| "cannot encode HighlightSet import")?,
                confirmed: true,
                ..Default::default()
            }
        }
        [op, session, sequence, count, path] if op == "export-ansi" => {
            let start = number(sequence)?;
            let count = number(count)?;
            if count == 0 || count > 4096 {
                return Err("ANSI export record limit is 4096");
            }
            let end = start
                .checked_add(count)
                .ok_or("ANSI export range overflow")?;
            let mut request = Request {
                operation: Op::RawJournal as i32,
                session_id: id(session)?.as_uuid().as_bytes().to_vec(),
                cursor: start,
                ..Default::default()
            };
            let mut bytes = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while request.cursor < end {
                let TerminalToolsReply::Raw(raw) =
                    tokio::time::timeout_at(deadline, query(&request))
                        .await
                        .map_err(|_| "ANSI export exceeded 120 seconds")??
                else {
                    return Err("raw journal reply missing");
                };
                if raw.total_records < end {
                    return Err("ANSI export range unavailable");
                }
                if bytes.len() + raw.data.len() > 8 * 1024 * 1024 {
                    return Err("ANSI export exceeds 8 MiB");
                }
                bytes.extend_from_slice(&raw.data);
                (request.cursor, request.offset) = raw.next_cursor();
            }
            publish(path, &bytes)?;
            return Ok(());
        }
        [op, input, output] if op == "convert-xcs" => {
            let document = ThemeDocument::from_xcs(&read(input, MAX_THEME_BYTES)?)
                .map_err(|_| "invalid XCS input")?;
            let bytes = serde_json::to_vec_pretty(&document)
                .map_err(|_| "cannot encode converted Theme")?;
            publish(output, &bytes)?;
            return Ok(());
        }
        [op, path] if op == "themes-export" => {
            let mut request = cshell_ipc::ProfileRequest::default();
            request.operation = cshell_ipc::ProfileOperation::List as i32;
            let listed = theme(request)
            .await?;
            let mut request = cshell_ipc::ProfileRequest::default();
            request.operation = cshell_ipc::ProfileOperation::ExportThemes as i32;
            request.expected_revision = listed.revision;
            let response = theme(request)
            .await?;
            let document =
                ThemeDocument::parse(&response.export_json).map_err(|_| "invalid Theme export")?;
            let bytes = if std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            {
                document
                    .to_toml()
                    .map_err(|_| "Theme TOML encoding failed")?
            } else {
                response.export_json
            };
            publish(path, &bytes)?;
            return Ok(());
        }
        [op, rev, path, confirm] if op == "themes-import" && confirm == "--confirm" => {
            let revision = number(rev)?;
            let bytes = read(path, MAX_THEME_BYTES)?;
            let document = if std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xcs"))
            {
                ThemeDocument::from_xcs(&bytes)
            } else if std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            {
                ThemeDocument::from_toml(&bytes)
            } else {
                ThemeDocument::parse(json(&bytes))
            }
            .map_err(|_| "invalid Theme input")?;
            if document.themes.is_empty() {
                return Err("Theme import must contain a theme");
            }
            let bytes = serde_json::to_vec(&document).map_err(|_| "cannot encode Theme input")?;
            let mut request = cshell_ipc::ProfileRequest::default();
            request.operation = cshell_ipc::ProfileOperation::PreviewThemes as i32;
            request.import_json = bytes.clone();
            request.import_policy = cshell_ipc::ProfileImportPolicy::Replace as i32;
            let response = theme(request)
            .await?;
            let preview = response.preview.ok_or("Theme preview missing")?;
            if preview.base_revision != revision
                || !preview.can_commit
                || preview.items.len() != document.themes.len()
            {
                return Err("Theme preview revision/scope mismatch");
            }
            for (item, expected) in preview.items.iter().zip(&document.themes) {
                if item
                    .theme
                    .clone()
                    .and_then(|t| cshell_domain::ThemeRecord::try_from(t).ok())
                    .as_ref()
                    != Some(expected)
                {
                    return Err("Theme preview content mismatch");
                }
            }
            let mut request = cshell_ipc::ProfileRequest::default();
            request.operation = cshell_ipc::ProfileOperation::CommitThemes as i32;
            request.expected_revision = revision;
            request.import_json = bytes;
            request.import_policy = cshell_ipc::ProfileImportPolicy::Replace as i32;
            let response = theme(request)
            .await?;
            if Some(response.revision) != revision.checked_add(1) {
                return Err("Theme commit revision mismatch");
            }
            println!("Theme revision {}", response.revision);
            return Ok(());
        }
        _ => return Err(crate::USAGE),
    };
    let reply = query(&request).await?;
    if args.first().is_some_and(|s| s == "hex")
        && let TerminalToolsReply::Raw(raw) = &reply
    {
        for (index, row) in raw.data.chunks(16).enumerate() {
            let hex = row
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let text = row
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        char::from(*b)
                    } else {
                        '.'
                    }
                })
                .collect::<String>();
            println!(
                "{:012x}  {hex:<47}  |{text}|",
                raw.offset + index as u64 * 16
            );
        }
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&reply).map_err(|_| "terminal reply encoding failed")?
        );
    }
    Ok(())
}
