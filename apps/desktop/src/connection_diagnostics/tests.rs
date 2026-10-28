use super::*;
use crate::ui_layout_gate::{bounds, context};
use cshell_domain::{ConnectionStep, SessionId};

pub(super) fn sample(outcome: Outcome) -> View {
    let stages = if outcome == Outcome::Succeeded {
        vec![
            Stage::Preparing,
            Stage::ReusingTransport,
            Stage::SftpChannel,
            Stage::SftpSubsystem,
            Stage::InitialDirectory,
            Stage::Ready,
        ]
    } else {
        vec![
            Stage::Preparing,
            Stage::Credentials,
            Stage::Resolving,
            Stage::Tcp,
            Stage::Handshake,
            Stage::HostKey,
            Stage::Authentication,
            Stage::WaitingForInput,
        ]
    };
    let count = stages.len();
    View {
        received: Instant::now(),
        diagnostic: Arc::new(ConnectionDiagnostic {
            attempt: SessionId::new(),
            elapsed_ms: count as u64 * 1000,
            outcome,
            failure_code: 0,
            truncated: false,
            steps: stages
                .into_iter()
                .enumerate()
                .map(|(i, stage)| ConnectionStep {
                    stage,
                    hop: if i < 4 { 1 } else { 0 },
                    endpoint: "linux.internal.example:22".into(),
                    started_ms: i as u64 * 1000,
                    elapsed_ms: 1000,
                    outcome: if i + 1 == count {
                        outcome
                    } else {
                        Outcome::Succeeded
                    },
                })
                .collect(),
        }),
    }
}
fn frame(
    ctx: &egui::Context,
    view: &View,
    size: egui::Vec2,
    events: Vec<egui::Event>,
) -> (egui::accesskit::TreeUpdate, bool) {
    let mut retry = false;
    let mut out = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..Default::default()
        },
        |ui| {
            retry = draw(
                ui,
                Some(view),
                view.diagnostic.outcome == Outcome::Running,
                true,
            );
        },
    );
    out.textures_delta.clear();
    (out.platform_output.accesskit_update.unwrap(), retry)
}
#[test]
fn small_window_keeps_retry_in_bounds_and_uncertain_outcomes_cannot_replay() {
    for size in [egui::vec2(720.0, 480.0), egui::vec2(1000.0, 700.0)] {
        let ctx = context();
        let view = sample(Outcome::Failed);
        assert!(view.diagnostic.validate());
        let (tree, retry) = frame(&ctx, &view, size, vec![]);
        assert!(!retry);
        let button = &tree
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some("重试连接"))
            .unwrap()
            .1;
        let rect = bounds(button);
        assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(rect));
        let pos = rect.center();
        let events = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        assert!(frame(&ctx, &view, size, events).1);
        let (tree, retry) = frame(&context(), &sample(Outcome::Uncertain), size, vec![]);
        assert!(!retry);
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, n)| n.label() == Some("重试连接"))
        );
        let (tree, _) = frame(&context(), &sample(Outcome::Succeeded), size, vec![]);
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, n)| n.label().is_some_and(|s| s.contains("连接已就绪")))
        );
    }
}
#[tokio::test]
async fn streamed_progress_is_visible_before_final_and_rejects_foreign_or_regressing_attempts() {
    let (mut client, mut server) = tokio::io::duplex(2048);
    let trace = sample(Outcome::Succeeded).diagnostic;
    let expected = trace.attempt;
    let state = Arc::new(std::sync::Mutex::new(None));
    let capture = state.clone();
    let writer = tokio::spawn(async move {
        cshell_ipc::write_envelope(
            &mut server,
            &Envelope {
                request_id: 7,
                payload: Some(envelope::Payload::ConnectionDiagnosticUpdate(
                    cshell_ipc::ConnectionDiagnosticUpdate {
                        document_json: serde_json::to_vec(&*trace).unwrap(),
                    },
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        cshell_ipc::write_envelope(
            &mut server,
            &Envelope {
                request_id: 7,
                payload: Some(envelope::Payload::SessionCreateResponse(Default::default())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    });
    let result = observe(
        true,
        move |v| *capture.lock().unwrap() = Some(v),
        read_response(&mut client, 7),
    )
    .await
    .unwrap();
    writer.await.unwrap();
    assert!(matches!(
        result.payload,
        Some(envelope::Payload::SessionCreateResponse(_))
    ));
    assert_eq!(
        state.lock().unwrap().as_ref().unwrap().diagnostic.attempt,
        expected
    );
    for foreign in [true, false] {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let trace = sample(Outcome::Running).diagnostic;
        let mut next = (*trace).clone();
        if foreign {
            next.attempt = SessionId::new();
        } else {
            next.elapsed_ms = 0;
        }
        let writer = tokio::spawn(async move {
            for t in [&*trace, &next] {
                let _ = cshell_ipc::write_envelope(
                    &mut server,
                    &Envelope {
                        request_id: 9,
                        payload: Some(envelope::Payload::ConnectionDiagnosticUpdate(
                            cshell_ipc::ConnectionDiagnosticUpdate {
                                document_json: serde_json::to_vec(t).unwrap(),
                            },
                        )),
                        ..Default::default()
                    },
                )
                .await;
            }
        });
        assert!(
            observe(true, |_| {}, read_response(&mut client, 9))
                .await
                .is_err()
        );
        writer.await.unwrap();
    }
}
#[test]
fn transport_loss_freezes_elapsed_and_marks_result_uncertain() {
    for outcome in [Outcome::Running, Outcome::Succeeded] {
        let mut trace = Some(sample(outcome));
        uncertain(&mut trace);
        let trace = trace.unwrap();
        assert_eq!(trace.diagnostic.outcome, Outcome::Uncertain);
        assert_eq!(
            trace.diagnostic.steps.last().unwrap().outcome,
            Outcome::Uncertain
        );
        assert!(trace.diagnostic.validate());
    }
}

#[test]
fn success_collapses_the_same_attempt_and_failure_summary_names_its_stage() {
    let ctx = context();
    let running = sample(Outcome::Running);
    let mut heights = vec![];
    let mut succeeded = sample(Outcome::Succeeded);
    Arc::make_mut(&mut succeeded.diagnostic).attempt = running.diagnostic.attempt;
    for value in [&running, &succeeded] {
        let mut height = 0.0;
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 480.0),
                )),
                ..Default::default()
            },
            |ui| {
                let top = ui.cursor().min.y;
                draw(
                    ui,
                    Some(value),
                    value.diagnostic.outcome == Outcome::Running,
                    true,
                );
                height = ui.cursor().min.y - top;
            },
        );
        out.textures_delta.clear();
        heights.push(height);
    }
    assert!(heights[0] > 100.0 && heights[1] < 55.0, "{heights:?}");
    let (tree, _) = frame(
        &context(),
        &sample(Outcome::Failed),
        egui::vec2(720.0, 480.0),
        vec![],
    );
    let texts=tree.nodes.iter().filter_map(|(_,n)|n.label().or(n.value())).collect::<Vec<_>>();
    assert!(texts.iter().any(|s|s.starts_with("连接失败 · 等待输入 / 解锁确认")),"{texts:?}");
}
