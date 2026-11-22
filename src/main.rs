mod app;
mod config;
mod keys;
mod known_hosts;
mod local;
mod sftp;
mod ssh;
mod ssh_config;
mod term;
mod transfer;

use anyhow::Result;
use zzzui::platform::{WindowOptions, run};

use crate::{app::LietermApp, term::Terminal};

fn main() -> Result<()> {
    // 字体 / 配色 / 编码来自持久化配置；终端样式同时用于后续新标签。
    let config = config::Config::load(&config::default_path());
    let style = term::TermStyle::from_settings(&config.terminal);
    let runtime = tokio::runtime::Runtime::new()?;
    let terminal = Terminal::new(80, 24, style);
    // `LIETERM_LOCAL=pwsh`（或 cmd / bash）时跳过登录页，直接跑本地 shell。
    // 用于没有 SSH 服务器时的真实终端试验。
    let app = LietermApp::new(terminal, runtime.handle().clone(), local::shell_from_env());
    run(app, WindowOptions::new("lieterm — SSH").size(960, 640))?;
    Ok(())
}
