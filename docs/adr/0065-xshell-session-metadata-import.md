# ADR-0065：Xshell 单会话元数据导入

- 日期：2026-10-14
- 状态：代码接入；编译、测试、真实导出和原生 GUI 验证统一后续完成
- 范围：P3-050，接续 ADR-0062～0064

## 决策

独立解析用户明确选择的单个明文 `.xsh` INI，转换为既有 Profile v2 文档。
不扫描厂商目录，不读取注册表/关联默认文件，不启动 Xshell，不连接目标。
不接受 `.xts`/ZIP 包或受保护容器，不提取或解密源密码/口令，不解析厂商密钥。

当前子集只转换文件名（不含扩展名）、`CONNECTION` 中的 `Host`、`Port`、
`Protocol` 和 `CONNECTION:AUTHENTICATION` 中的 `UserName`。
Protocol 必须显式为 SSH（忽略 ASCII 大小写），Port 必须显式为 1～65535 的
十进制数字。没有端口不猜默认值。账号缺失/空值时使用用户明确填写的默认账号。
拒绝 Default/Default Session/Default Settings/Global 文件名。

不解释端点中的 URL、user@host、方括号或端口拼接；冒号只接受裸 IPv6。
新 UUID、`import:xshell` 标签、Direct/Password、Logging=false，无密钥、证书、
agent 身份或源 secret。TERM/仿真/外观/算法/主机信任与其他设置省略；新会话
采用 CShell 设置，需要先核对或绑定凭据。导入不启动或修改已有会话。
Replace 沿用既有 Profile 规则，不据此删除已绑定的 CShell 凭据。

## 解析与省略边界

- 原输入与解码文本各不超过 1 MiB；UTF-8（可带 BOM）或 UTF-16LE BOM，
  不猜 ANSI/系统代码页。解码文本、UTF-16 words 和每个字段值使用 Zeroizing。
- 最多 8192 行、每行 8192 字节、128 节、1024 字段，节/字段名最长128字节。
  ASCII 节/键大小写折叠后去重；重复节或重复字段整份拒绝，不取最后一个值。
- 接受 LF/CRLF、空行、整行 `;`/`#` 注释、`[section]` 和 `key=value`。
  值保持字面形式；不移除行尾注释，不展开百分号、变量、引号、转义或多行续接。
  独立 CR、控制字符、无节标量、非法头部/键和无等号记录拒绝。
- 包含 include/inherit/parent/defaultsession 的节或引用键保守拒绝，即使引用值为空。
  proxy/jump/tunnel/forward/script/command/sendstring/sendtext/expect/logon/
  autologin/environment/nopty/noshell/x11 字段族只允许空、0、false、no、none；
  其他值拒绝，包括未启用但仍保存的脚本/路由引用。
- 这是拒绝不支持行为的保守门，不实现厂商运行语义，也不声称识别所有版本的
  扩展字段。未知字段只省略并统计；复杂会话可能被拒绝。其他协议、目录继承、
  脚本/跳板/转发及完整设置迁移不在这个子集内。
- 摘要只含固定来源和字段分类计数，不显示来源密码、口令、路径或原始错误行。
  凭据、密钥引用、信任、终端和日志分别提示；分类计数允许重叠。

## 接入

应用层导出 `import_xshell_session`、选项、错误及封闭摘要。
`ForeignProfileImportSummary` 增加有独立 format 标记的 Xshell 变体；保持旧 PuTTY
v1 review 的无标签表示，反序列化仍使用封闭且不同的结构。apply 校验来源、
摘要范围、单个元数据 Profile 的形状、文档 SHA、目录版本和重算后的逐项动作。

桌面新增 Xshell 格式选项、默认账号、固定省略提示与一次性核对提交。后台转换
后清理源字节，只把冻结的 v2 文档用于预览/提交；沿用摘要、版本、输入绑定和
遗漏确认。改变格式/文件/账号/策略需要重新预览。

CLI 增加：

```text
cshell-cli foreign-import convert-xshell server.xsh new-profiles.json operator
cshell-cli foreign-import preview-xshell server.xsh new-review.json stop operator
cshell-cli foreign-import apply new-review.json --confirm
```

convert 离线；preview 只读目录、纯计划；apply 重读/重算并核对完整目录回执。
输出只写新文件，原 XSH 不发送 daemon 或保存进 review。不明结果不自动重放。
无协议字段、能力位、数据库或依赖版本变化，协议仍 minor54。

## 依据与统一后续验证

[NetSarang 官方 Connection 说明](https://netsarang.atlassian.net/wiki/spaces/ENSUP/pages/2237304168?atl_f=content-tree)
用于名称/文件名及协议、主机、端口语义。
[Xshell 官方手册](https://www.netsarang.com/docs/Xshell5_manual.pdf)
用于 `.xsh` 独立会话文件和手工复制边界。
[Rex 自身 parser_xsh 的公开 API/源码说明](https://docs.metasploit.com/api/Rex/Parser/NetSarang.html)
用于核对 INI 主机、端口和账号的节/键位置；仅阅读元数据结构，未复制其代码或
调用解密功能。该依据不是厂商发布的完整 XSH 格式规范，不能据此声称全版本兼容。
不使用论坛中关于 INI 继承机制的用户提案作为已存在的语法依据。

本次仅源码编辑与 cargo fmt。未运行编译、测试、厂商进程、密码/密钥解码或真实
导入。统一门禁仍需覆盖 UTF-8/UTF-16、大小写/重复/边界、单行值与注释、IPv6/
端口/账号、未知及行为字段、默认/继承、敏感字段省略、review 变更和旧 PuTTY
兼容、GUI 输入绑定/确认、CAS/不明回执及不同版本真实脱敏 XSH 样本。

MobaXterm、RSA PPK 和更广格式覆盖继续编码；P3-050 验收未关闭。
