//! The normal window ("通常表示"): the transcript on the left over the whole height, the status /
//! 目次 / 設定 sidebar on the right. Hand-made controls in the C# app's dark look.
//!
//! Drawn the way the overlay is: DirectWrite + a Direct2D DC render target bound to a DIB of our
//! own, which WM_PAINT then BitBlts to the window. Nothing that is out of sight is ever laid out:
//! the two lists measure only the rows they put on the screen, so 5000 lines cost what 20 do.
use std::{collections::HashMap, ffi::c_void, mem::size_of};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::{Common::*, *}, DirectWrite::*, Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, Gdi::*},
    },
};

use crate::{app::Cmd, core::Settings, model::Model};

// Measures at 100% scaling, as in the C# MainWindow.
const SIDE_W: f32 = 300.0;
const METER_W: f32 = 268.0;
const SB_W: f32 = 12.0; // the dark scroll bar
const TOPICS_MIN: f32 = 110.0;
pub const MIN_W: f32 = 720.0;
pub const MIN_H: f32 = 460.0;

// Palette, from Model.cs.
const BG: u32 = 0x0B0D11;
const PANEL: u32 = 0x141820;
const FG: u32 = 0xE9EDF2;
const DIM: u32 = 0x9AA4B2;
const LINE: u32 = 0x242A33;
const ACCENT: u32 = 0x4DA3FF;
const BUTTON: u32 = 0x1D2430;
const BTN_BORDER: u32 = 0x2D3542;
pub const GOOD: u32 = 0x5CD68A;
pub const WARN: u32 = 0xE8B34D;
pub const BAD: u32 = 0xE86B6B;
const THUMB: u32 = 0x3A4350;

const JUMP: &str = "↓ 最新へ";
const NO_TOPICS: &str = "話題が出るとここに並びます";
const EXPANDER: &str = "透過モードの設定";

/// What the sidebar's status block shows; the controller fills it in on every redraw.
pub struct Status {
    pub text: String, pub dot: u32, pub level: f32, pub lag: String, pub models: String,
    /// The Chrome extension: 1 = a tab's audio is coming in, 0 = not, -1 = its port could not be opened.
    pub tab: i32,
    pub paused: bool,
}

/// Which drop-down list a click opened. The controller fills and shows it (TrackPopupMenu), so
/// that the list of running apps is as fresh as the moment it was opened.
#[derive(Clone, Copy, PartialEq)]
pub enum Pick { Lang, Source }

/// What a click, a drag or a wheel turn came to.
#[derive(PartialEq)]
pub enum Act { None, Redraw, Cmd(Cmd), Menu(Pick, RECT) }

/// What `compose` drew again, for WM_PAINT to copy: the transcript (or the whole window when the
/// sidebar was laid out anew), and the status block when its texts alone have changed.
pub struct Dirty { pub rect: RECT, pub head: Option<RECT> }

#[derive(Clone, Copy, PartialEq)]
enum Slide { Ja, En, Bg }

impl Slide {
    fn cmd(self, v: i32) -> Cmd {
        match self { Slide::Ja => Cmd::JaSize(v), Slide::En => Cmd::EnSize(v), Slide::Bg => Cmd::Bg(v) }
    }
}

enum Kind {
    Cap(u32),          // a caption or a note, in that colour
    Note(u32, bool),   // the Chrome line: colour, underlined
    Btn(Cmd),
    Chk(bool, Cmd),
    Sld(i32, i32, i32, Slide), // value, lowest, highest
    Cmb(Pick, bool),           // which list, usable
    Sep,
    Exp(bool),
}

/// One laid-out control. `y` is in the panel it belongs to; the drawing adds the panel's top.
/// `text` is kept because DirectWrite does not hand a layout's text back, and the checks name
/// the controls by it.
struct Row { id: u32, y: f32, h: f32, x: f32, w: f32, text: String, lay: Option<IDWriteTextLayout>, kind: Kind }

impl Row {
    fn hit(&self, top: f32, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= top + self.y && y < top + self.y + self.h
    }
}

/// The status block: everything in it keeps its place, so the level meter can be redrawn alone.
#[derive(Default)]
struct Head {
    dot: D2D_RECT_F, meter: D2D_RECT_F,
    text: Option<IDWriteTextLayout>, text_at: (f32, f32),
    lag: Option<IDWriteTextLayout>, lag_at: (f32, f32),
    models: Option<IDWriteTextLayout>, models_at: (f32, f32),
}

/// A measured transcript line: the layouts stay for as long as the line is on the screen.
struct TRow { key: String, time: Option<IDWriteTextLayout>, ja: Option<IDWriteTextLayout>, en: Option<IDWriteTextLayout>,
              ja_y: f32, en_y: f32, h: f32, pass: u32 }

/// A measured 目次 row.
struct KRow { key: String, time: Option<IDWriteTextLayout>, title: Option<IDWriteTextLayout>, sum: Option<IDWriteTextLayout>,
              sum_y: f32, h: f32, pass: u32 }

/// Where a list stands. The anchor is the uid of the first row on show, so trimming the head of
/// the history does not move what the user is looking at.
#[derive(Clone, Default)]
struct Scroll {
    follow: bool, anchor: u64, off: f32,
    first: usize, shown: usize, total: usize, avg: f32, over: bool,
}

impl Scroll {
    fn new() -> Scroll { Scroll { follow: true, avg: 40.0, ..Default::default() } }
}

/// Which slider or thumb the mouse is holding on to.
#[derive(Clone, Copy, PartialEq)]
enum Grab { None, Slider(u32), Transcript, Topics, Settings }

pub struct MainWin {
    pub hwnd: HWND,
    pub w: i32, pub h: i32, pub scale: f32,
    dw: IDWriteFactory,
    memdc: HDC, dib: HBITMAP, bits: *mut c_void,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    formats: HashMap<(bool, u32), IDWriteTextFormat>,
    trows: HashMap<u64, TRow>, tpass: u32,
    krows: HashMap<i64, KRow>, kpass: u32,
    // what was laid out, and from what
    key: String,
    head: Head,
    head_key: String, // the status block's own: its texts change with every sentence
    head_bottom: f32, // where the rest of the sidebar starts, which is all the block says to it
    btns: Vec<Row>,
    sets: Vec<Row>,
    set_h: f32, set_sb: bool, set_off: f32,
    topics_cap: Option<IDWriteTextLayout>, topics_cap_at: (f32, f32),
    empty: Option<IDWriteTextLayout>,
    jump: Option<IDWriteTextLayout>,
    r_trans: D2D_RECT_F, r_topics: D2D_RECT_F, r_set: D2D_RECT_F, r_jump: D2D_RECT_F, r_head: D2D_RECT_F,
    tr: Scroll, tp: Scroll,
    hover: u32, pressed: u32, grab: Grab,
    full: bool, // the sidebar has changed too, not only the transcript
    /// The label of the chosen sound source; the controller keeps it in step with the list.
    pub src_label: String,
    pub expanded: bool,
    pub shown: usize,
    pub rows_built: u64,
}

fn rgba(c: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: ((c >> 16) & 255) as f32 / 255.0, g: ((c >> 8) & 255) as f32 / 255.0, b: (c & 255) as f32 / 255.0, a }
}

fn rc(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F { D2D_RECT_F { left: l, top: t, right: r, bottom: b } }

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

pub unsafe fn surface(w: i32, h: i32) -> Result<(HDC, HBITMAP, *mut c_void)> {
    let memdc = CreateCompatibleDC(None);
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = match CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(d) => d,
        Err(e) => { let _ = DeleteDC(memdc); return Err(e); } // the DC would be left behind otherwise
    };
    SelectObject(memdc, HGDIOBJ(dib.0));
    Ok((memdc, dib, bits))
}

/// The DC and the bitmap of a surface nobody owns yet, let go of after a later step failed.
unsafe fn drop_surface(memdc: HDC, dib: HBITMAP) {
    let _ = DeleteDC(memdc);
    let _ = DeleteObject(HGDIOBJ(dib.0));
}

impl MainWin {
    /// The factories are the overlay's: both views draw with the same DirectWrite and Direct2D.
    pub unsafe fn new(hwnd: HWND, w: i32, h: i32, scale: f32, d2d: &ID2D1Factory, dw: &IDWriteFactory) -> Result<MainWin> {
        let (w, h) = (w.max(1), h.max(1));
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
            dpiX: 96.0, dpiY: 96.0, // everything in here is in pixels
            ..Default::default()
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        let (memdc, dib, bits) = surface(w, h)?;
        // until the window holds them, every way out has to let the DC and the bitmap go
        if let Err(e) = rt.BindDC(memdc, &RECT { left: 0, top: 0, right: w, bottom: h }) { drop_surface(memdc, dib); return Err(e); }
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let brush = match rt.CreateSolidColorBrush(&rgba(0, 1.0), None) {
            Ok(b) => b,
            Err(e) => { drop_surface(memdc, dib); return Err(e); }
        };
        Ok(MainWin {
            hwnd, w, h, scale, dw: dw.clone(), memdc, dib, bits, rt, brush,
            formats: HashMap::new(), trows: HashMap::new(), tpass: 0, krows: HashMap::new(), kpass: 0,
            key: String::new(), head: Head::default(), head_key: String::new(), head_bottom: 0.0,
            btns: Vec::new(), sets: Vec::new(),
            set_h: 0.0, set_sb: false, set_off: 0.0, topics_cap: None, topics_cap_at: (0.0, 0.0),
            empty: None, jump: None,
            r_trans: rc(0.0, 0.0, 0.0, 0.0), r_topics: rc(0.0, 0.0, 0.0, 0.0), r_set: rc(0.0, 0.0, 0.0, 0.0),
            r_jump: rc(0.0, 0.0, 0.0, 0.0), r_head: rc(0.0, 0.0, 0.0, 0.0),
            tr: Scroll::new(), tp: Scroll::new(), hover: 0, pressed: 0, grab: Grab::None, full: true,
            src_label: String::new(), expanded: false, shown: 0, rows_built: 0,
        })
    }

    pub unsafe fn resize(&mut self, w: i32, h: i32) -> Result<()> {
        let (w, h) = (w.max(1), h.max(1));
        if w == self.w && h == self.h { return Ok(()); }
        let (memdc, dib, bits) = surface(w, h)?;
        // the old surface is kept when the new one cannot be bound; the new one goes
        if let Err(e) = self.rt.BindDC(memdc, &RECT { left: 0, top: 0, right: w, bottom: h }) { drop_surface(memdc, dib); return Err(e); }
        drop_surface(self.memdc, self.dib);
        (self.memdc, self.dib, self.bits, self.w, self.h) = (memdc, dib, bits, w, h);
        self.key.clear(); // the whole layout hangs off the size
        self.full = true;
        Ok(())
    }

    /// Lets go of the picture and of the laid-out rows, while the window is hidden. The next
    /// `resize` brings the picture back.
    pub unsafe fn release(&mut self) -> Result<()> {
        self.trows.clear();
        self.krows.clear();
        self.resize(1, 1)
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 0.001 {
            self.scale = scale;
            self.formats.clear();
            self.trows.clear();
            self.krows.clear();
            self.key.clear();
            self.full = true;
        }
    }

    pub fn px(&self, v: f32) -> f32 { (v * self.scale).round() }

    /// The window's picture as it was last composed: BGRA, top row first.
    pub fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits as *const u8, (self.w * self.h * 4) as usize) }
    }

    pub fn following(&self) -> bool { self.tr.follow }
    pub fn top_row(&self) -> usize { self.tr.first }

    /// Back to the newest line (a new stream, or the "↓ 最新へ" button).
    pub fn show_newest(&mut self) {
        (self.tr.follow, self.tr.anchor, self.tr.off, self.tr.first) = (true, 0, 0.0, 0);
        (self.tp.follow, self.tp.off, self.tp.first) = (true, 0.0, 0);
    }

    /// The sidebar has to be laid out again although nothing in the settings changed.
    pub fn key_dirty(&mut self) { self.key.clear(); self.full = true; }

    /// Shows the end of the 設定 panel (what the folded-away overlay settings are at).
    pub fn settings_to_end(&mut self) { self.set_off = (self.sets_h() - self.set_h).max(0.0); self.full = true; }

    // ---- text

    unsafe fn format(&mut self, bold: bool, size: f32) -> Result<IDWriteTextFormat> {
        let px = size * self.scale;
        let key = (bold, (px * 100.0) as u32);
        if let Some(f) = self.formats.get(&key) { return Ok(f.clone()); }
        if self.formats.len() > 24 { self.formats.clear(); }
        let weight = if bold { DWRITE_FONT_WEIGHT_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
        let f = self.dw.CreateTextFormat(w!("Yu Gothic UI"), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, w!("ja-jp"))?;
        self.formats.insert(key, f.clone());
        Ok(f)
    }

    /// A laid-out text and its height. `size` and `max_w` are in pixels of this window.
    unsafe fn lay(&mut self, text: &str, bold: bool, size: f32, max_w: f32) -> Result<(IDWriteTextLayout, f32)> {
        let f = self.format(bold, size)?;
        let u: Vec<u16> = text.encode_utf16().collect();
        let l = self.dw.CreateTextLayout(&u, &f, max_w.max(10.0), 100000.0)?;
        let mut m = DWRITE_TEXT_METRICS::default();
        l.GetMetrics(&mut m)?;
        self.rows_built += 1;
        // An empty line still takes a line's height, as an empty WPF TextBlock does.
        Ok((l, m.height.max(m.lineCount.max(1) as f32 * 0.0).ceil()))
    }

    unsafe fn text_w(l: &IDWriteTextLayout) -> f32 {
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = l.GetMetrics(&mut m);
        m.width.ceil()
    }

    // ---- drawing helpers

    unsafe fn fill(&self, r: D2D_RECT_F, c: u32) {
        self.brush.SetColor(&rgba(c, 1.0));
        self.rt.FillRectangle(&r, &self.brush);
    }

    unsafe fn round(&self, r: D2D_RECT_F, rad: f32, c: u32) {
        self.brush.SetColor(&rgba(c, 1.0));
        self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: rad, radiusY: rad }, &self.brush);
    }

    unsafe fn outline(&self, r: D2D_RECT_F, rad: f32, c: u32, w: f32) {
        self.brush.SetColor(&rgba(c, 1.0));
        self.rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: rad, radiusY: rad }, &self.brush, w, None);
    }

    unsafe fn put(&self, l: &IDWriteTextLayout, x: f32, y: f32, c: u32) {
        self.brush.SetColor(&rgba(c, 1.0));
        self.rt.DrawTextLayout(D2D_POINT_2F { x, y }, l, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
    }

    unsafe fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, c: u32, w: f32) {
        self.brush.SetColor(&rgba(c, 1.0));
        self.rt.DrawLine(D2D_POINT_2F { x: x0, y: y0 }, D2D_POINT_2F { x: x1, y: y1 }, &self.brush, w, None);
    }

    /// The chevron of a drop-down or of the expander.
    unsafe fn chevron(&self, cx: f32, cy: f32, r: f32, c: u32) {
        self.line(cx - r, cy - r * 0.45, cx, cy + r * 0.45, c, self.px(1.3));
        self.line(cx, cy + r * 0.45, cx + r, cy - r * 0.45, c, self.px(1.3));
    }

    // ---------------------------------------------------------------------- layout

    /// The status block: its texts change with every sentence (the 遅延 does), so it is laid out
    /// on its own and everything below it hangs off its height alone. True when it has changed.
    unsafe fn build_head(&mut self, st: &Status) -> Result<bool> {
        let sc = self.scale;
        let px = move |v: f32| (v * sc).round();
        let w = self.w as f32;
        let side_w = px(SIDE_W);
        let x0 = (w - side_w).max(0.0);
        let (ml, mt, mr) = (px(13.0), px(12.0), px(12.0));
        let lx = x0 + ml;
        let inner = (side_w - ml - mr).max(10.0);
        let key = format!("{}x{}/{:.3}/{}/{}/{}/{}", self.w, self.h, self.scale, st.dot, st.text, st.lag, st.models);
        if key == self.head_key { return Ok(false); }

        // status block (StackPanel margin 13,12,12,8)
        let dot_sz = px(10.0);
        let tx = lx + dot_sz + px(8.0);
        let (text, text_h) = self.lay(&st.text, false, 12.0, (x0 + side_w - mr - tx).max(10.0))?;
        let head_h = text_h.max(dot_sz + px(4.0));
        let meter_y = mt + head_h + px(8.0);
        let meter = rc(lx, meter_y, lx + px(METER_W), meter_y + px(6.0));
        let lag_y = meter.bottom + px(6.0);
        let (lag, lag_h) = self.lay(&st.lag, false, 11.0, inner)?;
        let models_y = lag_y + lag_h + px(2.0);
        let (models, models_h) = self.lay(&st.models, false, 10.0, inner)?;
        self.head = Head {
            dot: rc(lx, mt + px(4.0), lx + dot_sz, mt + px(4.0) + dot_sz), meter,
            text: Some(text), text_at: (tx, mt),
            lag: Some(lag), lag_at: (lx, lag_y),
            models: Some(models), models_at: (lx, models_y),
        };
        self.head_bottom = models_y + models_h + px(8.0);
        self.r_head = rc(x0, 0.0, x0 + side_w, self.head_bottom);
        // The key only now: a layout that gave up halfway (a `?` above) must not pass for the
        // one it stands for, or the next sentence with the same status would be taken for
        // nothing to do and the stale block would stay.
        self.head_key = key;
        Ok(true)
    }

    /// Lays the sidebar out again when anything it shows has changed. The transcript and the
    /// 目次 are laid out row by row while they are drawn, so their length costs nothing.
    /// True when only the status block changed, which is then the only part drawn again.
    unsafe fn build(&mut self, model: &Model, s: &Settings, st: &Status) -> Result<bool> {
        let head_changed = self.build_head(st)?;
        // The status texts are not in this key: of them only the height matters here, and the
        // 遅延 changes with every sentence, which would lay the whole sidebar out again.
        let key = format!("{}x{}/{:.3}/{}/{}/{}/{}{}{}{}{}{}/{}/{}/{}/{}/{}/{}",
            self.w, self.h, self.scale, s.ja_size, s.en_size, s.text_mode(), s.always_on_top as u8, s.topics as u8,
            s.save_history as u8, s.click_through as u8, self.expanded as u8, st.paused as u8, s.bg_percent,
            self.src_label, st.tab, self.head_bottom,
            model.topics.len(), model.topics_rev);
        if key == self.key { return Ok(head_changed); }

        let sc = self.scale;
        let px = move |v: f32| (v * sc).round();
        let (w, h) = (self.w as f32, self.h as f32);
        let side_w = px(SIDE_W);
        let x0 = (w - side_w).max(0.0);
        let (ml, mr) = (px(13.0), px(12.0));
        let lx = x0 + ml;
        let inner = (side_w - ml - mr).max(10.0);
        let after_status = self.head_bottom;

        // the three buttons (grid margin 13,0,12,10)
        let pause_text = if st.paused { "再開" } else { "一時停止" };
        let (pause, btn_text_h) = self.lay(pause_text, false, 12.0, inner)?;
        let bh = btn_text_h + px(12.0); // padding 6 + 6
        let colw = ((inner - px(8.0)) / 2.0).floor();
        let (over, _) = self.lay("透過モード", false, 12.0, colw)?;
        let (fresh, _) = self.lay("配信を切り替え（字幕をクリア）", false, 12.0, inner)?;
        self.btns = vec![
            Row { id: 1, y: after_status, h: bh, x: lx, w: colw, text: pause_text.into(), lay: Some(pause), kind: Kind::Btn(Cmd::Pause) },
            Row { id: 2, y: after_status, h: bh, x: lx + colw + px(8.0), w: colw, text: "透過モード".into(), lay: Some(over), kind: Kind::Btn(Cmd::ShowOverlay) },
            Row { id: 3, y: after_status + bh + px(8.0), h: bh, x: lx, w: inner, text: "配信を切り替え（字幕をクリア）".into(), lay: Some(fresh), kind: Kind::Btn(Cmd::NewStream) },
        ];
        let after_btns = after_status + 2.0 * bh + px(8.0) + px(10.0);

        // 目次 caption (margin 13,0,12,4)
        let (cap, cap_h) = self.lay("目次", false, 11.0, inner)?;
        self.topics_cap = Some(cap);
        self.topics_cap_at = (lx, after_btns);
        let topics_top = after_btns + cap_h + px(4.0);

        // 設定: built twice at most, because needing a scroll bar makes it narrower
        let room = (h - topics_top - px(TOPICS_MIN)).max(0.0);
        for _ in 0..2 {
            let w_in = inner - if self.set_sb { px(SB_W) } else { 0.0 };
            let desired = self.settings_rows(s, st, lx, w_in.max(10.0))?;
            self.set_h = desired.min(room);
            let sb = desired > self.set_h + 0.5;
            if sb == self.set_sb { break; }
            self.set_sb = sb;
        }
        let set_top = h - self.set_h;
        self.r_trans = rc(0.0, 0.0, x0, h);
        self.r_topics = rc(x0, topics_top, x0 + side_w, set_top.max(topics_top));
        self.r_set = rc(x0, set_top, x0 + side_w, h);

        let (empty, _) = self.lay(NO_TOPICS, false, 11.0, (side_w - px(40.0)).max(10.0))?;
        self.empty = Some(empty);
        let (jump, jump_h) = self.lay(JUMP, false, 12.0, 4000.0)?;
        let (jw, jh) = (MainWin::text_w(&jump) + px(24.0), jump_h + px(12.0));
        self.r_jump = rc(x0 - px(22.0) - jw, h - px(18.0) - jh, x0 - px(22.0), h - px(18.0));
        self.jump = Some(jump);
        self.set_off = self.set_off.min((self.sets_h() - self.set_h).max(0.0));
        // The key only now, for the reason `build_head` gives: half a layout must not be taken
        // for the finished one.
        self.key = key;
        self.full = true;
        Ok(false) // the whole sidebar was laid out: all of it is drawn, status block and all
    }

    fn sets_h(&self) -> f32 { self.sets.last().map(|r| r.y + r.h).unwrap_or(0.0) + self.px(10.0) }

    /// The 設定 panel, top to bottom; the y of each row is inside the panel. Returns its height.
    unsafe fn settings_rows(&mut self, s: &Settings, st: &Status, lx: f32, w: f32) -> Result<f32> {
        let sc = self.scale;
        let px = move |v: f32| (v * sc).round();
        let mut rows: Vec<Row> = Vec::new();
        let mut y = px(6.0); // StackPanel margin 13,6,12,10
        let mut id = 10u32;

        macro_rules! cap {
            ($t:expr, $size:expr, $col:expr, $mb:expr) => {{
                let (l, hh) = self.lay($t, false, $size, w)?;
                rows.push(Row { id: 0, y, h: hh, x: lx, w, text: $t.into(), lay: Some(l), kind: Kind::Cap($col) });
                y += hh + px($mb);
            }};
        }
        macro_rules! check {
            ($t:expr, $on:expr, $cmd:expr) => {{
                let (l, hh) = self.lay($t, false, 11.0, w - px(20.0))?;
                id += 1;
                rows.push(Row { id, y, h: hh.max(px(14.0)), x: lx, w, text: $t.into(), lay: Some(l), kind: Kind::Chk($on, $cmd) });
                y += hh.max(px(14.0)) + px(6.0);
            }};
        }
        macro_rules! slider {
            ($v:expr, $lo:expr, $hi:expr, $k:expr) => {{
                id += 1;
                rows.push(Row { id, y, h: px(18.0), x: lx, w, text: String::new(), lay: None, kind: Kind::Sld($v, $lo, $hi, $k) });
                y += px(18.0) + px(6.0);
            }};
        }
        macro_rules! combo {
            ($t:expr, $pick:expr, $on:expr, $mt:expr, $mb:expr) => {{
                y += px($mt);
                let (l, hh) = self.lay($t, false, 11.0, w - px(26.0))?;
                let box_h = hh + px(8.0);
                id += 1;
                rows.push(Row { id, y, h: box_h, x: lx, w, text: $t.into(), lay: Some(l), kind: Kind::Cmb($pick, $on) });
                y += box_h + px($mb);
            }};
        }

        cap!("設定", 11.0, DIM, 2.0);
        cap!(&format!("文字サイズ（日本語） {}", s.ja_size), 11.0, DIM, 2.0);
        slider!(s.ja_size, 16, 80, Slide::Ja);
        cap!(&format!("文字サイズ（英語） {}", s.en_size), 11.0, DIM, 2.0);
        slider!(s.en_size, 10, 60, Slide::En);
        cap!("表示する言語", 11.0, DIM, 2.0);
        let lang = ["日本語のみ", "日本語＋英語", "英語のみ"][s.text_mode()];
        combo!(lang, Pick::Lang, true, 2.0, 8.0);
        check!("常に最前面", s.always_on_top, Cmd::AlwaysOnTop);
        check!("目次を作る", s.topics, Cmd::Topics);
        check!("履歴をファイルに保存", s.save_history, Cmd::SaveHistory);
        {
            let (l, hh) = self.lay("保存先を開く", false, 12.0, w)?;
            let bw = MainWin::text_w(&l) + px(20.0);
            id += 1;
            rows.push(Row { id, y, h: hh + px(12.0), x: lx, w: bw, text: "保存先を開く".into(), lay: Some(l), kind: Kind::Btn(Cmd::OpenHistory) });
            y += hh + px(12.0) + px(8.0);
        }
        cap!("音声の取得元", 11.0, DIM, 2.0);
        let src = if self.src_label.is_empty() { "PC の音すべて".to_string() } else { self.src_label.clone() };
        combo!(&src, Pick::Source, st.tab != 1, 2.0, 4.0);
        {
            let (text, col, under) = match st.tab {
                1 => ("Chrome 拡張機能で選んだタブの音を取得中", ACCENT, false),
                t if t < 0 => ("Chrome 拡張機能は使えません（ポートが使用中）", WARN, false),
                _ => ("Chrome のタブを 1 つだけ選ぶには（拡張機能）", DIM, true),
            };
            let (l, hh) = self.lay(text, false, 11.0, w)?;
            id += 1;
            rows.push(Row { id, y, h: hh, x: lx, w, text: text.into(), lay: Some(l), kind: Kind::Note(col, under) });
            y += hh + px(8.0);
        }
        y += px(2.0);
        rows.push(Row { id: 0, y, h: px(1.0), x: lx, w, text: String::new(), lay: None, kind: Kind::Sep });
        y += px(1.0) + px(6.0);
        {
            let (l, hh) = self.lay(EXPANDER, false, 11.0, w - px(24.0))?;
            id += 1;
            let hh = hh.max(px(18.0));
            rows.push(Row { id, y, h: hh, x: lx, w, text: EXPANDER.into(), lay: Some(l), kind: Kind::Exp(self.expanded) });
            y += hh;
        }
        if self.expanded {
            y += px(6.0);
            cap!(&format!("背景の濃さ {}", s.bg_percent), 11.0, DIM, 2.0);
            slider!(s.bg_percent, 0, 100, Slide::Bg);
            check!("クリック透過", s.click_through, Cmd::ClickThrough);
        }
        self.sets = rows;
        Ok(y + px(10.0))
    }

    // ----------------------------------------------------------------------- paint

    /// Draws the window again and says which part of it changed, for WM_PAINT to copy. While
    /// subtitles stream in and nothing else moves, that is the transcript alone, and the status
    /// block when its texts have changed with the sentence.
    pub unsafe fn compose(&mut self, model: &Model, s: &Settings, st: &Status) -> Result<Dirty> {
        let head = self.build(model, s, st)?;
        let full = self.full;
        self.full = false;
        self.rt.BeginDraw();
        if !full { self.rt.PushAxisAlignedClip(&rc(0.0, 0.0, self.r_trans.right + 1.0, self.h as f32), D2D1_ANTIALIAS_MODE_ALIASED); }
        self.fill(rc(0.0, 0.0, self.r_trans.right, self.h as f32), BG);
        self.draw_transcript(model, s)?;
        if full {
            self.fill(rc(self.r_trans.right, 0.0, self.w as f32, self.h as f32), PANEL);
            self.draw_head(st);
            self.draw_rows(&self.btns, 0.0)?;
            self.draw_topics(model)?;
            self.draw_settings()?;
        }
        // the 1 px divider, over the sidebar's left edge
        self.fill(rc(self.r_trans.right, 0.0, self.r_trans.right + 1.0, self.h as f32), LINE);
        if !full { self.rt.PopAxisAlignedClip(); }
        // only the status texts changed: the block alone is drawn again, as the meter is.
        // From one pixel in: the divider drawn just above runs down the sidebar's left edge,
        // which is the block's own left edge, and filling over it would take it away beside
        // the block after every sentence until the next full layout.
        if !full && head {
            let r = rc(self.r_head.left + 1.0, self.r_head.top, self.r_head.right, self.r_head.bottom);
            self.rt.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED);
            self.fill(r, PANEL);
            self.draw_head(st);
            self.rt.PopAxisAlignedClip();
        }
        self.rt.EndDraw(None, None)?;
        let box_of = |r: D2D_RECT_F| RECT { left: r.left as i32, top: r.top as i32, right: r.right.ceil() as i32, bottom: r.bottom.ceil() as i32 };
        Ok(if full { Dirty { rect: RECT { left: 0, top: 0, right: self.w, bottom: self.h }, head: None } }
           else {
               Dirty { rect: RECT { left: 0, top: 0, right: (self.r_trans.right as i32 + 1).min(self.w), bottom: self.h },
                       head: if head { Some(box_of(self.r_head)) } else { None } }
           })
    }

    /// Only the level meter: it moves eight times a second and nothing else moves with it.
    pub unsafe fn draw_meter(&mut self, level: f32) -> Result<RECT> {
        let m = self.head.meter;
        self.rt.BeginDraw();
        self.rt.PushAxisAlignedClip(&m, D2D1_ANTIALIAS_MODE_ALIASED);
        let r = self.px(3.0);
        self.round(m, r, BUTTON);
        let v = level.clamp(0.0, 1.0);
        if v > 0.0 { self.round(rc(m.left, m.top, m.left + (m.right - m.left) * v, m.bottom), r, ACCENT); }
        self.rt.PopAxisAlignedClip();
        self.rt.EndDraw(None, None)?;
        Ok(RECT { left: m.left as i32, top: m.top as i32, right: m.right.ceil() as i32, bottom: m.bottom.ceil() as i32 })
    }

    unsafe fn draw_head(&self, st: &Status) {
        self.brush.SetColor(&rgba(st.dot, 1.0));
        let d = self.head.dot;
        let r = (d.right - d.left) / 2.0;
        self.rt.FillEllipse(&D2D1_ELLIPSE { point: D2D_POINT_2F { x: d.left + r, y: d.top + r }, radiusX: r, radiusY: r }, &self.brush);
        if let Some(l) = &self.head.text { self.put(l, self.head.text_at.0, self.head.text_at.1, FG); }
        let m = self.head.meter;
        let rad = self.px(3.0);
        self.round(m, rad, BUTTON);
        let v = st.level.clamp(0.0, 1.0);
        if v > 0.0 { self.round(rc(m.left, m.top, m.left + (m.right - m.left) * v, m.bottom), rad, ACCENT); }
        if let Some(l) = &self.head.lag { self.put(l, self.head.lag_at.0, self.head.lag_at.1, DIM); }
        if let Some(l) = &self.head.models { self.put(l, self.head.models_at.0, self.head.models_at.1, DIM); }
    }

    /// Draws a list of controls at `top` (the top of the panel they scroll in).
    unsafe fn draw_rows(&self, rows: &[Row], top: f32) -> Result<()> {
        for r in rows.iter() {
            let (x, y, w, h) = (r.x, top + r.y, r.w, r.h);
            match &r.kind {
                Kind::Cap(c) => { if let Some(l) = &r.lay { self.put(l, x, y, *c); } }
                Kind::Note(c, under) => {
                    if let Some(l) = &r.lay {
                        self.put(l, x, y, *c);
                        if *under { let b = y + h - self.px(2.0); self.line(x, b, x + MainWin::text_w(l), b, *c, self.px(1.0)); }
                    }
                }
                Kind::Btn(_) => {
                    let bg = if self.pressed == r.id { LINE } else if self.hover == r.id { BTN_BORDER } else { BUTTON };
                    let rad = self.px(6.0);
                    self.round(rc(x, y, x + w, y + h), rad, bg);
                    self.outline(rc(x + 0.5, y + 0.5, x + w - 0.5, y + h - 0.5), rad, BTN_BORDER, 1.0);
                    if let Some(l) = &r.lay {
                        let tw = MainWin::text_w(l);
                        self.put(l, (x + (w - tw) / 2.0).round(), (y + (h - MainWin::line_h(l)) / 2.0).round(), FG);
                    }
                }
                Kind::Chk(on, _) => {
                    let s = self.px(13.0);
                    let by = (y + (h - s) / 2.0).round();
                    let rad = self.px(2.0);
                    if *on {
                        self.round(rc(x, by, x + s, by + s), rad, ACCENT);
                        self.line(x + s * 0.24, by + s * 0.52, x + s * 0.44, by + s * 0.73, 0xFFFFFF, self.px(1.6));
                        self.line(x + s * 0.44, by + s * 0.73, x + s * 0.78, by + s * 0.28, 0xFFFFFF, self.px(1.6));
                    } else {
                        self.round(rc(x, by, x + s, by + s), rad, BUTTON);
                        self.outline(rc(x + 0.5, by + 0.5, x + s - 0.5, by + s - 0.5), rad, BTN_BORDER, 1.0);
                    }
                    if let Some(l) = &r.lay { self.put(l, x + s + self.px(7.0), y, FG); }
                }
                Kind::Sld(v, lo, hi, _) => {
                    let (tx, tw) = self.track(r);
                    let ty = (y + h / 2.0).round();
                    let th = self.px(4.0);
                    self.round(rc(tx, ty - th / 2.0, tx + tw, ty + th / 2.0), th / 2.0, BTN_BORDER);
                    let f = ((*v - *lo) as f32 / (*hi - *lo).max(1) as f32).clamp(0.0, 1.0);
                    if f > 0.0 { self.round(rc(tx, ty - th / 2.0, tx + tw * f, ty + th / 2.0), th / 2.0, ACCENT); }
                    let (kw, kh) = (self.px(9.0), self.px(16.0));
                    let kx = (tx + tw * f - kw / 2.0).round();
                    self.round(rc(kx, ty - kh / 2.0, kx + kw, ty + kh / 2.0), self.px(3.0), 0xC9D1DB);
                }
                Kind::Cmb(_, on) => {
                    let rad = self.px(4.0);
                    self.round(rc(x, y, x + w, y + h), rad, BUTTON);
                    self.outline(rc(x + 0.5, y + 0.5, x + w - 0.5, y + h - 0.5), rad, BTN_BORDER, 1.0);
                    if let Some(l) = &r.lay { self.put(l, x + self.px(7.0), (y + (h - MainWin::line_h(l)) / 2.0).round(), if *on { FG } else { DIM }); }
                    self.chevron(x + w - self.px(11.0), y + h / 2.0 - self.px(1.0), self.px(3.5), if *on { DIM } else { LINE });
                }
                Kind::Sep => self.fill(rc(x, y, x + w, y + h), LINE),
                Kind::Exp(open) => {
                    let d = self.px(16.0);
                    let cy = y + h / 2.0;
                    self.brush.SetColor(&rgba(BTN_BORDER, 1.0));
                    self.rt.FillEllipse(&D2D1_ELLIPSE { point: D2D_POINT_2F { x: x + d / 2.0, y: cy }, radiusX: d / 2.0, radiusY: d / 2.0 }, &self.brush);
                    if *open {
                        self.line(x + d * 0.28, cy + self.px(1.5), x + d / 2.0, cy - self.px(1.5), DIM, self.px(1.3));
                        self.line(x + d / 2.0, cy - self.px(1.5), x + d * 0.72, cy + self.px(1.5), DIM, self.px(1.3));
                    } else {
                        self.chevron(x + d / 2.0, cy - self.px(1.0), self.px(3.0), DIM);
                    }
                    if let Some(l) = &r.lay { self.put(l, x + d + self.px(7.0), (cy - MainWin::line_h(l) / 2.0).round(), DIM); }
                }
            }
        }
        Ok(())
    }

    unsafe fn line_h(l: &IDWriteTextLayout) -> f32 {
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = l.GetMetrics(&mut m);
        m.height
    }

    /// Where a slider's track runs inside its row.
    fn track(&self, r: &Row) -> (f32, f32) {
        let k = self.px(5.0); // half the knob, so the knob never leaves the row
        (r.x + k, (r.w - 2.0 * k).max(1.0))
    }

    // ---- the transcript

    unsafe fn draw_transcript(&mut self, model: &Model, s: &Settings) -> Result<()> {
        let r = self.r_trans;
        let (vh, n) = (r.bottom - r.top, model.items.len());
        let aw = (r.right - r.left - self.px(SB_W) - self.px(14.0) - self.px(16.0)).max(10.0);
        self.tpass = self.tpass.wrapping_add(1);

        // where the first row on show starts
        let (mut i, mut off) = if self.tr.follow {
            let (mut acc, mut idx, mut off) = (0.0f32, n, 0.0f32);
            while idx > 0 {
                let hh = self.row_h(model, idx - 1, s, aw)?;
                idx -= 1;
                if acc + hh >= vh { off = acc + hh - vh; acc += hh; break; }
                acc += hh;
            }
            if acc < vh { (0usize, 0.0f32) } else { (idx, off) }
        } else {
            (self.anchor_at(model), self.tr.off)
        };
        if i > n { i = n; }
        if i == 0 && n == 0 { off = 0.0; }
        self.tr.anchor = model.items.get(i).map(|it| it.uid).unwrap_or(0);
        self.tr.off = off;
        self.tr.first = i;

        self.rt.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED);
        let (mut y, mut shown, mut sum) = (r.top - off, 0usize, 0.0f32);
        while i < n && y < r.bottom {
            let hh = self.row_h(model, i, s, aw)?;
            let it = &model.items[i];
            let row = self.trows.get(&it.uid).unwrap();
            let (time, ja, en, ja_y, en_y) = (row.time.clone(), row.ja.clone(), row.en.clone(), row.ja_y, row.en_y);
            let x = r.left + self.px(14.0);
            if let Some(l) = &time { self.put(l, x, y + self.px(10.0), DIM); }
            if let Some(l) = &ja { self.put(l, x, y + ja_y, if it.dim { DIM } else { FG }); }
            if let Some(l) = &en {
                // English alone takes the main line's colours
                let c = if s.show_japanese { DIM } else if it.dim { DIM } else { FG };
                self.put(l, x, y + en_y, c);
            }
            y += hh;
            sum += hh;
            i += 1;
            shown += 1;
        }
        self.rt.PopAxisAlignedClip();
        let pass = self.tpass;
        self.trows.retain(|_, b| b.pass == pass);
        if shown > 0 { self.tr.avg = sum / shown as f32; }
        (self.tr.shown, self.tr.total) = (shown, n);
        self.tr.over = !(self.tr.first == 0 && off <= 0.0 && i >= n && y <= r.bottom);
        self.shown = shown;
        if self.tr.over { self.thumb(&self.tr.clone(), r); }
        if !self.tr.follow {
            let j = self.r_jump;
            let rad = (j.bottom - j.top) / 2.0;
            self.round(j, rad, BUTTON);
            self.outline(rc(j.left + 0.5, j.top + 0.5, j.right - 0.5, j.bottom - 0.5), rad, BTN_BORDER, 1.0);
            if let Some(l) = &self.jump {
                let tw = MainWin::text_w(l);
                self.put(l, (j.left + (j.right - j.left - tw) / 2.0).round(), (j.top + self.px(6.0)).round(), FG);
            }
        }
        Ok(())
    }

    /// The index of the row the view is anchored to, or 0 when it has been trimmed away.
    fn anchor_at(&self, model: &Model) -> usize {
        if self.tr.anchor == 0 { return 0; }
        model.items.iter().position(|i| i.uid == self.tr.anchor).unwrap_or(0)
    }

    /// Measures one transcript line (and keeps the measurement for as long as it is on show).
    unsafe fn row_h(&mut self, model: &Model, i: usize, s: &Settings, aw: f32) -> Result<f32> {
        let it = &model.items[i];
        let key = format!("{}/{}/{}/{}/{}/{}/{}/{}", it.ja, it.en, it.time, it.show_time as u8, s.ja_size, s.en_size, s.text_mode(), aw as i32);
        if let Some(r) = self.trows.get_mut(&it.uid) {
            if r.key == key { r.pass = self.tpass; return Ok(r.h); }
        }
        let (uid, show_time) = (it.uid, it.show_time);
        let (ja_text, en_text) = (it.ja.clone(), it.en.clone());
        let time_text = it.time.clone();
        let sc = self.scale;
        let px = move |v: f32| (v * sc).round();
        let mut y = px(2.0); // StackPanel margin 14,2,16,2
        let mut time = None;
        if show_time && !time_text.is_empty() {
            let (l, hh) = self.lay(&time_text, false, 11.0, aw)?;
            time = Some(l);
            y += px(8.0) + hh + px(2.0);
        }
        let mut ja = None;
        let mut ja_y = y;
        if s.show_japanese && !ja_text.is_empty() {
            let (l, hh) = self.lay(&ja_text, true, s.ja_size as f32, aw)?;
            ja = Some(l);
            ja_y = y;
            y += hh;
        }
        let mut en = None;
        let mut en_y = y;
        if s.show_english && !en_text.is_empty() {
            y += px(1.0);
            let (l, hh) = self.lay(&en_text, false, s.en_size as f32, aw)?;
            en = Some(l);
            en_y = y;
            y += hh;
        }
        let h = y + px(2.0);
        self.trows.insert(uid, TRow { key, time, ja, en, ja_y, en_y, h, pass: self.tpass });
        Ok(h)
    }

    // ---- 目次

    unsafe fn draw_topics(&mut self, model: &Model) -> Result<()> {
        if let Some(l) = &self.topics_cap { self.put(l, self.topics_cap_at.0, self.topics_cap_at.1, DIM); }
        let r = self.r_topics;
        let n = model.topics.len();
        if n == 0 {
            if let Some(l) = &self.empty {
                let tw = MainWin::text_w(l);
                let th = MainWin::line_h(l);
                self.put(l, (r.left + (r.right - r.left - tw) / 2.0).round(), (r.top + (r.bottom - r.top - th) / 2.0).round(), DIM);
            }
            (self.tp.shown, self.tp.total, self.tp.over) = (0, 0, false);
            return Ok(());
        }
        let aw = (r.right - r.left - self.px(SB_W) - self.px(13.0) - self.px(12.0)).max(10.0);
        let vh = r.bottom - r.top;
        self.kpass = self.kpass.wrapping_add(1);
        let (mut i, off) = if self.tp.follow {
            let (mut acc, mut idx, mut off) = (0.0f32, n, 0.0f32);
            while idx > 0 {
                let hh = self.topic_h(model, idx - 1, aw)?;
                idx -= 1;
                if acc + hh >= vh { off = acc + hh - vh; acc += hh; break; }
                acc += hh;
            }
            if acc < vh { (0usize, 0.0f32) } else { (idx, off) }
        } else {
            (self.tp.first.min(n.saturating_sub(1)), self.tp.off)
        };
        self.tp.first = i;
        self.tp.off = off;
        self.rt.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED);
        let (mut y, mut shown, mut sum) = (r.top - off, 0usize, 0.0f32);
        while i < n && y < r.bottom {
            let hh = self.topic_h(model, i, aw)?;
            let row = self.krows.get(&(i as i64)).unwrap();
            let (time, title, sumy, sl) = (row.time.clone(), row.title.clone(), row.sum_y, row.sum.clone());
            let x = r.left + self.px(13.0);
            if let Some(l) = &time { self.put(l, x, y + self.px(5.0), DIM); }
            let tx = x + self.px(32.0);
            if let Some(l) = &title { self.put(l, tx, y + self.px(3.0), FG); }
            if let Some(l) = &sl { self.put(l, tx, y + sumy, DIM); }
            y += hh;
            sum += hh;
            i += 1;
            shown += 1;
        }
        self.rt.PopAxisAlignedClip();
        let pass = self.kpass;
        self.krows.retain(|_, b| b.pass == pass);
        if shown > 0 { self.tp.avg = sum / shown as f32; }
        (self.tp.shown, self.tp.total) = (shown, n);
        self.tp.over = !(self.tp.first == 0 && off <= 0.0 && i >= n && y <= r.bottom);
        if self.tp.over { self.thumb(&self.tp.clone(), r); }
        Ok(())
    }

    /// 目次 rows are keyed by their place in the list: the engine rewrites them in place.
    unsafe fn topic_h(&mut self, model: &Model, i: usize, aw: f32) -> Result<f32> {
        let t = &model.topics[i];
        let key = format!("{}/{}/{}/{}", t.time, t.title, t.summary, aw as i32);
        if let Some(r) = self.krows.get_mut(&(i as i64)) {
            if r.key == key { r.pass = self.kpass; return Ok(r.h); }
        }
        let sc = self.scale;
        let px = move |v: f32| (v * sc).round();
        let body = (aw - px(32.0)).max(10.0);
        let (time, _) = self.lay(&t.time, false, 10.0, px(30.0))?;
        let (title, th) = self.lay(&t.title, true, 12.0, body)?;
        let mut y = px(3.0) + th;
        let mut sum = None;
        let mut sum_y = y;
        if !t.summary.is_empty() {
            y += px(1.0);
            let (l, sh) = self.lay(&t.summary, false, 11.0, body)?;
            sum = Some(l);
            sum_y = y;
            y += sh + px(3.0);
        }
        let h = y + px(3.0);
        self.krows.insert(i as i64, KRow { key, time: Some(time), title: Some(title), sum, sum_y, h, pass: self.kpass });
        Ok(h)
    }

    /// How tall the thumb of a `vh`-high bar over a list of `total` pixels is. The pointer →
    /// offset mapping needs the same height the drawing uses, or the thumb runs away from the
    /// pointer: it travels `vh - th`, not `vh`.
    fn thumb_h(&self, vh: f32, total: f32) -> f32 {
        (vh * (vh / total.max(1.0))).max(self.px(24.0))
    }

    /// Where a press or a drag at `fy` puts a bar: the fraction of what there is to scroll.
    fn bar_fraction(fy: f32, top: f32, vh: f32, th: f32) -> f32 {
        ((fy - top - th / 2.0) / (vh - th).max(1.0)).clamp(0.0, 1.0)
    }

    /// Where a list's thumb sits: its top and its height. One place for both the drawing and
    /// the drag that has to put it under the pointer.
    fn bar_thumb(&self, sc: &Scroll, area: D2D_RECT_F) -> (f32, f32) {
        let vh = area.bottom - area.top;
        let total = (sc.total as f32 * sc.avg).max(vh + 1.0);
        let pos = (sc.first as f32 * sc.avg + sc.off) / (total - vh);
        let th = self.thumb_h(vh, total);
        (area.top + (vh - th) * pos.clamp(0.0, 1.0), th)
    }

    /// The scroll bar: a rounded thumb over the list's right edge, as in the C# template.
    unsafe fn thumb(&self, sc: &Scroll, area: D2D_RECT_F) {
        let w = self.px(SB_W);
        let x = area.right - w;
        let (ty, th) = self.bar_thumb(sc, area);
        self.round(rc(x + self.px(3.0), ty + self.px(2.0), x + w - self.px(3.0), ty + th - self.px(2.0)), self.px(4.0), THUMB);
    }

    unsafe fn draw_settings(&mut self) -> Result<()> {
        let r = self.r_set;
        if r.bottom - r.top <= 0.0 { return Ok(()); }
        self.rt.PushAxisAlignedClip(&r, D2D1_ANTIALIAS_MODE_ALIASED);
        self.draw_rows(&self.sets, r.top - self.set_off)?;
        if let Some((ty, th)) = self.settings_thumb() {
            let w = self.px(SB_W);
            let x = r.right - w;
            self.round(rc(x + self.px(3.0), ty + self.px(2.0), x + w - self.px(3.0), ty + th - self.px(2.0)), self.px(4.0), THUMB);
        }
        self.rt.PopAxisAlignedClip();
        Ok(())
    }

    /// The bitmap the picture is composed in (the load test copies it the way WM_PAINT does).
    pub fn dc(&self) -> HDC { self.memdc }

    /// Hands the composed picture to the window, the part of it that WM_PAINT asked for.
    pub unsafe fn blit(&self, hdc: HDC, r: &RECT) {
        let (w, h) = ((r.right - r.left).min(self.w), (r.bottom - r.top).min(self.h));
        if w > 0 && h > 0 { let _ = BitBlt(hdc, r.left, r.top, w, h, self.memdc, r.left, r.top, SRCCOPY); }
    }

    // ------------------------------------------------------------------ the mouse

    /// What a left click at (x, y) in the window does. `down` is the press, else the release.
    pub unsafe fn click(&mut self, x: i32, y: i32, down: bool, model: &Model) -> Act {
        let (fx, fy) = (x as f32, y as f32);
        if !down {
            let was = self.pressed;
            self.pressed = 0;
            self.grab = Grab::None;
            if was == 0 { return Act::None; }
            self.full = true; // the control no longer looks pressed
            // the command belongs to the control the press started on, and only if it is still under the mouse
            for r in self.btns.iter().chain(self.sets.iter()) {
                if r.id != was { continue; }
                let top = if self.btns.iter().any(|b| b.id == was) { 0.0 } else { self.r_set.top - self.set_off };
                if !r.hit(top, fx, fy) { return Act::Redraw; }
                return match &r.kind {
                    Kind::Btn(c) => Act::Cmd(c.clone()),
                    Kind::Chk(_, c) => Act::Cmd(c.clone()),
                    Kind::Note(_, _) => Act::Cmd(Cmd::OpenExtension),
                    Kind::Exp(_) => { self.expanded = !self.expanded; self.key.clear(); Act::Redraw }
                    Kind::Cmb(p, on) => {
                        if !*on { Act::Redraw } else {
                            Act::Menu(*p, RECT { left: r.x as i32, top: (top + r.y) as i32, right: (r.x + r.w) as i32, bottom: (top + r.y + r.h) as i32 })
                        }
                    }
                    _ => Act::Redraw,
                };
            }
            return Act::Redraw;
        }

        // the "↓ 最新へ" button sits over the transcript
        if !self.tr.follow && inside(&self.r_jump, fx, fy) {
            self.tr.follow = true;
            return Act::Redraw;
        }
        if inside(&self.r_trans, fx, fy) {
            if self.tr.over && fx >= self.r_trans.right - self.px(SB_W) {
                self.grab = Grab::Transcript;
                self.scroll_to_fraction(model, fy);
                return Act::Redraw;
            }
            return Act::None; // the transcript itself has nothing to click
        }
        if inside(&self.r_topics, fx, fy) {
            if self.tp.over && fx >= self.r_topics.right - self.px(SB_W) {
                self.grab = Grab::Topics;
                self.full = true;
                return Act::Redraw;
            }
            if let Some(i) = self.topic_at(fy) {
                if let Some(t) = model.topics.get(i) {
                    if let Some(idx) = model.topic_target(t) { self.show_line(model, idx); }
                }
            }
            return Act::Redraw;
        }
        for i in 0..self.btns.len() {
            if self.btns[i].hit(0.0, fx, fy) { self.pressed = self.btns[i].id; self.full = true; return Act::Redraw; }
        }
        if inside(&self.r_set, fx, fy) {
            if self.set_sb && fx >= self.r_set.right - self.px(SB_W) {
                self.grab = Grab::Settings;
                self.scroll_settings(fy);
                return Act::Redraw;
            }
            let top = self.r_set.top - self.set_off;
            for i in 0..self.sets.len() {
                if !self.sets[i].hit(top, fx, fy) { continue; }
                if let Kind::Sld(_, lo, hi, k) = self.sets[i].kind {
                    let id = self.sets[i].id;
                    self.grab = Grab::Slider(id);
                    self.pressed = id;
                    self.full = true;
                    return Act::Cmd(k.cmd(self.slider_value(i, fx, lo, hi)));
                }
                self.pressed = self.sets[i].id;
                self.full = true;
                return Act::Redraw;
            }
        }
        Act::None
    }

    /// A move with the button held, or a plain move (hover).
    pub unsafe fn mouse_move(&mut self, x: i32, y: i32, model: &Model) -> Act {
        let (fx, fy) = (x as f32, y as f32);
        match self.grab {
            Grab::Slider(id) => {
                if let Some(i) = self.sets.iter().position(|r| r.id == id) {
                    if let Kind::Sld(v, lo, hi, k) = self.sets[i].kind {
                        let nv = self.slider_value(i, fx, lo, hi);
                        if nv != v { return Act::Cmd(k.cmd(nv)); }
                    }
                }
                return Act::None;
            }
            Grab::Transcript => { self.scroll_to_fraction(model, fy); return Act::Redraw; }
            Grab::Topics => {
                self.full = true;
                let r = self.r_topics;
                let f = ((fy - r.top) / (r.bottom - r.top).max(1.0)).clamp(0.0, 1.0);
                self.tp.follow = f >= 0.999;
                self.tp.first = ((model.topics.len() as f32 * f) as usize).min(model.topics.len().saturating_sub(1));
                self.tp.off = 0.0;
                return Act::Redraw;
            }
            Grab::Settings => { self.scroll_settings(fy); return Act::Redraw; }
            Grab::None => {}
        }
        let mut over = 0u32;
        for r in self.btns.iter() { if r.hit(0.0, fx, fy) { over = r.id; } }
        if inside(&self.r_set, fx, fy) {
            let top = self.r_set.top - self.set_off;
            for r in self.sets.iter() { if r.hit(top, fx, fy) { over = r.id; } }
        }
        if over == self.hover { return Act::None; }
        self.hover = over;
        self.full = true;
        Act::Redraw
    }

    /// The pointer left the window. A drag goes on: the mouse is held, and a slider is meant to
    /// follow it past the window's edge until the button is let go.
    pub fn leave(&mut self) -> Act {
        if self.grab != Grab::None { return Act::None; }
        if self.hover == 0 && self.pressed == 0 { return Act::None; }
        (self.hover, self.pressed) = (0, 0);
        self.full = true;
        Act::Redraw
    }

    /// The mouse was taken away from the window (Alt+Tab, another window): the drag is over.
    pub fn lost_capture(&mut self) -> Act {
        if self.grab == Grab::None { return Act::None; }
        self.grab = Grab::None;
        (self.hover, self.pressed) = (0, 0);
        self.full = true;
        Act::Redraw
    }

    /// True while a drag is in progress, so the window knows to hold the mouse.
    pub fn dragging(&self) -> bool { self.grab != Grab::None }

    /// Is the pointer over something that the hand cursor belongs on?
    pub fn is_hot(&self) -> bool { self.hover != 0 }

    fn slider_value(&self, i: usize, fx: f32, lo: i32, hi: i32) -> i32 {
        let (tx, tw) = self.track(&self.sets[i]);
        let f = ((fx - tx) / tw.max(1.0)).clamp(0.0, 1.0);
        lo + (f * (hi - lo) as f32).round() as i32
    }

    /// The wheel: Ctrl zooms the Japanese text, as in the C# window; otherwise it scrolls
    /// whichever list the pointer is over.
    pub unsafe fn wheel(&mut self, x: i32, y: i32, delta: i32, ctrl: bool, model: &Model, s: &Settings) -> Act {
        let (fx, fy) = (x as f32, y as f32);
        let notches = delta as f32 / 120.0;
        if ctrl && inside(&self.r_trans, fx, fy) {
            if delta == 0 { return Act::None; }
            let v = (s.ja_size + if delta > 0 { 2 } else { -2 }).clamp(16, 80);
            return if v == s.ja_size { Act::None } else { Act::Cmd(Cmd::JaSize(v)) };
        }
        let step = self.px(3.0 * 36.0); // three lines a notch, as a list scrolls in Windows
        if inside(&self.r_trans, fx, fy) {
            let d = -notches * step;
            if let Err(e) = self.scroll_transcript(model, s, d) { crate::core::log(&format!("scroll failed: {e:?}")); }
            return Act::Redraw;
        }
        if inside(&self.r_topics, fx, fy) {
            if notches > 0.0 {
                self.tp.follow = false;
                self.tp.first = self.tp.first.saturating_sub(notches as usize + 1);
            } else if self.tp.first + self.tp.shown < self.tp.total {
                self.tp.first += (-notches) as usize + 1;
            } else {
                self.tp.follow = true;
            }
            self.tp.off = 0.0;
            self.full = true;
            return Act::Redraw;
        }
        if inside(&self.r_set, fx, fy) && self.set_sb {
            let max = (self.sets_h() - self.set_h).max(0.0);
            let was = self.set_off;
            self.set_off = (self.set_off - notches * step).clamp(0.0, max);
            if was == self.set_off { return Act::None; }
            self.full = true;
            return Act::Redraw;
        }
        Act::None
    }

    /// Moves the transcript by `d` pixels (positive = towards the newest line) and works out
    /// whether that put it back at the bottom, which arms auto-follow again.
    unsafe fn scroll_transcript(&mut self, model: &Model, s: &Settings, d: f32) -> Result<()> {
        let r = self.r_trans;
        let aw = (r.right - r.left - self.px(SB_W) - self.px(14.0) - self.px(16.0)).max(10.0);
        let n = model.items.len();
        if n == 0 { return Ok(()); }
        let (mut i, mut off) = (self.anchor_at(model).min(n - 1), self.tr.off);
        if d >= 0.0 {
            let mut left = d;
            while left > 0.0 {
                let hh = self.row_h(model, i, s, aw)?;
                if i + 1 >= n {
                    // the last row can be taller than the view (big text, small window): its
                    // lower part has to be reachable, down to its bottom and no further
                    off = (off + left).min((hh - (r.bottom - r.top)).max(0.0));
                    break;
                }
                let room = hh - off;
                if left < room { off += left; break; }
                left -= room;
                off = 0.0;
                i += 1;
            }
        } else {
            let mut left = -d;
            while left > 0.0 {
                if off >= left { off -= left; break; }
                left -= off;
                if i == 0 { off = 0.0; break; }
                i -= 1;
                off = self.row_h(model, i, s, aw)?;
            }
        }
        self.tr.anchor = model.items[i].uid;
        self.tr.off = off;
        self.tr.first = i;
        // at the bottom? walk forward over at most a screenful
        let (mut y, mut j) = (-off, i);
        let vh = r.bottom - r.top;
        while j < n && y < vh { y += self.row_h(model, j, s, aw)?; j += 1; }
        self.tr.follow = j >= n && y <= vh + 1.0;
        Ok(())
    }

    /// The 設定 panel's thumb was dragged to `fy`: its middle goes under the pointer, which is
    /// what the drawing above puts it at for the offset this works out.
    fn scroll_settings(&mut self, fy: f32) {
        let (r, vh) = (self.r_set, self.set_h);
        let th = self.thumb_h(vh, self.sets_h());
        let f = MainWin::bar_fraction(fy, r.top, vh, th);
        self.set_off = f * (self.sets_h() - self.set_h).max(0.0);
        self.full = true;
    }

    /// The thumb was dragged to `fy`: jump straight there, without measuring what is skipped.
    fn scroll_to_fraction(&mut self, model: &Model, fy: f32) {
        let r = self.r_trans;
        let (_, th) = self.bar_thumb(&self.tr, r);
        let f = MainWin::bar_fraction(fy, r.top, r.bottom - r.top, th);
        let n = model.items.len();
        if n == 0 { return; }
        if f >= 0.999 { self.tr.follow = true; return; }
        self.tr.follow = false;
        let i = ((n as f32 * f) as usize).min(n - 1);
        self.tr.anchor = model.items[i].uid;
        self.tr.first = i;
        self.tr.off = 0.0;
    }

    /// A 目次 entry was clicked: show its first line and stay there.
    pub fn show_line(&mut self, model: &Model, idx: usize) {
        if idx >= model.items.len() { return; }
        self.tr.follow = false;
        self.tr.anchor = model.items[idx].uid;
        self.tr.first = idx;
        self.tr.off = 0.0;
    }

    /// Which 目次 row is at this height, from the rows the last pass measured and drew.
    fn topic_at(&self, fy: f32) -> Option<usize> {
        let mut y = self.r_topics.top - self.tp.off;
        for i in self.tp.first..self.tp.first + self.tp.shown {
            let h = self.krows.get(&(i as i64))?.h;
            if fy >= y && fy < y + h { return Some(i); }
            y += h;
        }
        None
    }

    // ---------------------------------------------------- what the checks ask about

    /// The middle of the control with this text, in window pixels; `frac` picks a point along a
    /// slider instead. Only controls that are on the screen are found.
    pub fn spot(&self, label: &str, frac: f32) -> Option<(i32, i32)> {
        if label == JUMP {
            if self.tr.follow { return None; }
            let j = self.r_jump;
            return Some((((j.left + j.right) / 2.0) as i32, ((j.top + j.bottom) / 2.0) as i32));
        }
        let top = self.r_set.top - self.set_off;
        for (rows, base) in [(&self.btns, 0.0f32), (&self.sets, top)] {
            for (i, r) in rows.iter().enumerate() {
                if self.label_of(rows, i) != Some(label) { continue; }
                let y = base + r.y + r.h / 2.0;
                if base > 0.0 && (y < self.r_set.top || y >= self.r_set.bottom) { return None; }
                if let Kind::Sld(_, _, _, _) = r.kind {
                    let (tx, tw) = self.track(r);
                    return Some(((tx + tw * frac) as i32, y as i32));
                }
                let x = if matches!(r.kind, Kind::Chk(_, _)) { r.x + self.px(6.0) } else { r.x + r.w / 2.0 };
                return Some((x as i32, y as i32));
            }
        }
        None
    }

    /// The text a control was built with; a slider answers with the caption above it, which is
    /// what the checks name it by (its own text is the live value).
    fn label_of<'a>(&self, rows: &'a [Row], i: usize) -> Option<&'a str> {
        let r = &rows[i];
        if matches!(r.kind, Kind::Sld(_, _, _, _)) {
            return rows.get(i.wrapping_sub(1))?.text.split(' ').next();
        }
        if matches!(r.kind, Kind::Cap(_) | Kind::Sep) || r.text.is_empty() { return None; }
        Some(&r.text)
    }

    /// The middle of a 目次 row, for the checks.
    pub fn topic_spot(&self, i: usize) -> Option<(i32, i32)> {
        if i < self.tp.first || i >= self.tp.first + self.tp.shown { return None; }
        let mut y = self.r_topics.top - self.tp.off;
        for k in self.tp.first..i { y += self.krows.get(&(k as i64))?.h; }
        y += self.krows.get(&(i as i64))?.h / 2.0;
        Some((((self.r_topics.left + self.r_topics.right) / 2.0) as i32, y as i32))
    }

    /// A point on the 設定 panel's scroll bar, `frac` of the way down it; None while the panel
    /// needs no bar.
    pub fn settings_bar_spot(&self, frac: f32) -> Option<(i32, i32)> {
        if !self.set_sb { return None; }
        let r = self.r_set;
        Some(((r.right - self.px(SB_W) / 2.0) as i32, (r.top + (r.bottom - r.top) * frac) as i32))
    }

    /// How far the 設定 panel is scrolled, for the checks.
    pub fn settings_off(&self) -> f32 { self.set_off }

    /// The 設定 thumb, top and height: `draw_settings` draws it there and `scroll_settings`
    /// maps the pointer to it, so the checks can hold one against the other. None while the
    /// panel needs no bar.
    pub fn settings_thumb(&self) -> Option<(f32, f32)> {
        if !self.set_sb { return None; }
        let (vh, total) = (self.set_h, self.sets_h());
        let th = self.thumb_h(vh, total);
        Some((self.r_set.top + (vh - th) * (self.set_off / (total - vh).max(1.0)).clamp(0.0, 1.0), th))
    }

    /// The transcript's thumb, top and height, the same ones `thumb` draws; None while the
    /// whole transcript is on the screen and there is no bar.
    pub fn transcript_thumb(&self) -> Option<(f32, f32)> {
        if !self.tr.over { return None; }
        Some(self.bar_thumb(&self.tr, self.r_trans))
    }

    pub fn transcript_spot(&self) -> (i32, i32) {
        (((self.r_trans.left + self.r_trans.right) / 2.0) as i32, ((self.r_trans.top + self.r_trans.bottom) / 2.0) as i32)
    }

    pub fn sidebar_left(&self) -> i32 { self.r_trans.right as i32 }
}

impl Drop for MainWin {
    fn drop(&mut self) {
        unsafe { drop_surface(self.memdc, self.dib) }
    }
}
