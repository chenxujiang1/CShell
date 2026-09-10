# ADR-0004：终端核心采用 alacritty_terminal 适配层

- 状态：Accepted
- 日期：2026-08-31
- 部分替代：[ADR-0002](0002-pure-rust-gui-stack.md) 第4、6项中的 `wezterm-term` 选择
- 部分替代：[ADR-0003](0003-desktop-platform-local-terminal-sync-input.md) 第4项中的 `wezterm-term` 选择

## 背景

原方案选择 `wezterm-term`，因为它覆盖完整VT状态、滚动缓冲、超链接、图片和输入编码。但 Phase 0 实测确认该crate仍未发布到 crates.io，只能从WezTerm单体仓库按Git commit引用。精确commit探针需要拉取整个上游workspace，解析时间、传递依赖面、构建成本和长期升级边界都超出终端状态适配层应承担的范围。

CShell需要一个可锁定发布版本、三平台可构建、具备成熟VT/grid/scrollback/selection/search/mode模型，并能把DSR等终端响应交还transport的核心。终端渲染、冷日志、主题、高亮和批量输出本来就由项目自有模块负责，不要求核心库提供GUI。

Phase 0 对 `alacritty_terminal 0.26.0` 完成了编译与最小适配验证：ANSI色、True Color、项目自有cell快照、resize和 `CSI 6 n` 产生的PTY response均通过。Windows ConPTY冒烟也证明host response必须回到有序输入通道，否则短命令会停在终端查询阶段。

## 决策

1. 产品终端核心默认采用 crates.io 的 `alacritty_terminal = 0.26.0`，精确版本和传递依赖写入 `Cargo.lock`。
2. 第三方类型只能存在于 `cshell-terminal::alacritty` 适配模块；daemon、IPC、renderer和持久格式只消费项目自有 `FrameSnapshot`、`FrameDelta`、`Cell`、`Style` 和 `InputAction`。
3. `Event::PtyWrite` 等host response必须作为有序、高优先级输入返回对应SSH/PTY transport；禁止当成普通屏幕输出或丢弃。
4. xterm/modifyOtherKeys/Kitty等键盘编码由项目自有 `InputEncoder` 根据终端mode实现并用语料测试，不把前端按键直接编码后广播到多个终端。
5. 冷滚动、原始journal、10～100 GB日志和高亮仍由CShell实现；只从终端核心提取可见屏幕和有界hot scrollback。
6. Sixel/iTerm2/Kitty图片协议继续不阻塞v0.1。后续通过独立、限资源的image provider增加，不因此退回未发布的Git依赖。
7. `ProbeTerminalEngine`只保留为故障注入和接口测试double，不得成为产品默认终端核心。

## 验收要求

- 将Alacritty官方ref/vttest语料转换为项目差分测试，至少覆盖主/备用屏、滚动区、宽字符、组合字符、OSC 8、256色、True Color和输入mode。
- Windows ConPTY、Linux/macOS PTY和SSH PTY都必须验证host response闭环。
- 终端核心升级必须单独提交，附语料差分、性能、许可证和API变更报告。
- 若P0语料或性能硬门槛失败，必须新增ADR选择替代Provider，不能让Alacritty类型泄漏到上层以规避替换。

## 后果

正面影响：

- 使用已发布、可锁定、MSRV低于项目基线的终端crate，默认构建不依赖大型Git workspace。
- `Term`、Grid、scrollback、selection、search、mode与ref tests可直接支撑SSH客户端常规终端需求。
- Apache-2.0许可证与项目MIT/Apache-2.0策略兼容。

代价与风险：

- 键盘编码不作为稳定高层API完整提供，CShell必须维护并测试自己的 `InputEncoder`。
- 图片协议能力弱于WezTerm，后续需要独立扩展。
- 当前适配器首版为full visible snapshot；dirty-row精确提取和hot/cold scrollback迁移仍是P0工作。

## 依据

- [alacritty_terminal 0.26.0](https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/)
- [Alacritty terminal changelog](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/CHANGELOG.md)
- [Alacritty reference tests](https://github.com/alacritty/alacritty/tree/master/alacritty_terminal/tests/ref)
- [wezterm-term README](https://github.com/wezterm/wezterm/blob/main/term/README.md)
- [wezterm-term发布议题](https://github.com/wezterm/wezterm/issues/6663)
