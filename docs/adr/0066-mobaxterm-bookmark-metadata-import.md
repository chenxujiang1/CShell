# ADR-0066：MobaXterm 书签元数据导入

- 日期：2026-10-14
- 状态：源码接入；编译、测试、真实导出与原生 GUI 统一后续验证
- 范围：P3-050，接续 ADR-0062～0065

## 决策

新增独立、纯内存的 `import_mobaxterm_sessions`，读取用户选择的书签导出。
只转换受支持 SSH 记录的名称、主机、端口、账号与逻辑文件夹。复用既有 Profile
v2、冲突计划/事务、目录版本与 CAS；不创建新协议、能力位或数据库结构。

输入限定 `[Bookmarks]` / `[Bookmarks_N]`，首节必须是 Bookmarks，编号为正
十进制且不接受前导零。节名忽略 ASCII 大小写并去重。每节必须有且仅有一份
SubRep，ImgNum 可选；标量键/会话名按 Unicode 小写去重。完整 MobaXterm.ini、
无节单行 `.moba`、其他节、共享文件引用和未知扩展元数据拒绝。不会扫描配置
目录、访问厂商 registry/进程、读取关联文件或连接目标。

### 编码和预算

- 默认 Unicode：严格 UTF-8（可带 BOM）或带 BOM 的 UTF-16LE。
- 旧导出可显式选择 Windows-1252；不自动猜测/降级，拒绝该编码未定义字节和
  与所选编码不符的 Unicode BOM。其他系统代码页（包括 GBK）不在当前范围内。
- 原输入和解码后 UTF-8 各至多 1 MiB；8192 行、每行 16 KiB、128 节、512 个
  会话条目（含被跳过者）、256 个文件夹、层级最多8层。
- LF/CRLF、空行和整行分号注释可读；拒绝独立 CR、其他控制字符和续接语法。
  不展开变量、转义或厂商占位符，不截断行尾注释。
- 解码缓冲和 UTF-16 words 使用 Zeroizing；解析条目借用该缓冲，不额外复制
  原始值。只持久化生成的元数据，错误为固定类别/行号，不返回原始值。

### 有界布局与行为处理

记录的图标与会话类型分别读取，不能把图标109当成 SSH 的判断条件。限定6/7个
外层组；SSH 连接组只接受26、27、35字段，终端组只接受25～28字段的内置调色板
或40～43字段的16色 RGB 调色板。宏位置随已识别调色板宽度移动；不通过搜索
字符串猜字段位置，未知长度、保留位置或不一致布局拒绝。

合法外层容器中的非 SSH 类型整条跳过并计数，不启动其他协议。有任何候选 SSH
不满足布局、目标、账号或行为限制时整份转换失败，不写部分结果。没有 SSH 拒绝。

已识别的命令、跳板、代理、宏、非交互远程环境、locale 发送、SSH-1、X11/
agent 转发和“无认证即断开”策略均不转换。非空命令/宏/跳板/代理引用和活动开关
保守拒绝，包括未启用但仍保存的引用；仅允许没有跳板主机时的空/默认22端口及
没有代理时的惰性代理端口。源 X11 默认可能开启，需要用户关闭后重新导出。

连接组末尾的 SSH agent 偏好、算法偏好及源私钥引用只省略；不读取私钥、不提取
源凭据，也不沿用厂商认证顺序。空账号、`<default>` 或源“未指定账号”标志要求
显式默认账号；不读取本机 Windows 登录名。保守拒绝已知分隔/占位标记和 credential
引用形状，不尝试解析命名凭据。不能据此声称识别所有新版本的私有编码。

TERM、主题/字体/颜色、日志、文件浏览器、压缩、窗口显示、注释和图标均省略。
固定摘要显示字段总数、终端字段子计数、源密钥引用、注释、图标与非 SSH 跳过数；
不显示其原始内容或路径。新 Profile 使用新 UUID、`import:mobaxterm`、
Direct/Password/Logging=false，无私钥/证书/agent 身份引用。已有会话不变，打开
前需要检查或重新绑定 CShell 凭据与主机信任。Replace 不据此删除既有凭据。

### 文件夹与核对提交

SubRep 是逻辑路径，用反斜杠分段，逐级生成父文件夹 UUID；不作为本机路径访问。
空 SubRep 表示根；拒绝空片段、`.`/`..`、斜杠/冒号和片段首尾空白。相同路径的
公共祖先复用，同名但大小写不同的祖先拒绝；重复节路径拒绝，避免模糊合并。
保存前做完整 ProfileCatalog/文档验证；文件夹不携带任何终端覆盖。

ForeignProfileImportSummary 新增独立 format 的封闭 MobaXterm 变体，绑定编码、
计数与有界目录形状。只有该来源可携带文件夹；其他来源的原有范围保持。沿用
旧 PuTTY review v1 的无标签表示，反序列化后仍检查 source 与摘要变体一致。

桌面提供 Unicode / Windows-1252 两个明确格式选项，编码属于已有 source.format
绑定，变更会使旧预览/确认失效。后台只缓存生成文档，清理原字节后再显示省略
提示；复用一次性文档 SHA/版本/遗漏确认/CommitReviewedImport。

CLI：

```text
cshell-cli foreign-import convert-mobaxterm sessions.mxtsessions new-profiles.json operator
cshell-cli foreign-import preview-mobaxterm sessions.mxtsessions new-review.json stop operator
cshell-cli foreign-import preview-mobaxterm legacy.mxtsessions new-review.json stop operator --windows-1252
cshell-cli foreign-import apply new-review.json --confirm
```

默认账号可省；缺省账号的会话仍必须提供该参数。编码标志仅在末尾接受，不根据
失败自动换编码。convert 离线；preview 只读目录并作纯计划；apply 重读目录/
重算逐项动作、SHA/版本与完整回执。输出必须新文件，review 不包含原始导出，
未知提交结果不自动重放。协议仍 minor54，无依赖/版本/schema升级。

## 公开依据与后续统一验证

[厂商文档](https://mobaxterm.mobatek.net/documentation.html)用于导出/INI书签与功能边界，
[厂商共享会话说明](https://blog.mobatek.net/post/configure-shared-sessions/)用于共享
导出是独立文件的边界。
[原作者对23.6记录的逐项观察](https://gist.github.com/Ruzgfpegk/ab597838e4abbe8de30d7224afd062ea/dc1d11e8604350a0d6d23db340c13b5d736807c1)
是布局位置的一手观察依据，属于非厂商格式记录，不能据此声称完整/全版本兼容；
没有复制其模板、图标或转换器代码，也没有运行厂商二进制。
[Unicode 发布的 CP1252 表](https://www.unicode.org/Public/MAPPINGS/VENDORS/MICSFT/WINDOWS/CP1252.TXT)
用于明确字符映射和未定义字节；未引入编码依赖。

本轮只编辑源码和 cargo fmt；没有编译、测试、实际导入、厂商运行或密钥解码。
后续统一验证需覆盖双编码/BOM/未定义字节、边界/重复/路径/深度、调色板宽度与
宏移位、不同连接长度、未知布局、惰性端口与活动行为、账号哨兵、非 SSH 跳过、
摘要/形状/旧review兼容、GUI格式变化/CAS/不明提交，以及真实脱敏导出。

四种厂商的已列元数据子集均已接入代码；RSA PPK、更广格式覆盖、P3-060脚本
和所有统一门禁仍待后续，P3-050 验收未关闭。
