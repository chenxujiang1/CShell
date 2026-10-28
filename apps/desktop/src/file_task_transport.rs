//! Authenticated control only; file bytes stay with the daemon transfer workers.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_ipc::{
    Envelope, Handshake, TaskOperation, TaskRequest, TaskResponse, TaskStatus, client_handshake,
    envelope, features, read_envelope, transport, write_envelope,
};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(crate) enum FileTaskKind {
    Upload,
    Download,
    Mixed,
}
impl FileTaskKind {
    fn accepts(self, operation: TaskOperation) -> bool {
        match self {
            Self::Upload => operation.is_upload(),
            Self::Download => operation.is_download(),
            Self::Mixed => operation.is_mixed(),
        }
    }
    fn feature(self) -> u64 {
        match self {
            Self::Upload => features::TASK_UPLOADS,
            Self::Download => features::TASK_DOWNLOADS,
            Self::Mixed => features::MIXED_TASK_CONTROL,
        }
    }
    fn creates(self, operation: TaskOperation) -> bool {
        match self {
            Self::Upload => operation == TaskOperation::UploadCreate,
            Self::Download => operation == TaskOperation::DownloadCreate,
            Self::Mixed => matches!(
                operation,
                TaskOperation::MixedCreate | TaskOperation::MixedRetry
            ),
        }
    }
    fn lists(self, operation: TaskOperation) -> bool {
        match self {
            Self::Upload => operation == TaskOperation::UploadList,
            Self::Download => operation == TaskOperation::DownloadList,
            Self::Mixed => operation == TaskOperation::MixedList,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Upload => "上传",
            Self::Download => "下载",
            Self::Mixed => "混合任务",
        }
    }
}
pub(crate) async fn send(
    config: &DesktopConnectionConfig,
    query: TaskRequest,
    kind: FileTaskKind,
) -> Result<TaskResponse, String> {
    let name = kind.name();
    let operation =
        TaskOperation::try_from(query.operation).map_err(|_| format!("{name}操作无效"))?;
    if !kind.accepts(operation) {
        return Err(format!("{name}操作类型不一致"));
    }
    let expected = query.run_id.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        let endpoint = config.resolve().map_err(|_| "无法发现 daemon")?;
        let mut stream = transport::connect(&endpoint.endpoint)
            .await
            .map_err(|_| "无法连接 daemon")?;
        let mut handshake = Handshake::new(
            endpoint.daemon_instance_id.to_vec(),
            endpoint.instance_token.to_vec(),
        );
        let required = features::TASK_CONTROL
            | kind.feature()
            | if operation.is_mixed_repair() {
                features::MIXED_FILE_REPAIR
            } else {
                0
            }
            | if operation.is_mixed_results() {
                features::MIXED_RESULTS
            } else {
                0
            };
        let required = required
            | if operation == TaskOperation::MixedRetry {
                features::MIXED_TASK_RETRY
            } else {
                0
            };
        let rollout = matches!(
            operation,
            TaskOperation::MixedRolloutGet | TaskOperation::MixedRolloutApprove
        ) || operation == TaskOperation::MixedCreate
            && serde_json::from_slice::<serde_json::Value>(&query.definition_json)
                .ok()
                .is_some_and(|definition| {
                    definition
                        .get("policy")
                        .and_then(|policy| policy.get("rollout"))
                        .is_some_and(|value| !value.is_null())
                })
            || operation == TaskOperation::MixedStart
                && serde_json::from_slice::<serde_json::Value>(&query.expected_targets_json)
                    .ok()
                    .is_some_and(|confirmation| {
                        confirmation
                            .get("control_revision")
                            .is_some_and(|value| !value.is_null())
                    });
        let required = required | if rollout { features::MIXED_ROLLOUT } else { 0 };
        let required = required
            | if cshell_application::definition_uses_file_publication(&query.definition_json) {
                features::FILE_PUBLICATION
            } else {
                0
            };
        handshake.feature_bits = required | features::FILE_PUBLICATION;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon 身份或协议验证失败")?;
        if negotiated.feature_bits & required != required {
            return Err(format!("当前 daemon 不支持多主机{name}"));
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TaskRequest(query)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| format!("{name}请求发送中断，结果可能不明"))?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| format!("{name}回复中断，结果可能不明"))?;
        if reply.request_id != 2 {
            return Err(format!("{name}回复编号不一致"));
        }
        let Some(envelope::Payload::TaskResponse(reply)) = reply.payload else {
            return Err(format!("{name}回复类型不一致"));
        };
        if reply.status != TaskStatus::Ok as i32 {
            let message = match TaskStatus::try_from(reply.status) {
                Ok(TaskStatus::Conflict) => "状态变化或运行者正在处理，请重新载入",
                Ok(TaskStatus::Unsupported) => "daemon 不支持此操作",
                Ok(TaskStatus::Missing) => "运行不存在",
                Ok(TaskStatus::Invalid) => "请求无效或恢复条件不满足",
                Ok(TaskStatus::Corrupt) => "保存状态无效",
                Ok(TaskStatus::Limit) => "请求或状态超过限额",
                _ => "服务不可用，结果可能不明",
            };
            return Err(format!("{name}{message}"));
        }
        if !kind.creates(operation) && !kind.lists(operation) && reply.run_id != expected
            || operation != TaskOperation::MixedOutput
                && (reply.revision != 0 || !reply.output.is_empty() || reply.output_length != 0)
        {
            return Err(format!("{name}回复身份或字段不一致"));
        }
        Ok(reply)
    })
    .await
    .map_err(|_| format!("{name}请求超时，结果可能不明"))?
}
