use super::*;

fn take_wire(protocol: &mut Protocol) -> Vec<u8> {
    match protocol.poll() {
        Action::WriteWire(bytes) => {
            let wire = bytes.to_vec();
            let n = wire.len();
            protocol.written(n);
            wire
        }
        other => panic!("expected wire, got {other:?}"),
    }
}
fn feed_fragmented(transfer: &mut Transfer, wire: &[u8]) {
    for chunk in wire.chunks(3) {
        assert!(transfer.incoming(chunk).is_empty());
    }
}

#[test]
fn probe_handles_every_split_and_rejects_crc_corruption_and_ordinary_output() {
    let mut peer = Protocol::Receive(Receiver::new().unwrap());
    let header = take_wire(&mut peer);
    for split in 0..=header.len() {
        let mut t = Transfer::default();
        let mut bytes = t.incoming(&header[..split]);
        bytes.extend(t.incoming(&header[split..]));
        assert!(bytes.is_empty());
        assert!(t.busy());
        assert_eq!(t.status.unwrap().direction, 1);
    }
    let normal = b"hello * **x **\x18B01000000230000\r\n";
    let mut t = Transfer::default();
    let mut out = t.incoming(normal);
    t.candidate_since = Some(Instant::now() - Duration::from_secs(1));
    out.extend(t.flush_candidate());
    assert_eq!(out, normal);
    assert!(!t.busy());
    let mut t = Transfer::default();
    assert_eq!(t.incoming(b"prompt*"), b"prompt");
    t.candidate_since = Some(Instant::now() - Duration::from_secs(1));
    assert_eq!(t.flush_candidate(), b"*");
}

#[test]
fn download_streams_binary_and_empty_files_and_publishes_without_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.bin"), b"keep").unwrap();
    let data: Vec<_> = (0..192 * 1024).map(|i| (i % 256) as u8).collect();
    let mut sender = Sender::new().unwrap();
    sender
        .start_file(FileInfo::new(
            b"data.bin",
            Some(Position::new(data.len() as u32)),
        ))
        .unwrap();
    let mut peer = Protocol::Send(sender);
    let initial = take_wire(&mut peer);
    let mut t = Transfer::default();
    feed_fragmented(&mut t, &initial);
    let id = t.status.as_ref().unwrap().transfer_id;
    t.control(&ZmodemRequest {
        operation: 2,
        transfer_id: id,
        paths: vec![dir.path().to_string_lossy().into()],
        ..Default::default()
    })
    .unwrap();
    let mut to_peer = Vec::new();
    let mut offset = 0;
    let mut file = 0;
    let mut done = false;
    for _ in 0..20000 {
        to_peer.extend(t.pump());
        match peer.poll() {
            Action::WriteWire(wire) => {
                let wire = wire.to_vec();
                peer.written(wire.len());
                feed_fragmented(&mut t, &wire);
            }
            Action::ReadFile {
                offset: pos,
                max_len,
            } => {
                let bytes = if file == 0 { &data[..] } else { &[] };
                let start = pos.get() as usize;
                let end = (start + max_len).min(bytes.len());
                if let Protocol::Send(s) = &mut peer {
                    s.submit_file(&bytes[start..end]).unwrap();
                }
            }
            Action::Event(Event::FileCompleted) => {
                file += 1;
                if let Protocol::Send(s) = &mut peer {
                    if file == 1 {
                        s.start_file(FileInfo::new(b"empty.txt", Some(Position::ZERO)))
                            .unwrap();
                    } else {
                        s.finish().unwrap();
                    }
                }
            }
            Action::Event(Event::SessionCompleted) => {
                done = true;
            }
            Action::Idle => {
                if offset < to_peer.len() {
                    offset += peer.submit(&to_peer[offset..]).unwrap();
                }
            }
            other => panic!("unexpected {other:?}"),
        }
        if done && t.status.as_ref().unwrap().state == 3 {
            break;
        }
    }
    assert!(done, "peer unfinished: {:?}", t.status);
    assert_eq!(t.status.as_ref().unwrap().state, 3, "{:?}", t.status);
    assert_eq!(t.status.as_ref().unwrap().files_completed, 2);
    assert_eq!(std::fs::read(dir.path().join("data.bin")).unwrap(), b"keep");
    assert_eq!(
        std::fs::read(dir.path().join("data (1).bin")).unwrap(),
        data
    );
    assert_eq!(std::fs::read(dir.path().join("empty.txt")).unwrap(), b"");
    assert_eq!(t.incoming(b"shell$ "), b"shell$ ");
}

#[test]
fn upload_multiple_files_matches_peer_bytes_and_finishes_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let data: Vec<_> = (0..160 * 1024).map(|i| (i % 251) as u8).collect();
    let paths = vec![dir.path().join("upload.bin"), dir.path().join("empty")];
    std::fs::write(&paths[0], &data).unwrap();
    std::fs::write(&paths[1], b"").unwrap();
    let mut peer = Protocol::Receive(Receiver::new().unwrap());
    let initial = take_wire(&mut peer);
    let mut t = Transfer::default();
    feed_fragmented(&mut t, &initial);
    let id = t.status.as_ref().unwrap().transfer_id;
    t.control(&ZmodemRequest {
        operation: 1,
        transfer_id: id,
        paths: paths.iter().map(|p| p.to_string_lossy().into()).collect(),
        ..Default::default()
    })
    .unwrap();
    let mut to_peer = Vec::new();
    let mut offset = 0;
    let mut received = Vec::new();
    let mut names = Vec::new();
    let mut done = false;
    for _ in 0..20000 {
        to_peer.extend(t.pump());
        match peer.poll() {
            Action::WriteWire(wire) => {
                let wire = wire.to_vec();
                peer.written(wire.len());
                feed_fragmented(&mut t, &wire);
            }
            Action::WriteFile(bytes) => {
                let n = bytes.len();
                received.extend_from_slice(bytes);
                if let Protocol::Receive(r) = &mut peer {
                    r.file_written(n).unwrap();
                }
            }
            Action::Event(Event::FileStarted(info)) => names.push(info.name.to_vec()),
            Action::Event(Event::FileCompleted) => {}
            Action::Event(Event::SessionCompleted) => done = true,
            Action::Idle => {
                if offset < to_peer.len() {
                    offset += peer.submit(&to_peer[offset..]).unwrap();
                }
            }
            other => panic!("unexpected {other:?}"),
        }
        if done && t.status.as_ref().unwrap().state == 3 {
            break;
        }
    }
    assert!(done, "peer unfinished: {:?}", t.status);
    assert_eq!(t.status.as_ref().unwrap().state, 3, "{:?}", t.status);
    assert_eq!(t.status.as_ref().unwrap().files_completed, 2);
    assert_eq!(names, vec![b"upload.bin".to_vec(), b"empty".to_vec()]);
    assert_eq!(received, data);
}

#[test]
fn cancellation_disconnect_and_stale_control_remove_partial_files() {
    let mut peer = Protocol::Receive(Receiver::new().unwrap());
    let wire = take_wire(&mut peer);
    let mut t = Transfer::default();
    t.incoming(&wire);
    let id = t.status.as_ref().unwrap().transfer_id;
    assert!(
        t.control(&ZmodemRequest {
            operation: 3,
            transfer_id: id + 1,
            ..Default::default()
        })
        .is_err()
    );
    t.control(&ZmodemRequest {
        operation: 3,
        transfer_id: id,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(t.pump(), CANCEL);
    assert!(!t.busy());
    assert_eq!(t.incoming(b"prompt"), b"prompt");
    t.incoming(&wire);
    assert_ne!(t.status.as_ref().unwrap().transfer_id, id);
    let dir = tempfile::tempdir().unwrap();
    t.destination = Some(tempfile::NamedTempFile::new_in(dir.path()).unwrap());
    t.disconnected();
    assert!(!t.busy());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(t.pump().is_empty());
}

#[test]
fn rejects_untrusted_names_and_waiting_transfer_times_out() {
    for name in [
        "../secret",
        "..",
        "C:\\escape",
        "/etc/passwd",
        "a/b",
        "a\\b",
        "a:stream",
        "CON",
        "LPT1.txt",
        "trailing.",
        "a\0b",
    ] {
        assert!(safe_name(name.as_bytes()).is_err(), "{name}");
    }
    assert_eq!(safe_name("文件.txt".as_bytes()).unwrap(), "文件.txt");
    let mut peer = Protocol::Receive(Receiver::new().unwrap());
    let mut t = Transfer::default();
    t.incoming(&take_wire(&mut peer));
    t.began = Some(Instant::now() - Duration::from_secs(181));
    assert_eq!(t.pump(), CANCEL);
    assert_eq!(t.status.unwrap().state, 5);
}

/// Run explicitly with CSHELL_LRZSZ_TEST set to a Linux lrzsz source build.
/// This tests the production adapter against C lrz/sz, not another Rust peer.
#[cfg(windows)]
#[test]
#[ignore = "requires local WSL Ubuntu and CSHELL_LRZSZ_TEST"]
fn original_linux_lrzsz_upload_and_download_interoperate() {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    fn linux(path: &Path) -> String {
        let path = path
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let path = path.strip_prefix("//?/").unwrap_or(&path);
        format!("/mnt/{}/{}", path[..1].to_ascii_lowercase(), &path[3..])
    }
    let binaries =
        std::env::var("CSHELL_LRZSZ_TEST").expect("set CSHELL_LRZSZ_TEST to lrzsz src directory");
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/zmodem-interop");
    let root = tempfile::tempdir_in(fixture_root).unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&destination).unwrap();
    let payload: Vec<_> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
    let files = [source.join("binary.bin"), source.join("empty.txt")];
    std::fs::write(&files[0], &payload).unwrap();
    std::fs::write(&files[1], b"").unwrap();
    for download in [false, true] {
        let mut command = Command::new("wsl.exe");
        command.args([
            "-d",
            "Ubuntu",
            "--cd",
            &linux(&destination),
            "--exec",
            &format!("{binaries}/{}", if download { "lsz" } else { "lrz" }),
            "-b",
        ]);
        if download {
            for file in &files {
                command.arg(linux(file));
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut bytes = [0; 16384];
            loop {
                match output.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if send.send(bytes[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut transfer = Transfer::default();
        let started = Instant::now();
        let mut exited = None;
        let local_download = root.path().join("download");
        std::fs::create_dir_all(&local_download).unwrap();
        while started.elapsed() < Duration::from_secs(40) {
            if let Ok(bytes) = receive.recv_timeout(Duration::from_millis(5)) {
                transfer.incoming(&bytes);
            }
            if let Some(status) = &transfer.status
                && status.state == 1
            {
                transfer
                    .control(&ZmodemRequest {
                        operation: status.direction,
                        transfer_id: status.transfer_id,
                        paths: if download {
                            vec![local_download.to_string_lossy().into()]
                        } else {
                            files
                                .iter()
                                .map(|p| p.canonicalize().unwrap().to_string_lossy().into())
                                .collect()
                        },
                        ..Default::default()
                    })
                    .unwrap();
            }
            let wire = transfer.pump();
            if !wire.is_empty() {
                if input.write_all(&wire).and_then(|_| input.flush()).is_err() {
                    break;
                }
            }
            if let Some(exit) = child.try_wait().unwrap() {
                exited = Some(exit);
                break;
            }
            if transfer.status.as_ref().is_some_and(|s| s.state >= 4) {
                break;
            }
        }
        if exited.is_none() {
            let _ = child.kill();
            exited = Some(child.wait().unwrap());
        }
        drop(input);
        thread.join().unwrap();
        assert!(
            exited.unwrap().success(),
            "lrzsz {} failed: {:?}",
            if download { "sz" } else { "rz" },
            transfer.status
        );
        assert_eq!(
            transfer.status.as_ref().unwrap().state,
            3,
            "{:?}",
            transfer.status
        );
        let received = if download {
            &local_download
        } else {
            &destination
        };
        assert_eq!(std::fs::read(received.join("binary.bin")).unwrap(), payload);
        assert_eq!(std::fs::read(received.join("empty.txt")).unwrap(), b"");
        assert_eq!(transfer.status.as_ref().unwrap().files_completed, 2);
    }
}
