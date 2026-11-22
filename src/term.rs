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
    index::{Column, Point as TermPoint, Side},
    selection::{Selection, SelectionRange, SelectionType},
    term::{Config as TermConfig, Term, TermMode, cell::Flags, viewport_to_point},
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

use crate::config::TerminalSettings;

/// 默认字号；`TerminalSettings` 未配置时用它换算单元格高度。
pub const FONT_SIZE: f64 = 14.0;
pub const CELL_HEIGHT: f64 = 19.0;
pub const STATUS_HEIGHT: f64 = 26.0;

/// 终端配色：默认前景 / 背景 / 光标 / 选区，以及 16 色基础调色板。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermTheme {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub cursor: [u8; 3],
    pub selection: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

impl Default for TermTheme {
    fn default() -> Self {
        Self::from_name("dark")
    }
}

impl TermTheme {
    /// 按名称取内置配色；未知名称回退到深色。
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" => Self {
                background: [0xfa, 0xfa, 0xfa],
                foreground: [0x2b, 0x2b, 0x2b],
                cursor: [0x2b, 0x2b, 0x2b],
                selection: [0xcf, 0xe4, 0xfb],
                palette: [
                    [0x00, 0x00, 0x00],
                    [0xc9, 0x1b, 0x1b],
                    [0x00, 0x7a, 0x00],
                    [0x8f, 0x5c, 0x00],
                    [0x1e, 0x5f, 0xc4],
                    [0x9c, 0x27, 0xb0],
                    [0x00, 0x7a, 0x7a],
                    [0xb0, 0xb0, 0xb0],
                    [0x5a, 0x5a, 0x5a],
                    [0xe0, 0x30, 0x30],
                    [0x20, 0xa0, 0x20],
                    [0xb0, 0x74, 0x00],
                    [0x30, 0x78, 0xe0],
                    [0xb8, 0x40, 0xc8],
                    [0x00, 0x9a, 0x9a],
                    [0xf0, 0xf0, 0xf0],
                ],
            },
            "solarized" => Self {
                background: [0x00, 0x2b, 0x36],
                foreground: [0x83, 0x94, 0x96],
                cursor: [0x93, 0xa1, 0xa1],
                selection: [0x07, 0x36, 0x42],
                palette: [
                    [0x07, 0x36, 0x42],
                    [0xdc, 0x32, 0x2f],
                    [0x85, 0x99, 0x00],
                    [0xb5, 0x89, 0x00],
                    [0x26, 0x8b, 0xd2],
                    [0xd3, 0x36, 0x82],
                    [0x2a, 0xa1, 0x98],
                    [0xee, 0xe8, 0xd5],
                    [0x00, 0x2b, 0x36],
                    [0xcb, 0x4b, 0x16],
                    [0x58, 0x6e, 0x75],
                    [0x65, 0x7b, 0x83],
                    [0x83, 0x94, 0x96],
                    [0x6c, 0x71, 0xc4],
                    [0x93, 0xa1, 0xa1],
                    [0xfd, 0xf6, 0xe3],
                ],
            },
            _ => Self {
                background: [0x1c, 0x1c, 0x1c],
                foreground: [0xcc, 0xcc, 0xcc],
                cursor: [0xcc, 0xcc, 0xcc],
                selection: [0x26, 0x4f, 0x78],
                palette: [
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
                ],
            },
        }
    }

    /// 发送给 `VelloPixmapRenderer` 的背景色。
    fn background_color(&self) -> Color {
        Color::rgb(self.background[0], self.background[1], self.background[2])
    }
}

/// 远程输出 / 本地粘贴使用的字符编码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextEncoding {
    #[default]
    Utf8,
    Gbk,
    Gb18030,
    Big5,
    ShiftJis,
    Latin1,
}

impl TextEncoding {
    /// 按名称解析；未知名称回退到 UTF-8。
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "gbk" => Self::Gbk,
            "gb18030" | "gb2312" => Self::Gb18030,
            "big5" => Self::Big5,
            "shift-jis" | "sjis" => Self::ShiftJis,
            "latin1" | "iso-8859-1" | "windows-1252" => Self::Latin1,
            _ => Self::Utf8,
        }
    }

    /// UTF-8 直接交给 vte；其余编码用 `encoding_rs` 流式解码。
    fn codec(self) -> Option<&'static encoding_rs::Encoding> {
        match self {
            Self::Utf8 => None,
            Self::Gbk => Some(encoding_rs::GBK),
            Self::Gb18030 => Some(encoding_rs::GB18030),
            Self::Big5 => Some(encoding_rs::BIG5),
            Self::ShiftJis => Some(encoding_rs::SHIFT_JIS),
            Self::Latin1 => Some(encoding_rs::WINDOWS_1252),
        }
    }

    fn decoder(self) -> Option<encoding_rs::Decoder> {
        self.codec().map(|codec| codec.new_decoder())
    }

    /// 把本地文本编码成 PTY 字节（UTF-8 时零拷贝）。
    fn encode(self, text: &str) -> std::borrow::Cow<'_, [u8]> {
        match self.codec() {
            Some(codec) => codec.encode(text).0,
            None => std::borrow::Cow::Borrowed(text.as_bytes()),
        }
    }
}

/// 一个终端的完整外观参数：字体度量 + 配色 + 编码。
#[derive(Clone, Debug)]
pub struct TermStyle {
    pub family: String,
    pub font_size: f64,
    pub cell_width: f64,
    pub cell_height: f64,
    pub theme: TermTheme,
    pub encoding: TextEncoding,
}

impl TermStyle {
    /// 从持久化设置构造样式：探测字体、按字号换算单元格高度。
    #[must_use]
    pub fn from_settings(settings: &TerminalSettings) -> Self {
        let font_size = settings.clamped_font_size();
        let cell_height = (font_size * CELL_HEIGHT / FONT_SIZE).round().max(font_size);
        let family = if settings.font_family.trim().is_empty() {
            register_bundled_mono()
        } else {
            settings.font_family.trim().to_owned()
        };

        let style =
            TextStyle::new(Color::WHITE, font_size, family.clone()).with_line_height(cell_height);
        let sample = "M".repeat(64);
        let measured = measure_text(&[RichSpan::new(sample, style)], None);
        let mut cell_width = measured.size.width / 64.0;
        if !cell_width.is_finite() || cell_width <= 0.0 {
            cell_width = font_size * 0.6;
        }
        Self {
            family,
            font_size,
            cell_width,
            cell_height,
            theme: TermTheme::from_name(&settings.color_scheme),
            encoding: TextEncoding::from_name(&settings.encoding),
        }
    }
}

/// 注册随操作系统常见的内置等宽字体，返回最终字体族名。
fn register_bundled_mono() -> String {
    let candidates = [
        "C:/Windows/Fonts/consola.ttf",
        "C:/Windows/Fonts/cascadia.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ];
    for path in candidates {
        if std::path::Path::new(path).exists() {
            let source = zzzui::text::FontSource::Path(std::path::PathBuf::from(path));
            if register_font_generic(source, Some("Lieterm Mono"), Some(GenericFamily::Monospace))
                .is_ok()
            {
                return "Lieterm Mono".to_owned();
            }
        }
    }
    "monospace".to_owned()
}

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

/// 终端事件监听：远程标题（OSC 0 / 2）与需要回写 PTY 的查询响应。
///
/// fish / vim 等程序启动时会发 Primary Device Attribute（DA1）、光标位置、
/// 颜色查询；alacritty_terminal 把应答文本通过 `Event::PtyWrite` /
/// `Event::ColorRequest` 交出来，宿主必须写回 PTY，否则 fish 会等到超时并告警。
#[derive(Clone, Default)]
struct TermListener {
    title: Arc<Mutex<Option<String>>>,
    responses: Arc<Mutex<Vec<u8>>>,
    /// 配色与终端共享：OSC 颜色查询要按当前配色回答。
    theme: Arc<Mutex<TermTheme>>,
}

impl TermListener {
    fn push_response(&self, bytes: &[u8]) {
        if let Ok(mut slot) = self.responses.lock() {
            slot.extend_from_slice(bytes);
        }
    }
}

impl EventListener for TermListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                if let Ok(mut slot) = self.title.lock() {
                    *slot = Some(title);
                }
            }
            Event::PtyWrite(text) => self.push_response(text.as_bytes()),
            Event::ColorRequest(index, formatter) => {
                let theme = self
                    .theme
                    .lock()
                    .map(|theme| theme.clone())
                    .unwrap_or_default();
                let text = formatter(query_color(index, &theme));
                self.push_response(text.as_bytes());
            }
            _ => {}
        }
    }
}

/// 把 OSC 颜色查询的 `NamedColor` 序号换算成当前调色板里的具体颜色。
fn query_color(index: usize, theme: &TermTheme) -> Rgb {
    let rgb = match index {
        // Foreground / Cursor / BrightForeground。
        256 | 258 | 267 => theme.foreground,
        // Background。
        257 => theme.background,
        // DimBlack..=DimWhite 对应普通 0..=7。
        259..=266 => theme.palette[index - 259],
        // DimForeground。
        268 => [0x80, 0x80, 0x80],
        // 16..=255 是索引色；0..=15 是基础调色板。
        _ => indexed_color(index as u8, &theme.palette),
    };
    Rgb {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
    }
}

pub struct Terminal {
    term: Term<TermListener>,
    parser: Processor,
    renderer: VelloPixmapRenderer,
    family: String,
    font_size: f64,
    cell_width: f64,
    cell_height: f64,
    theme: Arc<Mutex<TermTheme>>,
    encoding: TextEncoding,
    /// 非 UTF-8 编码的流式解码器；UTF-8 时为 None（字节直接进 vte）。
    decoder: Option<encoding_rs::Decoder>,
    cols: usize,
    rows: usize,
    image: Option<ImageData>,
    title: Arc<Mutex<Option<String>>>,
    responses: Arc<Mutex<Vec<u8>>>,
    cursor_visible: bool,
    received: usize,
    /// 选择起点（网格坐标）。alacritty 在排序锚点时会连同 `Side` 一起交换，
    /// 反向拖动会导致两端各缩一格；这里记住起点，每次更新都按方向重建选择。
    selection_anchor: Option<TermPoint>,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, style: TermStyle) -> Self {
        let size = TermSize { cols, rows };
        let title = Arc::new(Mutex::new(None));
        let responses = Arc::new(Mutex::new(Vec::new()));
        let theme = Arc::new(Mutex::new(style.theme.clone()));
        let term = Term::new(
            TermConfig::default(),
            &size,
            TermListener {
                title: Arc::clone(&title),
                responses: Arc::clone(&responses),
                theme: Arc::clone(&theme),
            },
        );
        Self {
            term,
            parser: Processor::new(),
            renderer: VelloPixmapRenderer::new(1, 1)
                .with_background(style.theme.background_color()),
            family: style.family,
            font_size: style.font_size,
            cell_width: style.cell_width,
            cell_height: style.cell_height,
            theme,
            encoding: style.encoding,
            decoder: style.encoding.decoder(),
            cols,
            rows,
            image: None,
            title,
            responses,
            cursor_visible: true,
            received: 0,
            selection_anchor: None,
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

    /// 单元格高度（逻辑像素），几何与命中测试都按它换算行号。
    pub fn cell_height(&self) -> f64 {
        self.cell_height
    }

    /// 当前外观与编码参数（新建标签直接克隆）。
    pub fn style(&self) -> TermStyle {
        TermStyle {
            family: self.family.clone(),
            font_size: self.font_size,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
            theme: self
                .theme
                .lock()
                .map(|theme| theme.clone())
                .unwrap_or_default(),
            encoding: self.encoding,
        }
    }

    /// 切换配色（对所有已存在的终端立即生效）。
    pub fn set_theme(&mut self, theme: TermTheme) {
        self.renderer = VelloPixmapRenderer::new(1, 1).with_background(theme.background_color());
        if let Ok(mut slot) = self.theme.lock() {
            *slot = theme;
        }
    }

    /// 切换编码；解码器重置，边界处的半个字符可能显示为替换符。
    pub fn set_encoding(&mut self, encoding: TextEncoding) {
        self.encoding = encoding;
        self.decoder = encoding.decoder();
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

    /// 喂入终端字节流。非 UTF-8 编码先流式解码成 UTF-8 再交给 vte，
    /// 跨 `advance` 调用不完整的尾字节由解码器内部缓冲。
    pub fn advance(&mut self, bytes: &[u8]) {
        self.received += bytes.len();
        match &mut self.decoder {
            None => self.parser.advance(&mut self.term, bytes),
            Some(decoder) => {
                // `decode_to_string` 只写入预留容量，容量不足会静默截断，
                // 因此按 UTF-8 最坏膨胀（4 倍）预留。
                let mut text = String::with_capacity(bytes.len().saturating_mul(4) + 8);
                let _ = decoder.decode_to_string(bytes, &mut text, false);
                if !text.is_empty() {
                    self.parser.advance(&mut self.term, text.as_bytes());
                }
            }
        }
    }

    /// 取走终端查询（DA1 / DSR / 颜色）待回写 PTY 的响应字节。
    pub fn take_responses(&self) -> Vec<u8> {
        self.responses
            .lock()
            .map(|mut slot| std::mem::take(&mut *slot))
            .unwrap_or_default()
    }

    /// 按查看区行 / 列开始一次选择（行列都相对当前视口，含回滚偏移）。
    pub fn start_selection(&mut self, row: usize, col: usize) {
        let point = self.viewport_point(row, col);
        self.selection_anchor = Some(point);
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
    }

    /// 拖动中更新选择终点。
    ///
    /// 重新构造选择而不是只调 `update`：`to_range` 在起点晚于终点时会交换两个
    /// 锚点（`Side` 跟着锚点走），反向拖动会把按下格与当前格都排除掉。
    pub fn update_selection(&mut self, row: usize, col: usize) {
        let point = self.viewport_point(row, col);
        let Some(anchor) = self.selection_anchor else {
            return;
        };
        let (start, end) = if point < anchor {
            (point, anchor)
        } else {
            (anchor, point)
        };
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.term.selection = Some(selection);
    }

    /// 选中当前可见区域的全部行。
    pub fn select_all(&mut self) {
        self.selection_anchor = None;
        let offset = self.term.grid().display_offset();
        let rows = self.term.grid().screen_lines().max(1);
        let start = viewport_to_point(offset, TermPoint::new(0, Column(0)));
        let end = viewport_to_point(offset, TermPoint::new(rows - 1, Column(0)));
        let mut selection = Selection::new(SelectionType::Lines, start, Side::Left);
        selection.update(end, Side::Right);
        self.term.selection = Some(selection);
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.term.selection = None;
    }

    /// 当前选中的文本；没有选择或选择为空时返回 `None`。
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// 把粘贴文本编码成 PTY 字节（应用开启了 bracketed paste 时加保护序列）。
    /// 非 UTF-8 编码（如 GBK）会按当前编码转换后再发送。
    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let encoded = self.encoding.encode(text);
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let mut bytes = Vec::with_capacity(encoded.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(&encoded);
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            encoded.into_owned()
        }
    }

    fn viewport_point(&self, row: usize, col: usize) -> TermPoint {
        let row = row.min(self.rows.saturating_sub(1));
        let col = col.min(self.cols.saturating_sub(1));
        let offset = self.term.grid().display_offset();
        viewport_to_point(offset, TermPoint::new(row, Column(col)))
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
        let ch = (self.cell_height * scale).max(1.0);
        let font_size = self.font_size * scale;
        let theme = self
            .theme
            .lock()
            .map(|theme| theme.clone())
            .unwrap_or_default();
        let base = TextStyle::new(
            Color::rgb(
                theme.foreground[0],
                theme.foreground[1],
                theme.foreground[2],
            ),
            font_size,
            self.family.clone(),
        )
        .with_line_height(ch)
        .with_baseline(TextBaseline::Top);

        let mut scene = Scene::new(px_width as f64, px_height as f64);
        scene.background = theme.background_color();
        let bg_color = scene.background;

        let selection = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term));
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
            let mut fg = to_zzz_color(cell.fg, &theme);
            let mut bg = to_zzz_color(cell.bg, &theme);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selection.is_some_and(|range: SelectionRange| range.contains(indexed.point)) {
                bg = Color::rgb(theme.selection[0], theme.selection[1], theme.selection[2]);
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
                let color = Color::rgba(theme.cursor[0], theme.cursor[1], theme.cursor[2], 160);
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

fn to_zzz_color(color: AnsiColor, theme: &TermTheme) -> Color {
    fn array(rgb: [u8; 3]) -> Color {
        Color::rgb(rgb[0], rgb[1], rgb[2])
    }
    match color {
        AnsiColor::Spec(Rgb { r, g, b }) => Color::rgb(r, g, b),
        AnsiColor::Indexed(index) => array(indexed_color(index, &theme.palette)),
        AnsiColor::Named(named) => array(named_color(named, theme)),
    }
}

fn named_color(named: NamedColor, theme: &TermTheme) -> [u8; 3] {
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
        NamedColor::Foreground | NamedColor::BrightForeground => return theme.foreground,
        NamedColor::Background => return theme.background,
        NamedColor::Cursor => return theme.cursor,
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
    theme.palette[index as usize]
}

fn indexed_color(index: u8, palette: &[[u8; 3]; 16]) -> [u8; 3] {
    match index {
        0..=15 => palette[index as usize],
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

    fn style() -> TermStyle {
        TermStyle {
            family: "monospace".to_owned(),
            font_size: FONT_SIZE,
            cell_width: 8.0,
            cell_height: CELL_HEIGHT,
            theme: TermTheme::default(),
            encoding: TextEncoding::Utf8,
        }
    }

    #[test]
    fn ansi_palette_is_stable() {
        let theme = TermTheme::default();
        assert_eq!(
            to_zzz_color(AnsiColor::Named(NamedColor::Red), &theme),
            Color::rgb(0xb5, 0x47, 0x47)
        );
        assert_eq!(
            to_zzz_color(AnsiColor::Indexed(16), &theme),
            Color::rgb(0, 0, 0)
        );
        assert_eq!(
            to_zzz_color(AnsiColor::Indexed(255), &theme),
            Color::rgb(238, 238, 238)
        );
    }

    #[test]
    fn light_scheme_swaps_background_and_palette() {
        let theme = TermTheme::from_name("light");
        assert_eq!(theme.background, [0xfa, 0xfa, 0xfa]);
        assert_eq!(
            to_zzz_color(AnsiColor::Named(NamedColor::Red), &theme),
            Color::rgb(0xc9, 0x1b, 0x1b)
        );
        assert_eq!(
            to_zzz_color(AnsiColor::Named(NamedColor::Background), &theme),
            Color::rgb(0xfa, 0xfa, 0xfa)
        );
    }

    #[test]
    fn gbk_bytes_are_decoded_before_vte() {
        // 解码器按容量写入：验证流式解码本身（跨多次 advance 的尾字节也要保留）。
        let mut decoder = TextEncoding::Gbk.decoder().expect("GBK 解码器");
        let mut decoded = String::with_capacity(64);
        let _ = decoder.decode_to_string(&[0xd6, 0xd0], &mut decoded, false);
        let _ = decoder.decode_to_string(&[0xce, 0xc4], &mut decoded, false);
        assert_eq!(decoded, "中文");
        let mut styled = style();
        styled.encoding = TextEncoding::Gbk;
        let mut term = Terminal::new(20, 5, styled);
        // “中文”的 GBK 编码。
        term.advance(&[0xd6, 0xd0, 0xce, 0xc4]);
        term.select_all();
        let text = term.selection_text().unwrap_or_default();
        assert!(text.contains("中文"), "GBK 输出应解码为 UTF-8：{text:?}");
    }

    #[test]
    fn paste_uses_configured_encoding() {
        let mut styled = style();
        styled.encoding = TextEncoding::Gbk;
        let term = Terminal::new(20, 5, styled);
        assert_eq!(term.paste_bytes("中文"), vec![0xd6, 0xd0, 0xce, 0xc4]);
    }

    #[test]
    fn terminal_advances_vte_bytes() {
        let mut term = Terminal::new(20, 5, style());
        term.advance(b"\x1b[1;32mgreen\x1b[0m plain\r\nworld\x1b[41mRED\x1b[0m");
        term.render(800, 400, 1.0, 0.0, 0.0);
        let image = term.image().expect("渲染出位图");
        assert_eq!((image.width, image.height), (800, 400));
    }

    #[test]
    fn terminal_answers_primary_device_attribute_query() {
        // fish 启动时会发 DA1 并等待应答；不写回 PTY 就会等 10 秒后告警。
        let mut term = Terminal::new(20, 5, style());
        term.advance(b"\x1b[c");
        assert_eq!(term.take_responses(), b"\x1b[?6c");
        assert!(term.take_responses().is_empty(), "响应只能被取走一次");
    }

    #[test]
    fn selection_extracts_text() {
        let mut term = Terminal::new(20, 5, style());
        term.advance(b"hello world");
        term.start_selection(0, 0);
        term.update_selection(0, 4);
        assert_eq!(term.selection_text().as_deref(), Some("hello"));

        // 反向拖动两端的格子也要包含，不能各缩一格。
        term.start_selection(0, 10);
        term.update_selection(0, 6);
        assert_eq!(term.selection_text().as_deref(), Some("world"));
        term.start_selection(0, 6);
        term.update_selection(0, 10);
        assert_eq!(term.selection_text().as_deref(), Some("world"));

        term.clear_selection();
        assert!(term.selection_text().is_none());

        term.select_all();
        let text = term.selection_text().unwrap_or_default();
        assert!(text.contains("hello world"));
    }
}
