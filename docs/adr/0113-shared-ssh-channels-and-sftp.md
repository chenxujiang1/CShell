# SSH 渠道复制与 SFTP 复用认证

参考 Xshell 的独立会话和复制 SSH 渠道：前者以保存的配置建立新的网络连接并重新认证；后者仅在已认证的 SSH transport 上打开新的 PTY / shell。新渠道使用独立终端解析器、输入队列、历史、日志和剪贴板请求，不镜像原终端，不重放命令或动态码。入口同时位于“会话”菜单和标签右键菜单；本地终端继续使用原有“复制为新会话”。复制的 SSH 渠道标题和状态提示说明连接复用。

IPC minor 升到 63。SessionCreateRequest 可提供 source_ssh_session_id，与 profile_id 和 local_launch 互斥。新渠道必须引用同一后台实例里仍可用的 SSH 会话，并继承已验证的身份、终端类型和日志设置。新渠道返回独立 SessionId，客户端拒绝原 ID、错误配置和未验证的回复。客户端在旧后台上拒绝发送这类请求，结果不明不自动重发。通道打开最多等待 15 秒，服务端拒绝多渠道时明确失败，不退回新的认证。

通过显式 transport lease 管理终端和文件视图，每个新消费者 fork 一份独立 lease，每个连接最多 64 份。内部状态快照的 clone 只是同一 lease 的引用。释放一个消费者只关闭它的 PTY，最后一个释放后才关闭网络连接及跳板链。关闭请求或构造 future 取消后仍执行分离的资源清理；PTY / shell 创建失败也关闭自身渠道，不关闭其他消费者。

SSH 的 SFTP 工具栏优先当前终端主机，找不到当前 SSH 配置时才使用侧栏选中配置。明确从当前 SSH 打开的 SFTP 提交 source_ssh_session，后端按该会话的配置 ID、已验证身份和活跃 lease 校验；不以地址相似或空用户名代替身份比较。SFTP 使用独立视图 ID、队列和子系统渠道，界面显示“复用 SSH”。未指定来源时保留独立 SFTP 认证，兼容旧的 ProfileId 请求格式。关闭终端后，已打开的 SFTP 仍持有 lease；关闭 SFTP 也不关闭终端。传输、目录浏览和动态码不进入共享终端的输入输出。

共享渠道依赖同一 SSH 网络连接，实际网络断开会影响该连接上的所有渠道。现有原会话可按配置执行有界自动重连；复制出的渠道不额外启动并行认证，避免同一网络故障同时弹出多个动态码框。独立连接之间保持隔离。SFTP 重连时有可用的当前 SSH 则继续复用，否则按服务器要求重新认证。

验证使用临时配置 / Vault、经过握手的 IPC 和真实本机 MFA SSH / SFTP 服务，统计 TCP、验证码与密码次数，检查 Shell 隔离、两类复制、独立关闭、原终端退出后 SFTP 存活、渠道拒绝、无效来源、64 消费者上限和共同断线。官方参考：
- https://cdn.netsarang.net/docs/Xshell4_manual.pdf （Session Tab）
- https://netsarang.atlassian.net/wiki/spaces/ENFA/pages/1311808620/Unable+to+open+multiple+ssh+instances+1
