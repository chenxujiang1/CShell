# ADR-0083：系统凭据认证与首次主机密钥引导

- 日期：2026-10-15

## 问题

GUI 的失焦处理无条件发送 Vault Lock；该操作会推进全局凭据 generation，
取消正在创建的 SSH 会话。因而使用系统凭据的连接在切换应用时也可能报
“Vault was locked during SSH creation”，即使没有加密 Vault。

## 修复

当已取得状态为 Ok、无加密 Vault、OS 会话状态 Active，且没有排队或正在
处理的 Vault 变更时，GUI 失焦不再发送自动 Lock。仍清空 GUI 临时秘密和
粘贴确认。明确锁定、GUI 退出、加密 Vault、状态未知/旧协议 Unsupported、
锁屏/Unavailable 等情况维持原行为。后台 OS 会话观察、generation 校验、
凭据读取和最终会话提交的保护均不改变。

Vault 变更从入队前同步计数，完成结果发布后才减少，覆盖创建/解锁尚未
返回状态的时间窗。创建尚未使 exists 变为 true 时，失焦仍发送 Lock，防止
工作线程随后发布可用的解锁密钥。

HostKeyUnknown 仍拒绝连接。GUI 打开本次连接配置的“主机密钥”页，提示
预览、从可信渠道核对 SHA256 指纹、手动确认保存、再重新连接。切换到本次
配置失败时清空其他配置编辑器，避免在错误目标上确认。不会自动预览或接受
服务器密钥，不改变 HostKeyChanged 的拒绝行为。

## 验证

- 失焦：原生凭据 + Active 不发 Lock；明确锁定依然发出；加密 Vault、
  Unsupported/Locked/Unavailable、正在创建 Vault 均发送 Lock。
- 未知主机密钥：从其他配置跳转至实际连接配置，打开指纹页且不自动发出
  Preview/Confirm 请求。
- 完整桌面及工作台测试：108 + 15 通过，3 项原有测试跳过。
- GUI 构建通过；后台无需替换，不停止用户会话。
