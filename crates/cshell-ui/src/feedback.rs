//! Translate known failure categories; retain original details for diagnosis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    None,
    Reconnect,
    Unlock,
    InspectFiles,
    Refresh,
}
#[derive(Clone, Copy, Debug)]
pub struct Feedback {
    pub title: &'static str,
    pub guidance: &'static str,
    pub action: Action,
}
pub fn explain(detail: &str) -> Feedback {
    let text = detail.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    let (title, guidance, action) = if has(&[
        "未确认",
        "不确定",
        "不匹配",
        "unknown outcome",
        "unconfirmed",
        "mismatch",
    ]) {
        (
            "操作结果尚未确认",
            "先刷新状态核对结果，请勿重复提交。",
            Action::Refresh,
        )
    } else if has(&[
        "vault is locked",
        "vault locked",
        "凭据库已锁",
        "vault is manually locked",
    ]) {
        (
            "凭据库需要解锁",
            "解锁后再继续连接；已有连接不受影响。",
            Action::Unlock,
        )
    } else if has(&["hostkeychanged", "host key changed", "hostkeymismatch"]) {
        (
            "服务器主机密钥发生变化",
            "请核对服务器身份和指纹后再连接。",
            Action::None,
        )
    } else if has(&["hostkeyunknown", "unknown ssh host key"]) {
        (
            "需要确认服务器身份",
            "请在主机指纹弹框中核对并确认。",
            Action::None,
        )
    } else if has(&["permission denied", "权限不足", "无权限", "access denied"]) {
        (
            "没有访问权限",
            "请检查账号权限、目录权限或文件是否被占用。",
            Action::InspectFiles,
        )
    } else if has(&[
        "already exists",
        "destination exists",
        "同名",
        "已存在",
        "source changed",
        "文件已变化",
        "conflict",
    ]) {
        (
            "文件存在冲突或已被修改",
            "请先核对源文件和目标文件，再选择如何处理。",
            Action::InspectFiles,
        )
    } else if has(&[
        "authenticationfailed",
        "authentication failed",
        "密码错误",
        "认证失败",
    ]) {
        (
            "身份验证未通过",
            "请重新认证，并检查账号、密码或验证码。",
            Action::Reconnect,
        )
    } else if has(&[
        "timed out",
        "timeout",
        "连接超时",
        "已断开",
        "connection closed",
        "connection reset",
        "网络不可达",
    ]) {
        (
            "连接暂时不可用",
            "检查网络后重新连接；传输任务请先核对状态。",
            Action::Reconnect,
        )
    } else if has(&["capacity", "繁忙", "队列已满", "任务已满"]) {
        (
            "服务繁忙",
            "等待当前任务结束，再重试此操作。",
            Action::Refresh,
        )
    } else {
        ("操作提示", "", Action::None)
    };
    Feedback {
        title,
        guidance,
        action,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_outcome_never_suggests_blind_reconnect_or_replay() {
        assert_eq!(
            explain("timeout: write result unconfirmed").action,
            Action::Refresh
        );
        assert_eq!(explain("主机身份不匹配").action, Action::Refresh);
    }
    #[test]
    fn permission_and_vault_failures_offer_specific_recovery() {
        assert_eq!(
            explain("SFTP permission denied").action,
            Action::InspectFiles
        );
        assert_eq!(
            explain("CredentialUnavailable: Vault is locked").action,
            Action::Unlock
        );
        assert_eq!(
            explain("HostKeyUnknown: unknown SSH host key").action,
            Action::None
        );
    }
}
