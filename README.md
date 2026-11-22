# lieterm

基于本地 `../zzzui` 实验性 UI 库的 PuTTY 式 SSH 终端。终端仿真用 `alacritty_terminal` 0.26，SSH 客户端用 `russh` 0.65.2，宿主密钥/认证均在独立的 tokio 运行时里处理，UI 线程只负责渲染与事件转发。

## 运行

```powershell
cargo run --release
```

启动即打开「新建连接」对话框：

- **本地终端** 页：下拉框按操作系统探测可用 shell——Unix 用 `$SHELL` 与
  `/etc/shells`（bash / zsh / fish 等），Windows 探测 PowerShell 7 / Windows
  PowerShell / Git Bash / 命令提示符；
- **SSH** 页：主机、端口、用户、密码、密钥（可留空）。

点「连接」后对话框关闭、进入终端。顶部的「文件 / 会话 / 帮助」菜单栏可以随时
「新建连接」「清屏」「断开」、打开 **SFTP 侧边栏**，以及管理 **已保存会话** 与 **宏**；
窗口尺寸变化会自动同步远端 PTY；滚轮查看回滚；键盘事件按 xterm 控制序列编码后发往远端。

### 多标签会话

菜单栏下面一行是会话标签栏：

- 每个 SSH / 本地终端都是独立标签，各有自己的终端状态、连接标签与 **SFTP 侧边栏**
  （后台标签的 SFTP 会话保留，切换回来继续用）；
- 点标签切换；点标签右侧 `×` 关闭；`+` 或「文件 > 新建连接」新建标签；
- 标签标题优先用远端 OSC 0/2 设置的窗口标题，没有时显示 `user@host:port` 或「本地 shell」；
- 「会话 > 断开」关闭当前标签；关闭最后一个标签后回到空页并重新打开连接对话框。

### 复制、粘贴与右键菜单

- 终端里按住左键拖动即选中文本（回滚区同样适用），选中格显示高亮底色；
- `Ctrl+Shift+C` 复制、`Ctrl+Shift+V` 粘贴（开启 bracketed paste 的应用会自动加保护序列）、
  `Ctrl+Shift+A` 全选；单击终端清除选择；
- 终端区域点右键弹出「复制 / 粘贴 / 全选」菜单。

终端会回应 fish / vim 等程序启动时的 DA1、光标位置与颜色查询（写回 PTY），
因此 fish 不会再出现 `could not read response to Primary Device Attribute query` 告警。

### rz / sz 文件传输（ZMODEM）

远端执行 `sz <文件>`（下载）或 `rz`（上传）时自动识别并进入 ZMODEM 模式：

- 下载文件自动保存到 `$LIETERM_DOWNLOADS`，未设置时用 `~/Downloads`；重名自动加 `-1`、`-2`；
- `rz` 上传会弹出对话框，填写本地文件路径（支持 `~/`）后发送；取消会向对端发送 ZMODEM 取消序列；
- 传输期间普通按键被屏蔽，`Esc` 取消当前传输；进度提示显示在状态栏。

### SFTP 文件管理

「会话 > SFTP 侧边栏」打开左侧目录面板（终端区域自动缩窄，关闭后恢复）：

- 远端路径输入框 + 「↑ 上级 / 刷新 / 进入」浏览目录；目录项点击可进入，文件点击选中；
- 本地文件输入框：选中远端文件后点「下载」写到该路径；点「上传」把该文件传到当前远端目录；
- 操作结果与错误显示在面板底部。

SFTP 走**独立的 SSH 连接**（复用 `known_hosts` 与同一套认证顺序），不占用终端会话。

### 会话与宏

「会话 > 已保存会话…」打开会话管理器：

- 列表选择一条已保存的连接（主机、端口、用户名、密钥路径），「连接」把它载入 SSH 表单
  并重新打开连接对话框（密码不保存，需要时再输入）；
- 「保存当前」按名称写入（同名覆盖）当前 SSH 表单里的主机 / 端口 / 用户 / 密钥；
- 「删除选中」移除条目；所有改动立即写盘。

「会话 > 宏…」打开宏管理器：

- 宏是一段命名文本，点列表行载入名称 / 内容 / 自动回车开关；
- 「保存」新增或覆盖当前选中宏，「新建」清空编辑字段，「运行」把内容写入当前会话；
- 内容支持转义：`\n`、`\r`、`\t`、`\e`、`\0`、`\\`、`\xNN`（如 `\x03` 即 Ctrl-C）。

配置持久化在 `~/.config/lieterm/config.json`（Windows 为 `%APPDATA%\lieterm\config.json`），
可用 `LIETERM_CONFIG` 指定其它路径；文件损坏时按空配置启动且不覆盖原文件。
密码不会写入配置。

### 本地 shell（不需要 SSH 服务器）

```powershell
$env:LIETERM_LOCAL='pwsh'   # 或 cmd / bash（Windows 走 ConPTY，Unix 走 pty）
cargo run --release
```

试验用环境变量：

- `LIETERM_LOCAL_CMD`：启动后自动下发一条命令（无键盘交互也能验证渲染链路）。
- `LIETERM_IME=1`：允许系统 IME（默认关闭，避免终端里的 ASCII 命令被输入法组合成汉字）。

### SSH 认证与安全

- **主机密钥**：`~/.ssh/known_hosts` 按 TOFU 记录首次见到的密钥（等同
  `StrictHostKeyChecking=accept-new`）；同一主机、同一密钥类型的密钥变化即拒绝连接，
  并把指纹回报到连接对话框；`@revoked` 条目同样拒绝。`LIETERM_KNOWN_HOSTS` 可覆盖路径。
  - 22 端口记为 `host`，其它端口记为 `[host]:port`；兼容 `|1|` hashed hostname，
    同一主机的多种密钥类型（RSA/ECDSA/Ed25519）可并存。
- **认证顺序**：`none`（空密码账号）→ 未显式指定私钥时先尝试 ssh-agent / Pageant →
  指定私钥（或 `~/.ssh/config` 的 `IdentityFile`，再回退 `~/.ssh/id_ed25519` /
  `id_ecdsa` / `id_rsa`，逐个尝试）→ 密码 → 键盘交互（密码回填）。
  加密私钥会用密码字段尝试解锁一次；RSA 优先使用服务端支持的 SHA-2 签名算法。
- **SSH 配置**：`~/.ssh/config` 的 `HostName` / `Port` / `User` / `IdentityFile` 作为
  对话框输入的兜底（支持 `*` / `?` 与 `!` 否定，主机名匹配不区分大小写，首个匹配值生效）；
  `LIETERM_SSH_CONFIG` 可覆盖路径。IP 直连同样会应用 `Host *` 等配置兜底（与 OpenSSH 一致）。
- **超时/保活**：连接 15 s 超时、认证 30 s 超时；keepalive 20 s × 3。
- 连接失败原因显示在重新打开的连接对话框里（红字）。

## 结构

- `src/term.rs`：`alacritty_terminal::Term` + `vte::ansi::Processor`，把可见单元格拼成 zzzui `Scene`，用 `VelloPixmapRenderer` 光栅化成 RGBA 位图（`Pixmap::into_pixels` 零拷贝取出）；文本选择用 alacritty 内置 `Selection`，DA1 / DSR / 颜色查询的应答通过 `take_responses()` 回写 PTY。
- `src/transfer.rs`：`zmodem2` 状态机驱动，自动探测 `**\x18A/B` 起始头，负责下载落盘与上传读文件。
- `src/keys.rs`：逻辑键名 → xterm 转义序列（方向键、功能键、Ctrl/Alt/Shift 修饰、IME）。
- `src/ssh.rs`：russh 连接、known_hosts 校验、私钥/密码/键盘交互认证、PTY、shell、双向数据中继。
- `src/known_hosts.rs` / `src/ssh_config.rs`：OpenSSH `known_hosts` 与精简 `~/.ssh/config` 解析。
- `src/sftp.rs`：`russh-sftp` 子系统、目录列表 / 上传 / 下载任务；复用 `ssh::connect_authenticated`。
- `src/config.rs`：会话与宏的 `config.json` 读写、同名覆盖 / 删除、宏转义解析。
- `src/app.rs`：登录表单、终端画面、`KeyCapture` 捕获面、事件泵与状态栏。整帧位图走
  `Runtime::set_background_image`（按画布物理尺寸 1:1 铺底），状态栏是普通 Widget 声明；
  宿主在 `on_frame` 里改了声明字段后调用 `ctx.invalidate()`。

## 依赖的 zzzui 能力

全部是 `../zzzui` 的公开 API，不再有本地补丁：

1. `Decl::TextInput::passive`（`as_key_capture()`）/ `widget::KeyCapture`：只借焦点和键盘，
   不改文本、不画插入符；按键与滚轮以 `RawCommand::Key` / `RawCommand::Wheel` 上报。
2. `Runtime::focus_passive()`：连接建立后把键盘交给第一个捕获面（不必自己遍历实体表）。
3. `Runtime::set_background_image(Option<ImageData>)`：整窗像素 1:1 铺底，场景图元叠在上面。
4. `WindowContext::invalidate()`：宿主消费外部数据后请求重建声明；`WindowOpener::redraw(id)`
   只负责把窗口排进重绘，后台线程可以安全调用。
5. `Decl::Image::fit(ImageFit::Fill)`：需要把位图当图元显示时，关掉等比缩放。
6. `UpdateCtx::request_frame()`：`on_command` 里把动作交给外部通道后补一帧，
   下一帧的 `on_frame` 才会消费它（仅 `mark_dirty` 不会自动补帧）。

## 当前限制

- 尚未接入证书认证、`ProxyJump` 与 `~/.ssh/config` 的 `Include` / `Match`。
- SFTP 仅支持单文件上传 / 下载：无进度条、断点续传、批量/递归传输、远端新建目录/重命名/删除。
- `known_hosts` 的 `@cert-authority` 行暂不参与证书主机密钥校验（会按"未知"处理），
  仅支持 `@revoked` 拒绝与 `|1|` hashed hostname。
- 仅支持系统剪贴板；OSC52 剪贴板协议尚未实现。
- ZMODEM 只处理单文件会话，暂不支持断点续传与传输进度百分比。
- 宽字符按**列位置**绘制：宽字符连续段从 `col × cell_width` 起排，段内用
  `letter-spacing` 把每个 CJK 补到正好两格；窄字符段同样按列定位。parley 只负责字形与
  segment 内排版，不再决定跨段位置。像素级核对：20 个 CJK 后的 `X` 与 40 个 ASCII 后的
  `X` 落在同一列。
- 光标闪烁由 `WindowContext::wake_in` 驱动的宿主级定时帧实现（0.53 s）。
- 性能参考（release，960×640）：ASCII 满屏约 8 ms/帧，全中文满屏（32 行 × 60 字）约 21 ms/帧
  （耗时在 parley 排版与 vello 绘制字形，不在网格定位）。
