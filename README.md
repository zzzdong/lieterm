# lieterm

基于本地 `../zzzui` 实验性 UI 库的 PuTTY 式 SSH 终端。终端仿真用 `alacritty_terminal` 0.26，SSH 客户端用 `russh` 0.65.2，宿主密钥/认证均在独立的 tokio 运行时里处理，UI 线程只负责渲染与事件转发。

## 运行

```powershell
cargo run --release
```

启动后填写主机、端口、用户名、密码，点击「连接」。窗口尺寸变化会自动通过 SSH `window-change` 同步远端 PTY；滚轮查看回滚；键盘事件按 xterm 控制序列编码后发往远端。

## 结构

- `src/term.rs`：`alacritty_terminal::Term` + `vte::ansi::Processor`，把可见单元格拼成 zzzui `Scene`，用 `VelloPixmapRenderer` 光栅化成 RGBA 位图。
- `src/keys.rs`：逻辑键名 → xterm 转义序列（方向键、功能键、Ctrl/Alt/Shift 修饰、IME）。
- `src/ssh.rs`：russh 连接、密码认证、PTY、shell、双向数据中继。
- `src/app.rs`：登录表单、终端画面、被动键盘捕获面、事件泵与状态栏。

## 对 zzzui 的扩展

为支持自绘终端，本地 zzzui 增加了三项小能力（均在 `../zzzui`）：

1. `Decl::TextInput::passive`（`as_key_capture()`）：只借焦点和键盘，不编辑文本、不画插入符。
2. `RawCommand::Key` / `RawCommand::Wheel`：运行时把原始按键与滚轮发给被动捕获面的归属 Widget。
3. `WindowOpener::redraw(id)`：后台线程到达数据时请求指定窗口重画；`VelloPixmapRenderer::resize` 支持画布尺寸变化。

## 当前限制

- 主机密钥目前一律接受（仅打印到 stderr），尚未实现 PuTTY 式指纹缓存与严格校验。
- 只支持密码认证；公钥 / Pageant / 键盘交互认证未接入。
- 鼠标选择、剪贴板 OSC52、光标闪烁尚未实现；光标为半透明块。
- 宽字符按等宽字体单元格近似排版，双宽字形可能溢出所在格。
