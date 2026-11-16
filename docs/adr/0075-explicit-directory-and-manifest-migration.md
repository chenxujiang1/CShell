# ADR-0075：显式目录与清单批量迁移

- 日期：2026-10-14
- 状态：编码接入；统一编译、测试、实际导出与原生平台验证待完成
- 范围：P3-050，接续 ADR-0062～0067

## 决策

新增 `cshell-import` 只读 IO 适配层；application 保留纯内存解析和批次汇总。
用户明确选择 SecureCRT Sessions 备份目录、Xshell 会话备份目录，或版本化
混合来源清单。目录相对父路径成为逻辑文件夹，文件名成为会话名；清单的
`folder` 是逻辑前缀，PuTTY/MobaXterm 的内部会话名及 MobaXterm 层级保留。
不扫描默认位置、不访问注册表、厂商进程或凭据文件，不执行厂商脚本。

## 输入契约

`cshell.foreign-source-manifest` v1，封闭字段：

```json
{"format":"cshell.foreign-source-manifest","version":1,"entries":[
  {"path":"securecrt/team/server.ini","format":"secure_crt_ini","folder":["Imported","Team"]},
  {"path":"xshell/server.xsh","format":"xshell_session","folder":["Imported"]},
  {"path":"putty.reg","format":"putty_registry"},
  {"path":"sessions.mxtsessions","format":"moba_xterm_unicode"}
]}
```

编码变体另有 `moba_xterm_windows1252`；不得根据解析失败猜编码。清单路径仅能
使用以 `/` 分隔的相对片段，不接受绝对路径、驱动器、反斜杠、空片段、`.`、
`..`、控制字符、Windows 设备名/流/通配符或首尾空白/尾点。重复及 Unicode
小写后冲突的路径拒绝。清单不包含默认账号或 secret；默认账号由 GUI/CLI 明示。

目录逐层稳定排序，仅接受 `.ini` 或 `.xsh`，只生成导入会话所需的祖先文件夹。
SecureCRT Default/Global/
__FolderData__，Xshell Default/Default Session/Default Settings/Global 文件不读，
计入跳过；无关普通文件不读并计数。清单显式列出默认/全局文件则拒绝。
单会话文件经原解析器结构检查后，非 SSH 协议可跳过；未知/不受支持的 SSH
行为、缺少协议、损坏文件或任何候选转换错误停止整批。没有支持的 SSH 则拒绝。
PuTTY/MobaXterm 文件沿用自身的跳过规则及无 SSH 拒绝规则。

## 预算与本机读取

- 4096 个枚举条目，512 个源文件，单文件/清单 1 MiB，累计原输入 8 MiB
  （包含清单）；最终转换文档仍至多 1 MiB、512 个 Profile、256 个文件夹、8层。
- 单批合作式30秒读取/解析期限，逐条目、逐文件、每16 KiB检查取消与预算。
  阻塞系统调用无法在用户态强制终止；GUI等待超时后由原阻塞owner持有双槽
  之一至实际退出，避免取消后无界积累后台读取者。
- Unix 从根开始逐组件 `openat`，O_NOFOLLOW/O_DIRECTORY/O_NONBLOCK，读取
  相对固定目录 FD。目录枚举使用命名路径并前后核对dev/ino；数据读取始终
  使用固定父 FD。Windows OPEN_REPARSE_POINT，目录/文件均不共享写入/删除，
  保留祖先句柄以阻止重命名及reparse写入；拒绝所有 reparse point。
- 文件前后长度/修改时间与再次打开核对，变化则拒绝。不是跨文件原子快照，
  不能证明恶意原地同长度写入且恢复时间戳未发生；建议选择关闭厂商应用后的
  稳定本机备份。解析预算不等于可中断网络/故障设备内核 IO 的硬时限。
- 原字节及分块缓冲 Zeroizing，源路径/原值/密文/源文件摘要不进入review。

## 批次身份与提交

批次摘要有独立格式标识，保留来源叶摘要、Profile/原文件夹 UUID归属和计数，
不嵌套批次；验证重建每份叶文档后复用原叶校验，检查所有Profile恰好归属一次、
原文件夹归属不重复、前缀链准确连接及覆盖全部文件夹、最终catalog、逻辑层级和总预算。逻辑前缀使用默认终端
覆盖；不合并冲突会话或源内文件夹。转换后文件夹/会话冲突让整批失败。

GUI新增三种入口，离开UI线程读取，源路径/格式/账号/策略变化清除核对；
汇总跳过与逐来源省略提示，复用遗漏确认、文档SHA和版本冻结、一次提交。
CLI新增 convert/preview-securecrt-dir、-xshell-dir、-manifest；复用review v1、
纯计划重算、catalog版本、完整回执与新文件发布。apply不重新读取源文件，
不明提交不自动重放。旧单文件与PuTTY review表示保持可读。

## 支持矩阵与范围

首批支持以格式形状定义：PuTTY Sessions .reg；SecureCRT明文S/D/Z单会话INI
及其目录；Xshell明文XSH及其目录；MobaXterm Bookmarks导出（显式Unicode/
Windows-1252及ADR-0066所列布局）；以及上述来源的显式清单。PPK v2/v3的
RSA/Ed25519/NIST ECDSA独立准入沿用ADR-0064/0067。不能以源码接入宣称某个
厂商全部版本兼容，实际版本矩阵仍须脱敏真实导出统一验证。

SecureCRT XML、Xshell XTS/加密压缩包、完整厂商配置、密码恢复和未知私有布局
本次不支持，不自动解包或回退。后续若将这些容器纳入产品范围，须有一手格式
依据和独立ADR/实现；不能将目录导入标成这些容器已支持。P3-050验收仍未关闭。
无数据库schema、daemon协议/能力位变化（保持minor55）；只增加内部crate和
已锁依赖边，不升级已有依赖版本。

## 一手依据与后续统一验证

[VanDyke Sessions遍历说明](https://www.vandyke.com/support/scripting/scripting-examples/interate-over-saved-sessions.html)
确认 Sessions 目录层级、INI及Default/__FolderData__排除；只参考文件布局，
不复制或运行厂商脚本。[VanDyke备份说明](https://www.vandyke.com/support/tips/backupsessions_72andearlier.html)
用于显式配置备份与关闭源应用的一致性边界；XML备份不据此推导私有字段。
XSH与已有四种叶格式的一手依据沿用ADR-0062/0063/0065/0066。
[Microsoft文件命名说明](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file)
用于保留设备名（含上标数字）、扩展名别名、尾点/空格及设备namespace拒绝边界；
另外保守拒绝控制台/旧设备名，不将名称交给设备IO。

统一验证覆盖不同平台路径/链接/reparse/重命名/权限/并发写入、FIFO/特殊文件、
字节/文件/深度/条目上限和超时取消占额、非SSH/损坏SSH区别、空与默认文件、
混合编码/多来源层级与冲突、UUID归属/篡改/旧review、GUI切换与一次提交、
CLI新文件发布、CAS及未知提交。当前仅源码/格式化/离线依赖图，不运行导入。
