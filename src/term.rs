//! alacritty_terminal 内核 + zzzui 光栅化：终端网格 → 一张 RGBA 位图。
//!
//! 不走 zzzui 的 Widget 声明树逐格绘制（那样拿不到单元格背景色），而是自己拼一帧
//! `render::Scene`，用 zzzui 的 VelloPixmapRenderer 光栅化，再交给
//! `Runtime::set_background_image` 按 1:1 铺底。字体、颜色、场景 IR 全部复用 zzzui，
//! 自有代码只负责终端语义。

use std::sync::{Arc, Mutex};

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    term::{Config as TermConfig, Term, cell::Flags},
    vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb},
};
use zzzui::{
    element::ImageData,
    geom::{Color, Point, rect},
    render::{Element, FillStrokeStyle, Scene, SceneNode, VelloPixmapRenderer},
    text::{
        FontStyle, FontWeight, GenericFamily, RichSpan, TextBaseline, TextStyle, measure_text,
        register_font_generic,
    },
};

pub const FONT_SIZE: f64 = 14.0;
pub const CELL_HEIGHT: f64 = 19.0;
pub const STATUS_HEIGHT: f64 = 26.0;

const DEFAULT_BG: [u8; 3] = [0x1c, 0x1c, 0x1c];
const DEFAULT_FG: [u8; 3] = [0xcc, 0xcc, 0xcc];

#[derive(Clone, Copy)]
struct CellView {
    /// 列号（网格位置）。终端必须按列定位，不能依赖文本自然排版。
    col: usize,
    /// 占几格：`WIDE_CHAR` 为 2，其余为 1。
    width: usize,
    ch: char,
    fg: Color,
    bg: Color,
    flags: Flags,
}

/// 注册等宽字体并测量一个字符单元的宽高。
pub fn init_font() -> (String, f64) {
    let candidates = [
        "C:/Windows/Fonts/consola.ttf",
        "C:/Windows/Fonts/cascadia.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ];
    let mut family = "monospace".to_owned();
    for path in candidates {
        if std::path::Path::new(path).exists() {
            let source = zzzui::text::FontSource::Path(std::path::PathBuf::from(path));
            if register_font_generic(source, Some("Lieterm Mono"), Some(GenericFamily::Monospace))
                .is_ok()
            {
                family = "Lieterm Mono".to_owned();
                break;
            }
        }
    }

    let style =
        TextStyle::new(Color::WHITE, FONT_SIZE, family.clone()).with_line_height(CELL_HEIGHT);
    let sample = "M".repeat(64);
    let measured = measure_text(&[RichSpan::new(sample, style)], None);
    let mut cell_width = measured.size.width / 64.0;
    if !cell_width.is_finite() || cell_width <= 0.0 {
        cell_width = FONT_SIZE * 0.6;
    }
    (family, cell_width)
}

/// 终端事件监听：当前只关心远程设置的标题（OSC 0 / 2）。
#[derive(Clone, Default)]
struct TitleListener {
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        if let Event::Title(title) = event
            && let Ok(mut slot) = self.title.lock()
        {
            *slot = Some(title);
        }
    }
}

pub struct Terminal {
    term: Term<TitleListener>,
    parser: Processor,
    renderer: VelloPixmapRenderer,
    family: String,
    cell_width: f64,
    cols: usize,
    rows: usize,
    image: Option<ImageData>,
    title: Arc<Mutex<Option<String>>>,
    cursor_visible: bool,
    received: usize,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, family: String, cell_width: f64) -> Self {
        let size = TermSize { cols, rows };
        let title = Arc::new(Mutex::new(None));
        let term = Term::new(
            TermConfig::default(),
            &size,
            TitleListener {
                title: Arc::clone(&title),
            },
        );
        Self {
            term,
            parser: Processor::new(),
            renderer: VelloPixmapRenderer::new(1, 1).with_background(zzzui::geom::Color::rgb(
                DEFAULT_BG[0],
                DEFAULT_BG[1],
                DEFAULT_BG[2],
            )),
            family,
            cell_width,
            cols,
            rows,
            image: None,
            title,
            cursor_visible: true,
            received: 0,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cell_width(&self) -> f64 {
        self.cell_width
    }

    /// 远程标题（OSC 0/2）；没有设置过时回退到 `lieterm`。
    pub fn title(&self) -> String {
        self.title
            .lock()
            .ok()
            .and_then(|title| title.clone())
            .unwrap_or_else(|| "lieterm".to_owned())
    }

    /// 光标闪烁相位（`false` 时本帧不画光标）。
    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    pub fn image(&self) -> Option<ImageData> {
        self.image.clone()
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.received += bytes.len();
        self.parser.advance(&mut self.term, bytes);
    }

    /// 累计收到的字节数（诊断用）。
    pub fn received(&self) -> usize {
        self.received
    }

    /// 终端显示的行数（诊断用）。
    pub fn screen_lines(&self) -> usize {
        self.term.grid().screen_lines()
    }

    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let size = TermSize { cols, rows };
        self.term.resize(size);
    }

    /// 按设备像素重画整屏；`scale` 是逻辑像素到设备像素的比例。
    pub fn render(
        &mut self,
        px_width: u32,
        px_height: u32,
        scale: f64,
        top_inset: f64,
        left_inset: f64,
    ) {
        let px_width = px_width.max(1);
        let px_height = px_height.max(1);
        let cw = (self.cell_width * scale).max(1.0);
        let ch = (CELL_HEIGHT * scale).max(1.0);
        let font_size = FONT_SIZE * scale;
        let base = TextStyle::new(
            Color::rgb(DEFAULT_FG[0], DEFAULT_FG[1], DEFAULT_FG[2]),
            font_size,
            self.family.clone(),
        )
        .with_line_height(ch)
        .with_baseline(TextBaseline::Top);

        let mut scene = Scene::new(px_width as f64, px_height as f64);
        scene.background = Color::rgb(DEFAULT_BG[0], DEFAULT_BG[1], DEFAULT_BG[2]);
        let bg_color = scene.background;

        let content = self.term.renderable_content();
        let visible_rows = self.term.grid().screen_lines().min(self.rows);
        let offset = content.display_offset;
        let mut rows: Vec<Vec<CellView>> = vec![Vec::with_capacity(self.cols); visible_rows];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset as i32;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= visible_rows || col >= self.cols {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            // `Flags::contains(A | B)` 要求**同时**包含两个 flag；宽字符的填充格只有
            // 其中一个，必须用 `intersects`，否则每个 CJK 后面都会多画一个空格格。
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = to_zzz_color(cell.fg, DEFAULT_FG);
            let mut bg = to_zzz_color(cell.bg, DEFAULT_BG);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) && !flags.contains(Flags::DIM_BOLD) {
                fg = blend(fg, bg, 0.5);
            }
            let ch = if flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };
            let width = if flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            rows[row as usize].push(CellView {
                col,
                width,
                ch,
                fg,
                bg,
                flags,
            });
        }

        for (row_index, cells) in rows.iter().enumerate() {
            let y = top_inset + row_index as f64 * ch;
            // 背景：按**列位置**合并相邻同色（宽字符占两格），避免每格一个图元。
            let mut start = 0usize;
            while start < cells.len() {
                let color = cells[start].bg;
                let mut end = start + 1;
                while end < cells.len()
                    && cells[end].bg == color
                    && cells[end].col == cells[end - 1].col + cells[end - 1].width
                {
                    end += 1;
                }
                if color != bg_color {
                    // `geom::rect` 是「左上角 + 宽高」，不是 min/max。
                    let first = &cells[start];
                    let last = &cells[end - 1];
                    let x = left_inset + first.col as f64 * cw;
                    let width = (last.col + last.width - first.col) as f64 * cw;
                    let area = rect(x, y, width, ch);
                    scene.push_node(SceneNode::new(Element::rect(
                        area,
                        FillStrokeStyle::fill(color),
                    )));
                }
                start = end;
            }

            // 前景：**按列定位**，不依赖整行文本的自然排版：
            // - 宽字符单独成段，起点 = col × cw（字形 advance 与两格网格的差不会累积）；
            // - 窄字符按"列连续 + 同色同属性"成段，段起点同样是 col × cw。
            // 这样 CJK 与后续 ASCII 始终落在各自的格子里。
            let mut index = 0usize;
            while index < cells.len() {
                let cell = cells[index];
                if cell.width > 1 {
                    let style = cell_style(&base, cell.fg, cell.flags);
                    scene.push_node(SceneNode::new(Element::rich_text(
                        vec![RichSpan::new(cell.ch.to_string(), style)],
                        Point::new(left_inset + cell.col as f64 * cw, y),
                        base.clone(),
                    )));
                    index += 1;
                    continue;
                }

                let start = index;
                let mut text = String::new();
                while index < cells.len()
                    && cells[index].width == 1
                    && cells[index].fg == cell.fg
                    && cells[index].flags == cell.flags
                    && (index == start
                        || cells[index].col == cells[index - 1].col + cells[index - 1].width)
                {
                    text.push(cells[index].ch);
                    index += 1;
                }
                let style = cell_style(&base, cell.fg, cell.flags);
                scene.push_node(SceneNode::new(Element::rich_text(
                    vec![RichSpan::new(text, style)],
                    Point::new(left_inset + cells[start].col as f64 * cw, y),
                    base.clone(),
                )));
            }
        }

        let cursor = content.cursor;
        if self.cursor_visible && cursor.shape != CursorShape::Hidden {
            let row = cursor.point.line.0 + offset as i32;
            if row >= 0 && (row as usize) < visible_rows {
                let x = left_inset + cursor.point.column.0 as f64 * cw;
                let y = top_inset + row as f64 * ch;
                let area = match cursor.shape {
                    CursorShape::Underline => rect(x, y + ch - 2.0 * scale, cw, 2.0 * scale),
                    CursorShape::Beam => rect(x, y, 2.0 * scale, ch),
                    _ => rect(x, y, cw, ch),
                };
                let color = Color::rgba(DEFAULT_FG[0], DEFAULT_FG[1], DEFAULT_FG[2], 160);
                scene.push_node(SceneNode::new(Element::rect(
                    area,
                    FillStrokeStyle::fill(color),
                )));
            }
        }

        self.renderer.resize(px_width, px_height);
        let pixmap = self.renderer.render_scene_to_pixmap(&scene);
        // 像素缓冲是 `Arc<[u8]>`：直接取走，不再整屏复制一次。
        self.image = ImageData::new(px_width, px_height, pixmap.into_pixels());
    }
}

struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

fn cell_style(base: &TextStyle, color: Color, flags: Flags) -> TextStyle {
    let mut style = TextStyle {
        color,
        ..base.clone()
    };
    if flags.contains(Flags::BOLD) {
        style.font_weight = FontWeight::Bold;
    }
    if flags.contains(Flags::ITALIC) {
        style.font_style = FontStyle::Italic;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        style.underline = true;
    }
    if flags.contains(Flags::STRIKEOUT) {
        style.strikethrough = true;
    }
    style
}

fn blend(fg: Color, bg: Color, alpha: f64) -> Color {
    let a = fg.to_array();
    let b = bg.to_array();
    let mix = |x: u8, y: u8| (x as f64 * alpha + y as f64 * (1.0 - alpha)).round() as u8;
    Color::rgb(mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]))
}

fn to_zzz_color(color: AnsiColor, default: [u8; 3]) -> Color {
    fn array(rgb: [u8; 3]) -> Color {
        Color::rgb(rgb[0], rgb[1], rgb[2])
    }
    match color {
        AnsiColor::Spec(Rgb { r, g, b }) => Color::rgb(r, g, b),
        AnsiColor::Indexed(index) => array(indexed_color(index)),
        AnsiColor::Named(named) => array(named_color(named, default)),
    }
}

const PALETTE: [[u8; 3]; 16] = [
    [0x1c, 0x1c, 0x1c],
    [0xb5, 0x47, 0x47],
    [0x6e, 0x9e, 0x50],
    [0xc5, 0x96, 0x40],
    [0x42, 0x7a, 0xba],
    [0xac, 0x71, 0x9a],
    [0x39, 0xaa, 0xb2],
    [0xbe, 0xbe, 0xbe],
    [0x5c, 0x5c, 0x5c],
    [0xdf, 0x74, 0x74],
    [0x98, 0xc5, 0x6e],
    [0xea, 0xbc, 0x5a],
    [0x6e, 0x9e, 0xd9],
    [0xcb, 0x94, 0xba],
    [0x66, 0xcd, 0xcd],
    [0xf1, 0xf1, 0xf1],
];

fn named_color(named: NamedColor, default: [u8; 3]) -> [u8; 3] {
    let index = match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Foreground | NamedColor::BrightForeground => return default,
        NamedColor::Background => return DEFAULT_BG,
        NamedColor::Cursor => return DEFAULT_FG,
        NamedColor::DimBlack => 8,
        NamedColor::DimRed => 1,
        NamedColor::DimGreen => 2,
        NamedColor::DimYellow => 3,
        NamedColor::DimBlue => 4,
        NamedColor::DimMagenta => 5,
        NamedColor::DimCyan => 6,
        NamedColor::DimWhite => 7,
        NamedColor::DimForeground => return [0x80, 0x80, 0x80],
    };
    PALETTE[index as usize]
}

fn indexed_color(index: u8) -> [u8; 3] {
    match index {
        0..=15 => PALETTE[index as usize],
        16..=231 => {
            let n = index - 16;
            let levels = [0u8, 95, 135, 175, 215, 255];
            [
                levels[(n / 36) as usize],
                levels[((n % 36) / 6) as usize],
                levels[(n % 6) as usize],
            ]
        }
        232..=255 => {
            let v = 8 + (index - 232) * 10;
            [v, v, v]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_palette_is_stable() {
        assert_eq!(
            to_zzz_color(AnsiColor::Named(NamedColor::Red), DEFAULT_FG),
            Color::rgb(0xb5, 0x47, 0x47)
        );
        assert_eq!(
            to_zzz_color(AnsiColor::Indexed(16), DEFAULT_FG),
            Color::rgb(0, 0, 0)
        );
        assert_eq!(
            to_zzz_color(AnsiColor::Indexed(255), DEFAULT_FG),
            Color::rgb(238, 238, 238)
        );
    }

    #[test]
    fn terminal_advances_vte_bytes() {
        let (family, cw) = ("monospace".to_owned(), 8.0);
        let mut term = Terminal::new(20, 5, family, cw);
        term.advance(b"\x1b[1;32mgreen\x1b[0m plain\r\nworld\x1b[41mRED\x1b[0m");
        term.render(800, 400, 1.0, 0.0, 0.0);
        let image = term.image().expect("渲染出位图");
        assert_eq!((image.width, image.height), (800, 400));
    }
}
