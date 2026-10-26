//! mado-file-manager — pixel plugin for Mado
//!
//! Renders a file tree in the sidebar.  Folders toggle open/closed on click.
//! Files open in the configured editor via MACT open_file.
//! Right-click shows a context menu for rename / delete / new file / new folder.
//!
//! stdin:  newline-delimited JSON events from Mado
//! stdout: MADO pixel frames + MACT action messages

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fontdue::{Font, FontSettings};
use serde_json::Value;

// ── Font ──────────────────────────────────────────────────────────────────────

static FONT_BYTES: &[u8] =
    include_bytes!("../assets/HackNerdFontMono-Regular.ttf");

// ── Colours ───────────────────────────────────────────────────────────────────

const BG:           [u8; 4] = [0x1e, 0x20, 0x30, 0xff];
const BG_HOVER:     [u8; 4] = [0x2a, 0x2d, 0x42, 0xff];
const BG_SEL:       [u8; 4] = [0x32, 0x3a, 0x5a, 0xff];
const BG_CTX:       [u8; 4] = [0x22, 0x25, 0x38, 0xff];
const BG_CTX_SEL:   [u8; 4] = [0x3a, 0x43, 0x6a, 0xff];
const FG:           [u8; 4] = [0xcc, 0xcc, 0xcc, 0xff];
const FG_DIM:       [u8; 4] = [0x77, 0x7a, 0x99, 0xff];
const FG_DIR:       [u8; 4] = [0x77, 0xaa, 0xff, 0xff];
const FG_RENAME:    [u8; 4] = [0xff, 0xff, 0xff, 0xff];
const BG_RENAME:    [u8; 4] = [0x1a, 0x1c, 0x2e, 0xff];
const BORDER_RENAME:[u8; 4] = [0x55, 0x88, 0xcc, 0xff];
const CURSOR_COL:   [u8; 4] = [0x55, 0x88, 0xff, 0xff];
const FG_DANGER:    [u8; 4] = [0xff, 0x77, 0x77, 0xff];

// ── Layout ────────────────────────────────────────────────────────────────────

const ROW_H:     f32 = 22.0;
const INDENT_W:  f32 = 16.0;
const PADDING_L: f32 = 8.0;
const ICON_W:    f32 = 16.0;
const FONT_SIZE: f32 = 13.0;

// ── MACT helpers ──────────────────────────────────────────────────────────────

fn write_mact(json: &str) {
    let bytes = json.as_bytes();
    let len = bytes.len() as u32;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(b"MACT");
    let _ = out.write_all(&len.to_le_bytes());
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn mact_open_file(path: &str) {
    let json = format!(
        r#"{{"action":"open_file","path":{}}}"#,
        serde_json::to_string(path).unwrap_or_else(|_| "\"\"".into())
    );
    write_mact(&json);
}

fn mact_notify(title: &str, msg: &str) {
    let json = format!(
        r#"{{"action":"notify","title":{},"message":{}}}"#,
        serde_json::to_string(title).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(msg).unwrap_or_else(|_| "\"\"".into()),
    );
    write_mact(&json);
}

// ── MADO pixel frame writer ───────────────────────────────────────────────────

fn write_frame(w: u32, h: u32, pixels: &[u8]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(b"MADO");
    let _ = out.write_all(&w.to_le_bytes());
    let _ = out.write_all(&h.to_le_bytes());
    let _ = out.write_all(pixels);
    let _ = out.flush();
}

// ── Glyph cache ───────────────────────────────────────────────────────────────

struct GlyphCache {
    font:  Font,
    cache: std::collections::HashMap<(u32, u32), (Vec<u8>, fontdue::Metrics)>,
}

impl GlyphCache {
    fn new(font: Font) -> Self {
        GlyphCache { font, cache: std::collections::HashMap::new() }
    }

    fn rasterize(&mut self, ch: char, px: f32) -> (&[u8], &fontdue::Metrics) {
        let key = (ch as u32, px as u32);
        if !self.cache.contains_key(&key) {
            let (metrics, bitmap) = self.font.rasterize(ch, px);
            self.cache.insert(key, (bitmap, metrics));
        }
        let (bm, m) = self.cache.get(&key).unwrap();
        (bm.as_slice(), m)
    }
}

// ── Canvas ────────────────────────────────────────────────────────────────────

struct Canvas {
    w:      u32,
    h:      u32,
    pixels: Vec<u8>,
    scale:  f32,
}

impl Canvas {
    fn new(w: u32, h: u32, scale: f32) -> Self {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for i in (0..pixels.len()).step_by(4) {
            pixels[i]   = BG[0];
            pixels[i+1] = BG[1];
            pixels[i+2] = BG[2];
            pixels[i+3] = 0xff;
        }
        Canvas { w, h, pixels, scale }
    }

    fn fill_rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: [u8; 4]) {
        let x0 = ((x * self.scale).round() as i32).max(0) as u32;
        let y0 = ((y * self.scale).round() as i32).max(0) as u32;
        let x1 = (((x + width)  * self.scale).round() as i32).min(self.w as i32) as u32;
        let y1 = (((y + height) * self.scale).round() as i32).min(self.h as i32) as u32;
        for py in y0..y1 {
            for px in x0..x1 {
                let idx = ((py * self.w + px) * 4) as usize;
                self.pixels[idx]   = color[0];
                self.pixels[idx+1] = color[1];
                self.pixels[idx+2] = color[2];
                self.pixels[idx+3] = color[3];
            }
        }
    }

    fn draw_char(&mut self, ch: char, x: f32, y: f32, color: [u8; 4],
                 cache: &mut GlyphCache) {
        let px = FONT_SIZE * self.scale;
        let (bitmap, metrics) = cache.rasterize(ch, px);
        let gx = ((x * self.scale) as i32) + metrics.xmin;
        let gy = ((y * self.scale) as i32) - metrics.height as i32 - metrics.ymin;
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let alpha = bitmap[row * metrics.width + col] as f32 / 255.0;
                if alpha < 0.01 { continue; }
                let px_x = gx + col as i32;
                let px_y = gy + row as i32;
                if px_x < 0 || px_y < 0
                    || px_x >= self.w as i32 || px_y >= self.h as i32 { continue; }
                let idx = ((px_y as u32 * self.w + px_x as u32) * 4) as usize;
                let bg = [self.pixels[idx], self.pixels[idx+1], self.pixels[idx+2]];
                self.pixels[idx]   = lerp_u8(bg[0], color[0], alpha);
                self.pixels[idx+1] = lerp_u8(bg[1], color[1], alpha);
                self.pixels[idx+2] = lerp_u8(bg[2], color[2], alpha);
                self.pixels[idx+3] = 0xff;
            }
        }
    }

    fn draw_text(&mut self, text: &str, x: f32, y: f32, color: [u8; 4],
                 cache: &mut GlyphCache) -> f32 {
        let mut cx = x;
        for ch in text.chars() {
            let adv = self.char_advance(ch, cache);
            self.draw_char(ch, cx, y, color, cache);
            cx += adv;
        }
        cx - x
    }

    fn char_advance(&mut self, ch: char, cache: &mut GlyphCache) -> f32 {
        let px = FONT_SIZE * self.scale;
        let (_, metrics) = cache.rasterize(ch, px);
        metrics.advance_width / self.scale
    }

    fn text_width(&mut self, text: &str, cache: &mut GlyphCache) -> f32 {
        text.chars().map(|c| self.char_advance(c, cache)).sum()
    }

    fn clip_text<'a>(&mut self, text: &'a str, max_w: f32, cache: &mut GlyphCache)
        -> std::borrow::Cow<'a, str>
    {
        if self.text_width(text, cache) <= max_w {
            return std::borrow::Cow::Borrowed(text);
        }
        let ew = self.char_advance('…', cache);
        let mut w = 0.0f32;
        let mut end = 0;
        for ch in text.chars() {
            let adv = self.char_advance(ch, cache);
            if w + adv + ew > max_w { break; }
            w += adv;
            end += ch.len_utf8();
        }
        std::borrow::Cow::Owned(format!("{}…", &text[..end]))
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

// ── File tree ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Entry {
    name:   String,
    path:   PathBuf,
    is_dir: bool,
    depth:  usize,
    open:   bool,
}

fn load_dir(root: &std::path::Path, depth: usize, open_dirs: &HashSet<PathBuf>)
    -> Vec<Entry>
{
    let mut entries = Vec::new();
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return entries,
    };
    paths.sort_by(|a, b| {
        let ad = a.is_dir();
        let bd = b.is_dir();
        if ad != bd { return bd.cmp(&ad); }
        let an = a.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        let bn = b.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        an.cmp(&bn)
    });
    for path in paths {
        let name = path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if name.starts_with('.') { continue; }
        let is_dir = path.is_dir();
        let open = is_dir && open_dirs.contains(&path);
        entries.push(Entry { name: name.clone(), path: path.clone(), is_dir, depth, open });
        if open {
            entries.extend(load_dir(&path, depth + 1, open_dirs));
        }
    }
    entries
}

// ── Context menu ──────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
enum CtxAction {
    OpenFile,
    Rename,
    Delete,
    NewFile,
    NewFolder,
}

#[derive(Clone)]
struct CtxItem {
    label:  &'static str,
    action: CtxAction,
    danger: bool,
}

fn ctx_menu_items(is_dir: bool) -> Vec<CtxItem> {
    let mut items = Vec::new();
    if !is_dir {
        items.push(CtxItem { label: "Open",        action: CtxAction::OpenFile,  danger: false });
    }
    items.push(CtxItem { label: "Rename",           action: CtxAction::Rename,    danger: false });
    items.push(CtxItem { label: "New File",          action: CtxAction::NewFile,   danger: false });
    items.push(CtxItem { label: "New Folder",        action: CtxAction::NewFolder, danger: false });
    items.push(CtxItem { label: "Delete",            action: CtxAction::Delete,    danger: true  });
    items
}

// ── Rename / new-entry input ──────────────────────────────────────────────────

#[derive(PartialEq)]
enum RenameMode {
    Rename,
    NewFile,
    NewFolder,
}

struct RenameState {
    path:           PathBuf,
    mode:           RenameMode,
    text:           String,
    cursor:         usize,
    blink:          Instant,
    cursor_visible: bool,
}

impl RenameState {
    fn new(path: PathBuf, mode: RenameMode, initial: &str) -> Self {
        let text   = initial.to_string();
        let cursor = text.len();
        RenameState { path, mode, text, cursor, blink: Instant::now(), cursor_visible: true }
    }

    fn tick_blink(&mut self) -> bool {
        if self.blink.elapsed() >= Duration::from_millis(500) {
            self.cursor_visible = !self.cursor_visible;
            self.blink = Instant::now();
            true
        } else {
            false
        }
    }

    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.cursor_visible = true;
        self.blink = Instant::now();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices().last().map(|(i, _)| i).unwrap_or(0);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
        self.cursor_visible = true;
        self.blink = Instant::now();
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.text.len() {
            let ch_len = self.text[self.cursor..].chars().next()
                .map(|c| c.len_utf8()).unwrap_or(0);
            self.text.drain(self.cursor..self.cursor + ch_len);
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices().last().map(|(i, _)| i).unwrap_or(0);
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor += self.text[self.cursor..].chars().next()
                .map(|c| c.len_utf8()).unwrap_or(0);
        }
    }

    fn move_home(&mut self) { self.cursor = 0; }
    fn move_end(&mut self)  { self.cursor = self.text.len(); }
}

// ── Context menu state ────────────────────────────────────────────────────────

struct CtxMenu {
    x:         f32,
    y:         f32,
    items:     Vec<CtxItem>,
    entry_idx: usize,
    hover:     Option<usize>,
}

// ── Main plugin state ─────────────────────────────────────────────────────────

struct Plugin {
    root:              PathBuf,
    open_dirs:         HashSet<PathBuf>,
    entries:           Vec<Entry>,
    scroll:            f32,
    width:             f32,
    height:            f32,
    scale:             f32,
    hover_row:         Option<usize>,
    sel_row:           Option<usize>,
    ctx:               Option<CtxMenu>,
    rename:            Option<RenameState>,
    drag_start_y:      Option<f32>,
    drag_scroll_start: f32,
    font:              Arc<Mutex<Font>>,
}

impl Plugin {
    fn new(root: PathBuf, scale: f32, font: Arc<Mutex<Font>>) -> Self {
        let mut p = Plugin {
            root,
            open_dirs: HashSet::new(),
            entries: Vec::new(),
            scroll: 0.0,
            width: 300.0,
            height: 400.0,
            scale,
            hover_row: None,
            sel_row: None,
            ctx: None,
            rename: None,
            drag_start_y: None,
            drag_scroll_start: 0.0,
            font,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        let root = self.root.clone();
        self.entries = load_dir(&root, 0, &self.open_dirs);
    }

    fn logical_to_row(&self, y: f32) -> Option<usize> {
        let rel = y + self.scroll;
        if rel < 0.0 { return None; }
        let row = (rel / ROW_H) as usize;
        if row < self.entries.len() { Some(row) } else { None }
    }

    fn row_y(&self, row: usize) -> f32 {
        row as f32 * ROW_H - self.scroll
    }

    fn max_scroll(&self) -> f32 {
        (self.entries.len() as f32 * ROW_H - self.height + ROW_H).max(0.0)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    // ── Render ────────────────────────────────────────────────────────────────

    fn render(&self) {
        let phys_w = (self.width  * self.scale).round() as u32;
        let phys_h = (self.height * self.scale).round() as u32;
        if phys_w == 0 || phys_h == 0 { return; }

        let font = self.font.lock().unwrap().clone();
        let mut cache = GlyphCache::new(font);
        let mut cv = Canvas::new(phys_w, phys_h, self.scale);

        let first_row = (self.scroll / ROW_H).floor() as usize;
        let last_row  = (((self.scroll + self.height) / ROW_H).ceil() as usize)
            .min(self.entries.len());

        for row in first_row..last_row {
            let entry = &self.entries[row];
            let ry = self.row_y(row);
            if ry + ROW_H < 0.0 || ry > self.height { continue; }

            let bg = if Some(row) == self.sel_row { BG_SEL }
                     else if Some(row) == self.hover_row { BG_HOVER }
                     else { BG };
            cv.fill_rect(0.0, ry, self.width, ROW_H, bg);

            let baseline = ry + ROW_H - (ROW_H - FONT_SIZE) / 2.0 - 2.0;
            let indent   = PADDING_L + entry.depth as f32 * INDENT_W;

            // Chevron
            if entry.is_dir {
                let ch = if entry.open { "\u{F107}" } else { "\u{F105}" };
                cv.draw_text(ch, indent, baseline, FG_DIM, &mut cache);
            }

            // Icon
            let (icon, icon_col) = if entry.is_dir {
                if entry.open {
                    ("\u{F07C}", FG_DIR)
                } else {
                    ("\u{F07B}", FG_DIR)
                }
            } else {
                file_icon(&entry.name)
            };
            cv.draw_text(icon, indent + ICON_W, baseline, icon_col, &mut cache);

            // Name
            let name_x   = indent + ICON_W * 2.0 + 2.0;
            let name_max = self.width - name_x - PADDING_L;
            let clipped  = cv.clip_text(&entry.name, name_max, &mut cache);
            cv.draw_text(&clipped, name_x, baseline,
                         if entry.is_dir { FG_DIR } else { FG }, &mut cache);
        }

        // Empty state
        if self.entries.is_empty() && self.rename.is_none() {
            let msg = "Empty directory";
            let mw  = cv.text_width(msg, &mut cache);
            cv.draw_text(msg, (self.width - mw) / 2.0, self.height / 2.0,
                         FG_DIM, &mut cache);
        }

        // ── Rename input ──
        if let Some(ref rename) = self.rename {
            let inp_y = self.height - ROW_H * 2.0;
            let inp_x = PADDING_L;
            let inp_w = self.width - PADDING_L * 2.0;
            let inp_h = ROW_H + 4.0;

            cv.fill_rect(inp_x - 1.0, inp_y - 1.0, inp_w + 2.0, inp_h + 2.0, BORDER_RENAME);
            cv.fill_rect(inp_x, inp_y, inp_w, inp_h, BG_RENAME);

            let baseline = inp_y + inp_h - (inp_h - FONT_SIZE) / 2.0 - 3.0;
            let label = match rename.mode {
                RenameMode::Rename    => "Rename: ",
                RenameMode::NewFile   => "New file: ",
                RenameMode::NewFolder => "New folder: ",
            };
            let lw = cv.draw_text(label, inp_x + 4.0, baseline, FG_DIM, &mut cache);

            let text_x    = inp_x + 4.0 + lw;
            let available = inp_w - lw - 8.0;
            let cursor_w  = cv.text_width(&rename.text[..rename.cursor], &mut cache);
            let text_scroll = if cursor_w > available - 10.0 {
                cursor_w - (available - 10.0)
            } else {
                0.0
            };

            let vis_start = if text_scroll > 0.0 {
                let mut w = 0.0f32;
                let mut s = 0usize;
                for (i, ch) in rename.text.char_indices() {
                    let adv = cv.char_advance(ch, &mut cache);
                    if w + adv > text_scroll { s = i; break; }
                    w += adv;
                    s = i + ch.len_utf8();
                }
                s
            } else {
                0
            };
            let visible_text = &rename.text[vis_start..];
            let clipped_inp  = cv.clip_text(visible_text, available, &mut cache);
            cv.draw_text(&clipped_inp, text_x, baseline, FG_RENAME, &mut cache);

            if rename.cursor_visible {
                let cx = text_x + (cursor_w - text_scroll).max(0.0);
                cv.fill_rect(cx, inp_y + 3.0, 1.5, inp_h - 6.0, CURSOR_COL);
            }
        }

        // ── Context menu ──
        if let Some(ref ctx) = self.ctx {
            let item_h = ROW_H;
            let menu_w = 130.0f32;
            let menu_h = ctx.items.len() as f32 * item_h + 4.0;
            let mx = ctx.x.min(self.width  - menu_w - 4.0).max(0.0);
            let my = ctx.y.min(self.height - menu_h - 4.0).max(0.0);

            cv.fill_rect(mx - 1.0, my - 1.0, menu_w + 2.0, menu_h + 2.0, FG_DIM);
            cv.fill_rect(mx, my, menu_w, menu_h, BG_CTX);

            for (i, item) in ctx.items.iter().enumerate() {
                let iy = my + 2.0 + i as f32 * item_h;
                if ctx.hover == Some(i) {
                    cv.fill_rect(mx, iy, menu_w, item_h, BG_CTX_SEL);
                }
                let col      = if item.danger { FG_DANGER } else { FG };
                let baseline = iy + item_h - (item_h - FONT_SIZE) / 2.0 - 2.0;
                let clipped  = cv.clip_text(item.label, menu_w - 16.0, &mut cache);
                cv.draw_text(&clipped, mx + 8.0, baseline, col, &mut cache);
            }
        }

        write_frame(phys_w, phys_h, &cv.pixels);
    }

    // ── Input handlers ────────────────────────────────────────────────────────

    fn on_resize(&mut self, phys_w: f32, phys_h: f32, scale: f32) {
        self.scale  = scale;
        self.width  = phys_w / scale;
        self.height = phys_h / scale;
        self.clamp_scroll();
        self.render();
    }

    fn on_scroll(&mut self, delta: f32) {
        self.scroll = (self.scroll - delta * 3.0).clamp(0.0, self.max_scroll());
        self.render();
    }

    fn on_mouse_move(&mut self, lx: f32, ly: f32) {
        // Context menu hover
        if let Some(ref mut ctx) = self.ctx {
            let item_h = ROW_H;
            let menu_w = 130.0f32;
            let menu_h = ctx.items.len() as f32 * item_h + 4.0;
            let mx = ctx.x.min(self.width  - menu_w - 4.0).max(0.0);
            let my = ctx.y.min(self.height - menu_h - 4.0).max(0.0);
            let new_hover = if lx >= mx && lx < mx + menu_w && ly >= my && ly < my + menu_h {
                let i = ((ly - my - 2.0) / item_h) as usize;
                if i < ctx.items.len() { Some(i) } else { None }
            } else {
                None
            };
            if new_hover != ctx.hover {
                ctx.hover = new_hover;
                self.render();
            }
            return;
        }

        // Drag-to-scroll
        if let Some(sy) = self.drag_start_y {
            let dy = sy - ly;
            self.scroll = (self.drag_scroll_start + dy).clamp(0.0, self.max_scroll());
            self.render();
            return;
        }

        let new_hover = self.logical_to_row(ly);
        if new_hover != self.hover_row {
            self.hover_row = new_hover;
            self.render();
        }
        let _ = lx;
    }

    fn on_mouse_press(&mut self, _lx: f32, ly: f32) {
        if self.ctx.is_some() { return; }
        self.drag_start_y      = Some(ly);
        self.drag_scroll_start = self.scroll;
    }

    fn on_mouse_release(&mut self) {
        self.drag_start_y = None;
    }

    /// Returns a file path to open if the click triggered an open-file action.
    fn on_click(&mut self, lx: f32, ly: f32) -> Option<String> {
        // ── Context menu ──
        if let Some(ctx) = self.ctx.take() {
            let item_h = ROW_H;
            let menu_w = 130.0f32;
            let menu_h = ctx.items.len() as f32 * item_h + 4.0;
            let mx = ctx.x.min(self.width  - menu_w - 4.0).max(0.0);
            let my = ctx.y.min(self.height - menu_h - 4.0).max(0.0);
            if lx >= mx && lx < mx + menu_w && ly >= my && ly < my + menu_h {
                let i = ((ly - my - 2.0) / item_h) as usize;
                if i < ctx.items.len() {
                    let action     = ctx.items[i].action.clone();
                    let entry_idx  = ctx.entry_idx;
                    return self.dispatch_ctx_action(&action, entry_idx);
                }
            }
            self.render();
            return None;
        }

        // ── Rename input ──
        if self.rename.is_some() {
            let inp_y = self.height - ROW_H * 2.0;
            let inp_h = ROW_H + 4.0;
            if !(ly >= inp_y && ly <= inp_y + inp_h) {
                self.rename = None;
                self.render();
            }
            return None;
        }

        // ── Tree row ──
        let Some(row) = self.logical_to_row(ly) else {
            self.sel_row = None;
            self.render();
            return None;
        };
        self.sel_row = Some(row);
        let entry = self.entries[row].clone();
        if entry.is_dir {
            if self.open_dirs.contains(&entry.path) {
                self.open_dirs.remove(&entry.path);
            } else {
                self.open_dirs.insert(entry.path.clone());
            }
            self.reload();
            self.clamp_scroll();
        } else {
            self.render();
            return Some(entry.path.to_string_lossy().to_string());
        }
        self.render();
        let _ = lx;
        None
    }

    fn on_right_click(&mut self, lx: f32, ly: f32) {
        self.rename = None;
        let (entry_idx, items) = if let Some(row) = self.logical_to_row(ly) {
            (row, ctx_menu_items(self.entries[row].is_dir))
        } else {
            (0usize.max(self.entries.len().saturating_sub(1)),
             vec![
                CtxItem { label: "New File",   action: CtxAction::NewFile,   danger: false },
                CtxItem { label: "New Folder", action: CtxAction::NewFolder, danger: false },
             ])
        };
        self.ctx = Some(CtxMenu { x: lx, y: ly, items, entry_idx, hover: None });
        self.render();
    }

    fn dispatch_ctx_action(&mut self, action: &CtxAction, entry_idx: usize)
        -> Option<String>
    {
        let entry = self.entries.get(entry_idx).cloned().unwrap_or_else(|| Entry {
            name: String::new(), path: self.root.clone(),
            is_dir: true, depth: 0, open: false,
        });
        match action {
            CtxAction::OpenFile => {
                let path = entry.path.to_string_lossy().to_string();
                self.render();
                return Some(path);
            }
            CtxAction::Rename => {
                self.rename = Some(RenameState::new(
                    entry.path.clone(), RenameMode::Rename, &entry.name));
            }
            CtxAction::Delete => {
                let result = if entry.is_dir {
                    std::fs::remove_dir_all(&entry.path)
                } else {
                    std::fs::remove_file(&entry.path)
                };
                match result {
                    Ok(_) => {
                        self.open_dirs.remove(&entry.path);
                        self.sel_row = None;
                        self.reload();
                    }
                    Err(e) => mact_notify("File Manager", &format!("Delete failed: {e}")),
                }
            }
            CtxAction::NewFile => {
                let dir = if entry.is_dir { entry.path.clone() }
                          else { entry.path.parent().unwrap_or(&self.root).to_path_buf() };
                self.rename = Some(RenameState::new(dir, RenameMode::NewFile, ""));
            }
            CtxAction::NewFolder => {
                let dir = if entry.is_dir { entry.path.clone() }
                          else { entry.path.parent().unwrap_or(&self.root).to_path_buf() };
                self.rename = Some(RenameState::new(dir, RenameMode::NewFolder, ""));
            }
        }
        self.render();
        None
    }

    fn commit_rename(&mut self) {
        let name = match self.rename.as_ref() {
            Some(r) => r.text.trim().to_string(),
            None    => return,
        };
        if name.is_empty() {
            self.rename = None;
            self.render();
            return;
        }
        let (path, mode) = {
            let r = self.rename.as_ref().unwrap();
            (r.path.clone(), match r.mode {
                RenameMode::Rename    => 0u8,
                RenameMode::NewFile   => 1u8,
                RenameMode::NewFolder => 2u8,
            })
        };
        let result = match mode {
            0 => {
                let new_path = path.parent().unwrap_or(&self.root).join(&name);
                std::fs::rename(&path, &new_path)
            }
            1 => {
                let new_path = path.join(&name);
                std::fs::File::create(&new_path).map(|_| ())
            }
            _ => {
                let new_path = path.join(&name);
                std::fs::create_dir_all(&new_path)
            }
        };
        match result {
            Ok(_) => {
                if mode != 0 {
                    self.open_dirs.insert(path);
                }
                self.reload();
            }
            Err(e) => mact_notify("File Manager", &format!("Operation failed: {e}")),
        }
        self.rename = None;
        self.render();
    }

    fn on_key(&mut self, text: &str, ctrl: bool, _meta: bool, _alt: bool, _shift: bool)
        -> Option<String>
    {
        // ── Rename input ──
        if self.rename.is_some() {
            match text {
                "\r" | "\n" => {
                    self.commit_rename();
                    return None;
                }
                "\u{001B}" => {
                    self.rename = None;
                    self.render();
                    return None;
                }
                "\u{0008}" | "\u{007F}" => {
                    self.rename.as_mut().unwrap().backspace();
                    self.render();
                    return None;
                }
                "\u{F702}" => { // Left arrow (Slint)
                    self.rename.as_mut().unwrap().move_left();
                    self.render();
                    return None;
                }
                "\u{F703}" => { // Right arrow
                    self.rename.as_mut().unwrap().move_right();
                    self.render();
                    return None;
                }
                "\u{F729}" => { // Home
                    self.rename.as_mut().unwrap().move_home();
                    self.render();
                    return None;
                }
                "\u{F72B}" => { // End
                    self.rename.as_mut().unwrap().move_end();
                    self.render();
                    return None;
                }
                "\u{F728}" => { // Delete (forward)
                    self.rename.as_mut().unwrap().delete_forward();
                    self.render();
                    return None;
                }
                s if ctrl => {
                    match s {
                        "a" => { self.rename.as_mut().unwrap().move_home(); }
                        "e" => { self.rename.as_mut().unwrap().move_end(); }
                        "k" => {
                            let r = self.rename.as_mut().unwrap();
                            r.text.truncate(r.cursor);
                        }
                        "u" => {
                            let r = self.rename.as_mut().unwrap();
                            r.text.drain(..r.cursor);
                            r.cursor = 0;
                        }
                        _ => {}
                    }
                    self.render();
                    return None;
                }
                s if !s.is_empty() && s.chars().all(|c| !c.is_control()) => {
                    self.rename.as_mut().unwrap().insert(s);
                    self.render();
                    return None;
                }
                _ => return None,
            }
        }

        // ── Tree navigation ──
        match text {
            "\u{F700}" => { // Up arrow
                if let Some(row) = self.sel_row {
                    if row > 0 {
                        self.sel_row = Some(row - 1);
                        let ry = self.row_y(row - 1);
                        if ry < 0.0 { self.scroll += ry; self.clamp_scroll(); }
                        self.render();
                    }
                } else if !self.entries.is_empty() {
                    self.sel_row = Some(0);
                    self.render();
                }
            }
            "\u{F701}" => { // Down arrow
                let next = self.sel_row.map(|r| r + 1).unwrap_or(0);
                if next < self.entries.len() {
                    self.sel_row = Some(next);
                    let ry = self.row_y(next);
                    if ry + ROW_H > self.height {
                        self.scroll += ry + ROW_H - self.height;
                        self.clamp_scroll();
                    }
                    self.render();
                }
            }
            "\r" | "\n" => {
                if let Some(row) = self.sel_row {
                    if row < self.entries.len() {
                        let entry = self.entries[row].clone();
                        if entry.is_dir {
                            if self.open_dirs.contains(&entry.path) {
                                self.open_dirs.remove(&entry.path);
                            } else {
                                self.open_dirs.insert(entry.path.clone());
                            }
                            self.reload();
                            self.clamp_scroll();
                            self.render();
                        } else {
                            let path = entry.path.to_string_lossy().to_string();
                            self.render();
                            return Some(path);
                        }
                    }
                }
            }
            "\u{001B}" => {
                if self.ctx.is_some() {
                    self.ctx = None;
                    self.render();
                }
            }
            // ── File operations (tree mode, no modifier) ──────────────────
            "r" | "\u{F705}" if !ctrl => {
                // r / F2 → rename selected entry
                if let Some(row) = self.sel_row {
                    if row < self.entries.len() {
                        let e = self.entries[row].clone();
                        self.rename = Some(RenameState::new(
                            e.path.clone(), RenameMode::Rename, &e.name));
                        self.render();
                    }
                }
            }
            "n" if !ctrl => {
                // n → new file in parent of selected (or root)
                let dir = self.sel_row
                    .and_then(|row| self.entries.get(row))
                    .map(|e| if e.is_dir { e.path.clone() }
                               else { e.path.parent().unwrap_or(&self.root).to_path_buf() })
                    .unwrap_or_else(|| self.root.clone());
                self.rename = Some(RenameState::new(dir, RenameMode::NewFile, ""));
                self.render();
            }
            "N" if !ctrl => {
                // N (Shift+N) → new folder
                let dir = self.sel_row
                    .and_then(|row| self.entries.get(row))
                    .map(|e| if e.is_dir { e.path.clone() }
                               else { e.path.parent().unwrap_or(&self.root).to_path_buf() })
                    .unwrap_or_else(|| self.root.clone());
                self.rename = Some(RenameState::new(dir, RenameMode::NewFolder, ""));
                self.render();
            }
            "\u{F728}" | "\u{007F}" if !ctrl => {
                // Delete/Backspace → delete selected entry
                if let Some(row) = self.sel_row {
                    if row < self.entries.len() {
                        let e = self.entries[row].clone();
                        let result = if e.is_dir {
                            std::fs::remove_dir_all(&e.path)
                        } else {
                            std::fs::remove_file(&e.path)
                        };
                        match result {
                            Ok(_) => {
                                self.open_dirs.remove(&e.path);
                                self.sel_row = None;
                                self.reload();
                            }
                            Err(err) => mact_notify("File Manager",
                                &format!("Delete failed: {err}")),
                        }
                        self.render();
                    }
                }
            }
            "r" if ctrl => {
                self.reload();
                self.render();
            }
            _ => {}
        }
        None
    }

    fn on_chdir(&mut self, path: &str) {
        let new_root = PathBuf::from(path);
        if new_root != self.root {
            self.root = new_root;
            self.open_dirs.clear();
            self.sel_row = None;
            self.scroll  = 0.0;
            self.reload();
            self.render();
        }
    }
}

// ── File icon helper ──────────────────────────────────────────────────────────
//
// Returns (nerd-font glyph, RGBA colour).  All codepoints verified present in
// HackNerdFontMono-Regular.ttf via cmap scan.

fn file_icon(name: &str) -> (&'static str, [u8; 4]) {
    // ── palette ───────────────────────────────────────────────────────────────
    const RUST:     [u8; 4] = [0xE3, 0x7A, 0x2F, 0xFF]; // rust orange
    const JS:       [u8; 4] = [0xF0, 0xDB, 0x4F, 0xFF]; // javascript yellow
    const TS:       [u8; 4] = [0x5C, 0x9A, 0xE6, 0xFF]; // typescript blue
    const REACT:    [u8; 4] = [0x61, 0xD4, 0xF5, 0xFF]; // react cyan
    const HTML:     [u8; 4] = [0xE3, 0x65, 0x26, 0xFF]; // html5 orange-red
    const CSS:      [u8; 4] = [0x56, 0x4F, 0xDD, 0xFF]; // css purple-blue
    const SCSS:     [u8; 4] = [0xCC, 0x6A, 0x99, 0xFF]; // scss pink
    const PY:       [u8; 4] = [0x4E, 0xA0, 0xD6, 0xFF]; // python blue
    const RUBY:     [u8; 4] = [0xCC, 0x34, 0x2B, 0xFF]; // ruby red
    const GO:       [u8; 4] = [0x00, 0xBC, 0xD4, 0xFF]; // go cyan
    const C_COL:    [u8; 4] = [0x64, 0x9A, 0xD3, 0xFF]; // c/c++ steel-blue
    const JAVA:     [u8; 4] = [0xE7, 0x6F, 0x00, 0xFF]; // java orange
    const KOTLIN:   [u8; 4] = [0x9C, 0x6C, 0xF6, 0xFF]; // kotlin purple
    const SWIFT:    [u8; 4] = [0xF0, 0x50, 0x28, 0xFF]; // swift red-orange
    const LUA:      [u8; 4] = [0x51, 0xA0, 0xCF, 0xFF]; // lua blue
    const SHELL:    [u8; 4] = [0x4E, 0xC9, 0x4E, 0xFF]; // shell green
    const MD:       [u8; 4] = [0xA0, 0xB8, 0xD8, 0xFF]; // markdown muted-blue
    const JSON:     [u8; 4] = [0xF5, 0xC5, 0x18, 0xFF]; // json gold
    const CONFIG:   [u8; 4] = [0x8A, 0xB4, 0x8E, 0xFF]; // toml/yaml sage-green
    const IMAGE:    [u8; 4] = [0xB8, 0x86, 0xD0, 0xFF]; // image purple
    const PDF:      [u8; 4] = [0xE5, 0x44, 0x44, 0xFF]; // pdf red
    const ARCHIVE:  [u8; 4] = [0xA8, 0x80, 0x55, 0xFF]; // archive tan
    const DOCKER:   [u8; 4] = [0x00, 0x8B, 0xE5, 0xFF]; // docker blue
    const DB:       [u8; 4] = [0xE8, 0x8D, 0x2D, 0xFF]; // database amber
    const DIM:      [u8; 4] = [0x77, 0x7A, 0x99, 0xFF]; // dim (lock/env/etc.)
    const FG_DEF:   [u8; 4] = [0xCC, 0xCC, 0xCC, 0xFF]; // default fg

    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();

    // Special filenames first (before extension matching)
    let lower = name.to_lowercase();
    if lower == "dockerfile" || lower.starts_with("dockerfile.") {
        return ("\u{F308}", DOCKER); // nf-logos-docker
    }
    if lower == "makefile" || lower == "gnumakefile" {
        return ("\u{F085}", CONFIG); // fa-cogs
    }
    if lower == ".gitignore" || lower == ".gitattributes" || lower == ".gitmodules" {
        return ("\u{E711}", DIM); // nf-dev-git
    }
    if lower == "cargo.toml" || lower == "cargo.lock" {
        return ("\u{E7A8}", RUST); // nf-dev-rust
    }
    if lower == "package.json" || lower == "package-lock.json" {
        return ("\u{E718}", JS); // nf-dev-node
    }
    if lower == "tsconfig.json" {
        return ("\u{E628}", TS); // nf-seti-typescript
    }

    match ext.as_str() {
        // ── Systems / compiled ────────────────────────────────────────────────
        "rs"                                              => ("\u{E7A8}", RUST),
        "c"                                               => ("\u{E61E}", C_COL),
        "h"                                               => ("\u{E61F}", C_COL),
        "cpp" | "cc" | "cxx"                              => ("\u{E61D}", C_COL),
        "hpp" | "hxx"                                     => ("\u{E61D}", C_COL),
        "go"                                              => ("\u{E724}", GO),
        "swift"                                           => ("\u{E755}", SWIFT),
        "java"                                            => ("\u{E738}", JAVA),
        "kt" | "kts"                                      => ("\u{E634}", KOTLIN),
        "cs"                                              => ("\u{E61C}", C_COL),
        "lua"                                             => ("\u{E7B7}", LUA),
        "hs" | "lhs"                                      => ("\u{E6AA}", LUA),
        // ── Web / scripting ───────────────────────────────────────────────────
        "js" | "mjs" | "cjs"                              => ("\u{E60C}", JS),
        "ts"                                              => ("\u{E628}", TS),
        "jsx"                                             => ("\u{E625}", REACT),
        "tsx"                                             => ("\u{E625}", REACT),
        "html" | "htm"                                    => ("\u{F13B}", HTML),
        "css"                                             => ("\u{E749}", CSS),
        "scss"                                            => ("\u{E603}", SCSS),
        "sass"                                            => ("\u{E621}", SCSS),
        "less"                                            => ("\u{E619}", CSS),
        // ── Scripting / interpreted ───────────────────────────────────────────
        "py" | "pyw" | "pyi"                              => ("\u{E73C}", PY),
        "rb"                                              => ("\u{E739}", RUBY),
        "sh" | "bash"                                     => ("\u{E795}", SHELL),
        "zsh"                                             => ("\u{E795}", SHELL),
        "fish"                                            => ("\u{E796}", SHELL),
        // ── Config / data ─────────────────────────────────────────────────────
        "toml"                                            => ("\u{E615}", CONFIG),
        "yaml" | "yml"                                    => ("\u{E617}", CONFIG),
        "json"                                            => ("\u{E60B}", JSON),
        "xml"                                             => ("\u{F05A}", CONFIG),
        "env"                                             => ("\u{E7AD}", DIM),
        "lock"                                            => ("\u{F023}", DIM),
        "ini" | "cfg" | "conf"                            => ("\u{F013}", CONFIG),
        // ── Docs ──────────────────────────────────────────────────────────────
        "md" | "mdx"                                      => ("\u{E73E}", MD),
        "txt"                                             => ("\u{F0F6}", FG_DEF),
        "pdf"                                             => ("\u{F1C1}", PDF),
        "doc" | "docx"                                    => ("\u{F1C4}", TS),
        "xls" | "xlsx"                                    => ("\u{F1C2}", SHELL),
        "ppt" | "pptx"                                    => ("\u{F1C3}", JAVA),
        // ── Media ─────────────────────────────────────────────────────────────
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"  => ("\u{F1C5}", IMAGE),
        "svg"                                             => ("\u{F1C5}", REACT),
        "ico"                                             => ("\u{F1C5}", IMAGE),
        "mp3" | "wav" | "ogg" | "flac" | "m4a"           => ("\u{F1C8}", IMAGE),
        "mp4" | "mov" | "avi" | "mkv" | "webm"           => ("\u{F1C9}", REACT),
        // ── Archives ─────────────────────────────────────────────────────────
        "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" => ("\u{F1C6}", ARCHIVE),
        // ── Database ─────────────────────────────────────────────────────────
        "sql"                                             => ("\u{F1C0}", DB),
        "db" | "sqlite" | "sqlite3"                       => ("\u{E7C4}", DB),
        // ── Fallback ──────────────────────────────────────────────────────────
        _                                                 => ("\u{F15B}", FG_DEF),
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let font = Font::from_bytes(FONT_BYTES, FontSettings::default())
        .expect("failed to load font");
    let font = Arc::new(Mutex::new(font));

    let root = std::env::var("MADO_PROJECT_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
        });

    let scale = std::env::var("MADO_SCALE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.0)
        .max(0.1);

    let plugin = Arc::new(Mutex::new(Plugin::new(root, scale, Arc::clone(&font))));

    // Blink thread for rename cursor
    {
        let plugin = Arc::clone(&plugin);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(100));
                let mut p = plugin.lock().unwrap();
                if let Some(ref mut rename) = p.rename {
                    if rename.tick_blink() {
                        p.render();
                    }
                }
            }
        });
    }

    // Initial render
    plugin.lock().unwrap().render();

    // Event loop
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        let v: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        let event_type = v["type"].as_str().unwrap_or("");
        let mut p = plugin.lock().unwrap();

        match event_type {
            "resize" => {
                // Events arrive in physical pixels; scale starts at 1.0 until changed
                let phys_w = v["width"].as_f64().unwrap_or(300.0) as f32;
                let phys_h = v["height"].as_f64().unwrap_or(400.0) as f32;
                let scale  = p.scale; // keep current scale on plain resize
                p.on_resize(phys_w, phys_h, scale);
            }
            "click" => {
                let phys_x = v["x"].as_f64().unwrap_or(0.0) as f32;
                let phys_y = v["y"].as_f64().unwrap_or(0.0) as f32;
                let button = v["button"].as_str().unwrap_or("left");
                let lx = phys_x / p.scale;
                let ly = phys_y / p.scale;
                if button == "right" {
                    p.on_right_click(lx, ly);
                } else if let Some(path) = p.on_click(lx, ly) {
                    drop(p);
                    mact_open_file(&path);
                    continue;
                }
            }
            "mouse_press" => {
                let lx = v["x"].as_f64().unwrap_or(0.0) as f32 / p.scale;
                let ly = v["y"].as_f64().unwrap_or(0.0) as f32 / p.scale;
                p.on_mouse_press(lx, ly);
            }
            "mouse_move" => {
                let lx = v["x"].as_f64().unwrap_or(0.0) as f32 / p.scale;
                let ly = v["y"].as_f64().unwrap_or(0.0) as f32 / p.scale;
                p.on_mouse_move(lx, ly);
            }
            "mouse_release" => { p.on_mouse_release(); }
            "scroll" => {
                let delta = v["delta"].as_f64().unwrap_or(0.0) as f32;
                p.on_scroll(delta);
            }
            "key" => {
                let text  = v["text"].as_str().unwrap_or("");
                let ctrl  = v["ctrl"].as_bool().unwrap_or(false);
                let meta  = v["meta"].as_bool().unwrap_or(false);
                let alt   = v["alt"].as_bool().unwrap_or(false);
                let shift = v["shift"].as_bool().unwrap_or(false);
                // Modifier-only filter (Slint private codepoints U+0010–U+0019)
                if text.len() == 1 {
                    let c = text.chars().next().unwrap();
                    if ('\u{0010}'..='\u{0019}').contains(&c) { continue; }
                }
                if let Some(path) = p.on_key(text, ctrl, meta, alt, shift) {
                    drop(p);
                    mact_open_file(&path);
                    continue;
                }
            }
            "focus" | "blur" => { p.render(); }
            "chdir" => {
                let path = v["path"].as_str().unwrap_or("");
                if !path.is_empty() { p.on_chdir(path); }
            }
            "workspace" => {} // chdir is sent separately on workspace switch
            _ => {}
        }
    }
}
