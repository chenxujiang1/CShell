use super::*;
#[tokio::test]
async fn shared_ssh_create_sends_only_source_and_rejects_original_or_unverified_replies() {
    for valid in [true, false] {
        let (mut client, mut server) = tokio::io::duplex(65536);
        let source = SessionId::new();
        let created = SessionId::new();
        let profile = ProfileId::new();
        let job = tokio::spawn(async move {
            let request = read_envelope(&mut server).await.unwrap();
            let Some(envelope::Payload::SessionCreateRequest(body)) = request.payload else {
                panic!()
            };
            assert_eq!(
                body.source_ssh_session_id.as_deref(),
                Some(source.as_uuid().as_bytes().as_slice())
            );
            assert!(body.profile_id.is_none() && body.local_launch.is_none());
            assert_eq!((body.rows, body.cols), (30, 90));
            write_envelope(
                &mut server,
                &Envelope {
                    request_id: request.request_id,
                    payload: Some(envelope::Payload::SessionCreateResponse(
                        cshell_ipc::SessionCreateResponse {
                            session: Some(cshell_ipc::SessionSummary {
                                session_id: if valid { created } else { source }
                                    .as_uuid()
                                    .as_bytes()
                                    .to_vec(),
                                profile_id: Some(profile.as_uuid().as_bytes().to_vec()),
                                title: "SSH host".into(),
                                running: true,
                                can_duplicate_ssh_channel: true,
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        });
        let mut request = 11;
        let result = create_ssh_channel_session(
            &mut client,
            &mut request,
            source,
            Some(profile),
            TerminalSize::cells(30, 90),
        )
        .await;
        if valid {
            assert_eq!(result.unwrap().0, created);
        } else {
            assert!(matches!(
                result,
                Err(DesktopConnectionError::SessionCreationUncertain)
            ));
        }
        assert_eq!(request, 12);
        job.await.unwrap();
    }
}
