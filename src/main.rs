mod app;
mod keys;
mod ssh;
mod term;

use anyhow::Result;
use zzzui::platform::{WindowOptions, run};

use crate::{app::LietermApp, term::Terminal};

fn main() -> Result<()> {
    let (family, cell_width) = term::init_font();
    let runtime = tokio::runtime::Runtime::new()?;
    let terminal = Terminal::new(80, 24, family, cell_width);
    let app = LietermApp::new(terminal, cell_width, runtime.handle().clone());
    run(app, WindowOptions::new("lieterm — SSH").size(960, 640))?;
    Ok(())
}
