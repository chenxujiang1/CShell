//! One-shot connection choices shared by terminal and native SFTP workers.
use cshell_domain::{JumpConnectionOverride, JumpReusePolicy, ProfileId, SessionId};
use cshell_ipc::{
    Envelope, JumpCandidatesRequest, JumpCandidatesResponse, JumpCandidatesStatus,
    client_handshake, envelope, read_envelope, transport, write_envelope,
};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
enum Answer {
    Connect(JumpConnectionOverride),
    Refresh,
    Cancel,
}
struct Prompt {
    id: SessionId,
    response: JumpCandidatesResponse,
    policy: JumpReusePolicy,
    selected: Option<usize>,
    reply: Option<tokio::sync::oneshot::Sender<Answer>>,
    owner: Option<(SessionId, Instant)>,
}
#[derive(Default)]
struct State {
    prompts: VecDeque<Prompt>,
    context: Option<egui::Context>,
}
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Mutex::default)
}
pub(crate) fn active() -> bool {
    !state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .prompts
        .is_empty()
}
struct Waiting(SessionId);
impl Drop for Waiting {
    fn drop(&mut self) {
        let mut state = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.prompts.retain(|prompt| prompt.id != self.0);
        if let Some(context) = &state.context {
            context.request_repaint();
        }
    }
}
fn label(policy: JumpReusePolicy) -> &'static str {
    match policy {
        JumpReusePolicy::AlwaysNew => "始终新建独立连接",
        JumpReusePolicy::PreferExisting => "优先复用已有连接",
        JumpReusePolicy::OnlyExisting => "仅使用已有连接",
    }
}
pub(crate) async fn choose(
    config: &crate::daemon_connection::DesktopConnectionConfig,
    profile: ProfileId,
    force: bool,
) -> Result<Option<JumpConnectionOverride>, String> {
    loop {
        let response = query(config, profile).await?;
        let Some(response) = response else {
            if force {
                return Err("请先成套更新 GUI 和后台以选择跳板连接".into());
            }
            return Ok(None);
        };
        if !response.is_jump {
            return Ok(None);
        }
        if !response.execution_supported {
            if force || response.saved_policy != 0 {
                return Err(response.detail);
            }
            return Ok(None);
        }
        let policy = cshell_ipc::decode_jump_reuse_policy(response.saved_policy)
            .map_err(|_| "跳板策略无效")?;
        if !force && (policy == JumpReusePolicy::AlwaysNew || response.candidates.len() <= 1) {
            if policy == JumpReusePolicy::AlwaysNew {
                return Ok(None);
            }
            let candidate = response
                .candidates
                .first()
                .and_then(|candidate| candidate.selection.clone());
            return cshell_ipc::JumpConnectionOverrideData {
                policy: Some(response.saved_policy),
                candidate,
            }
            .try_into()
            .map(Some)
            .map_err(|_| "跳板候选回复无效".into());
        }
        let (reply, received) = tokio::sync::oneshot::channel();
        let id = SessionId::new();
        let waiting = Waiting(id);
        {
            let mut state = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.prompts.len() >= 16 {
                return Err("待处理连接选项过多，请先完成现有选择".into());
            }
            state.prompts.push_back(Prompt {
                id,
                selected: (response.candidates.len() == 1).then_some(0),
                response,
                policy,
                reply: Some(reply),
                owner: None,
            });
            if let Some(context) = &state.context {
                context.request_repaint();
            }
        }
        let answer = tokio::time::timeout(Duration::from_secs(300), received).await;
        drop(waiting);
        match answer {
            Ok(Ok(Answer::Connect(choice))) => return Ok(Some(choice)),
            Ok(Ok(Answer::Refresh)) => continue,
            _ => return Err("跳板连接选择已取消或超时".into()),
        }
    }
}
async fn query(
    config: &crate::daemon_connection::DesktopConnectionConfig,
    profile: ProfileId,
) -> Result<Option<JumpCandidatesResponse>, String> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let endpoint = config.resolve().map_err(|_| "后台服务不可用")?;
        let mut stream = transport::connect(&endpoint.endpoint)
            .await
            .map_err(|_| "无法查询跳板连接")?;
        let mut handshake = cshell_ipc::Handshake::new(
            endpoint.daemon_instance_id.to_vec(),
            endpoint.instance_token.to_vec(),
        );
        handshake.feature_bits = cshell_ipc::features::SSH_PROFILE_SESSION;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "跳板查询握手失败")?;
        if negotiated.protocol_minor < cshell_ipc::JUMP_REUSE_EXECUTION_MINOR {
            return Ok(None);
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::JumpCandidatesRequest(
                    JumpCandidatesRequest {
                        target_profile_id: profile.as_uuid().as_bytes().to_vec(),
                    },
                )),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "跳板查询发送失败")?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "跳板查询失败")?;
        if reply.request_id != 2 {
            return Err("跳板查询回复不匹配".into());
        }
        let Some(envelope::Payload::JumpCandidatesResponse(response)) = reply.payload else {
            return Err("跳板查询回复格式无效".into());
        };
        if response.status != JumpCandidatesStatus::Ok as i32 {
            return Err(response.detail);
        }
        if response.candidates.len() > 64
            || response.candidates.iter().any(|candidate| {
                candidate.selection.clone().is_none_or(|selection| {
                    JumpConnectionOverride::try_from(cshell_ipc::JumpConnectionOverrideData {
                        policy: Some(2),
                        candidate: Some(selection),
                    })
                    .is_err()
                })
            })
        {
            return Err("跳板候选身份无效或数量超限".into());
        }
        Ok(Some(response))
    })
    .await
    .map_err(|_| "查询跳板连接超时".to_owned())?
}
pub(crate) fn draw(context: &egui::Context) {
    let owner = context.data_mut(|data| {
        *data.get_temp_mut_or_insert_with(egui::Id::new("jump-choice-context"), SessionId::new)
    });
    let mut state = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.context = Some(context.clone());
    let Some(prompt) = state.prompts.front_mut() else {
        return;
    };
    if prompt
        .owner
        .as_ref()
        .is_some_and(|(id, seen)| *id != owner && seen.elapsed() < Duration::from_secs(2))
    {
        return;
    }
    prompt.owner = Some((owner, Instant::now()));
    let answer = draw_prompt(context, prompt);
    if let Some(answer) = answer
        && let Some(reply) = prompt.reply.take()
    {
        let _ = reply.send(answer);
    }
    context.request_repaint_after(Duration::from_millis(200));
}

fn draw_prompt(context: &egui::Context, prompt: &mut Prompt) -> Option<Answer> {
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new("jump-connection-choice")).show(context, |ui| {
        ui.set_width(460.0_f32.min((ui.ctx().content_rect().width() - 60.0).max(220.0)));
        ui.heading("本次跳板连接");
        ui.label("仅影响本次连接；关闭一个标签不会关闭其他会话正在使用的跳板。");
        for policy in [
            JumpReusePolicy::PreferExisting,
            JumpReusePolicy::AlwaysNew,
            JumpReusePolicy::OnlyExisting,
        ] {
            ui.radio_value(&mut prompt.policy, policy, label(policy));
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .show(ui, |ui| {
                for (index, candidate) in prompt.response.candidates.iter().enumerate() {
                    let id = candidate
                        .selection
                        .as_ref()
                        .map(|c| {
                            c.transport_id
                                .iter()
                                .rev()
                                .take(6)
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    ui.add_enabled_ui(prompt.policy != JumpReusePolicy::AlwaysNew, |ui| {
                        ui.radio_value(
                            &mut prompt.selected,
                            Some(index),
                            format!(
                                "{}@{}:{} · 连接 {} ({})",
                                candidate.username,
                                candidate.host,
                                candidate.port,
                                candidate
                                    .selection
                                    .as_ref()
                                    .map_or(0, |selection| selection.generation),
                                id
                            ),
                        );
                        ui.weak(format!(
                            "{} · {} 个使用者",
                            candidate.source_title, candidate.consumers
                        ))
                        .on_hover_text(&candidate.host_key_sha256);
                    });
                }
            });
        if prompt.response.candidates.is_empty() {
            ui.label("当前没有可复用的跳板；优先复用将新建并按服务器要求认证。");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let selected_valid = prompt
            .selected
            .and_then(|index| prompt.response.candidates.get(index))
            .is_some_and(|candidate| candidate.expires_unix_seconds > now);
        if prompt.selected.is_some()
            && !selected_valid
            && prompt.policy != JumpReusePolicy::AlwaysNew
        {
            ui.label("候选已过期，请刷新后重新选择。");
        }
        let can_connect = prompt.policy == JumpReusePolicy::AlwaysNew
            || selected_valid
            || prompt.policy == JumpReusePolicy::PreferExisting
                && prompt.response.candidates.is_empty();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_connect, egui::Button::new("连接"))
                .clicked()
            {
                let candidate = if prompt.policy == JumpReusePolicy::AlwaysNew {
                    None
                } else {
                    prompt
                        .selected
                        .and_then(|i| prompt.response.candidates.get(i))
                        .and_then(|c| c.selection.clone())
                };
                let choice = cshell_ipc::JumpConnectionOverrideData {
                    policy: Some(cshell_ipc::encode_jump_reuse_policy(prompt.policy)),
                    candidate,
                };
                if let Ok(choice) = choice.try_into() {
                    answer = Some(Answer::Connect(choice));
                }
            }
            if ui.button("刷新候选").clicked() {
                answer = Some(Answer::Refresh);
            }
            if ui.button("取消").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if modal.should_close() {
        answer = Some(Answer::Cancel);
    }
    answer
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn prompt(count: usize) -> Prompt {
        Prompt {
            id: SessionId::new(),
            policy: JumpReusePolicy::OnlyExisting,
            selected: None,
            reply: None,
            owner: None,
            response: JumpCandidatesResponse {
                execution_supported: true,
                is_jump: true,
                saved_policy: 2,
                candidates: (0..count)
                    .map(|i| cshell_ipc::JumpCandidateData {
                        selection: Some(cshell_ipc::JumpCandidateSelectionData {
                            transport_id: SessionId::new().as_uuid().as_bytes().to_vec(),
                            generation: i as u64 + 1,
                            token: vec![45; 32],
                        }),
                        username: "operator".into(),
                        host: "host-long-name.example.test".repeat(5),
                        port: 22,
                        source_title: "生产环境跳板".into(),
                        consumers: 2,
                        expires_unix_seconds: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            + 60,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
        }
    }
    fn render(
        context: &egui::Context,
        prompt: &mut Prompt,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> (egui::accesskit::TreeUpdate, Option<Answer>) {
        let mut answer = None;
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ui| answer = draw_prompt(ui.ctx(), prompt),
        );
        output.textures_delta.clear();
        (output.platform_output.accesskit_update.unwrap(), answer)
    }
    fn click(
        context: &egui::Context,
        prompt: &mut Prompt,
        label: impl Fn(&str) -> bool,
    ) -> Option<Answer> {
        let size = egui::vec2(960.0, 640.0);
        let (tree, _) = render(context, prompt, size, vec![]);
        let pos = crate::ui_layout_gate::bounds(
            &tree
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(&label))
                .unwrap()
                .1,
        )
        .center();
        render(
            context,
            prompt,
            size,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        render(
            context,
            prompt,
            size,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        )
        .1
    }
    #[test]
    fn jump_choice_actions_fit_small_windows_with_long_names() {
        for size in [egui::vec2(640.0, 480.0), egui::vec2(960.0, 640.0)] {
            let context = crate::ui_layout_gate::context();
            context.all_styles_mut(|style| style.animation_time = 0.0);
            let mut prompt = prompt(8);
            for _ in 0..3 {
                render(&context, &mut prompt, size, vec![]);
            }
            let (tree, _) = render(&context, &mut prompt, size, vec![]);
            let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for title in [
                "连接",
                "刷新候选",
                "取消",
                label(JumpReusePolicy::AlwaysNew),
                label(JumpReusePolicy::OnlyExisting),
            ] {
                let node = &tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(title))
                    .unwrap()
                    .1;
                assert!(
                    viewport.contains_rect(crate::ui_layout_gate::bounds(node)),
                    "{title}"
                );
            }
        }
    }
    #[test]
    fn only_existing_cannot_connect_without_selection_and_second_candidate_is_exact() {
        let context = crate::ui_layout_gate::context();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        let mut prompt = prompt(2);
        for _ in 0..3 {
            render(&context, &mut prompt, egui::vec2(960.0, 640.0), vec![]);
        }
        let (tree, _) = render(&context, &mut prompt, egui::vec2(960.0, 640.0), vec![]);
        assert!(
            tree.nodes
                .iter()
                .find(|(_, node)| node.label() == Some("连接"))
                .unwrap()
                .1
                .is_disabled()
        );
        click(&context, &mut prompt, |label| label.contains("连接 2 ("));
        let expected = prompt.response.candidates[1]
            .selection
            .as_ref()
            .unwrap()
            .transport_id
            .clone();
        let Some(Answer::Connect(choice)) = click(&context, &mut prompt, |label| label == "连接")
        else {
            panic!("connect")
        };
        assert_eq!(
            choice
                .candidate
                .unwrap()
                .transport_id
                .as_uuid()
                .as_bytes()
                .to_vec(),
            expected
        );
    }
    #[test]
    fn explicit_new_connection_contains_no_candidate_and_escape_cancels() {
        let context = crate::ui_layout_gate::context();
        context.all_styles_mut(|style| style.animation_time = 0.0);
        let mut prompt = prompt(0);
        for _ in 0..3 {
            render(&context, &mut prompt, egui::vec2(960.0, 640.0), vec![]);
        }
        click(&context, &mut prompt, |text| {
            text == label(JumpReusePolicy::AlwaysNew)
        });
        let Some(Answer::Connect(choice)) = click(&context, &mut prompt, |text| text == "连接")
        else {
            panic!("new connection")
        };
        assert_eq!(choice.policy, Some(JumpReusePolicy::AlwaysNew));
        assert!(choice.candidate.is_none());
        let (_, answer) = render(
            &context,
            &mut prompt,
            egui::vec2(960.0, 640.0),
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(matches!(answer, Some(Answer::Cancel)));
    }
}
