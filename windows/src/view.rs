//! The picture of the overlay: subtitles stacked from the bottom, outlined, on a see-through
//! plate. Drawn on the CPU (DirectWrite + Direct2D into a bitmap of our own) and handed to the
//! window whole, with per-pixel alpha. A subtitle is drawn once, into a small picture that is
//! kept for as long as it stays on show; a new frame is those pictures put in their places.
use std::{collections::HashMap, ffi::c_void, mem::size_of};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::{Common::*, *}, DirectWrite::*, Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, Gdi::*},
        UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA},
    },
};

// Measures at 100% scaling, as in the C# app.
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 10.0;
const JA_EN_GAP: f32 = 3.0;
const BLOCK_GAP: f32 = 11.0;
const STATUS_SIZE: f32 = 12.5;
const HINT_SIZE: f32 = 13.0;
const CORNER: f32 = 14.0;
pub const EDGE_BAND: f32 = 8.0; // along each side: resizes instead of dragging
pub const MIN_W: f32 = 260.0;
pub const MIN_H: f32 = 80.0;

const JA_FILL: u32 = 0xFFFFFF;
const JA_DIM: u32 = 0xBDC4CC;
const EN_FILL: u32 = 0xE2E6EA;
const EN_DIM: u32 = 0xB0B6BC;
const STATUS_FILL: u32 = 0xA8C8F0;
const HINT_FILL: u32 = 0x9A9A9A;

pub struct Entry<'a> { pub uid: u64, pub ja: &'a str, pub en: &'a str, pub dim: bool }

#[derive(Clone, PartialEq)]
pub struct Look { pub ja_size: i32, pub en_size: i32, pub show_ja: bool, pub show_en: bool, pub bg_percent: i32, pub grip: bool }

struct Block { key: String, bmp: Option<ID2D1Bitmap>, h: f32, margin: f32, pass: u32 }

impl Block {
    fn empty() -> Block { Block { key: String::new(), bmp: None, h: 0.0, margin: 0.0, pass: 0 } }
}

/// One text of a block: what to draw, where in the block, in which colour, how fat the outline.
struct Run { layout: IDWriteTextLayout, y: f32, fill: D2D1_COLOR_F, ring: i32 }

pub struct View {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    memdc: HDC,
    dib: HBITMAP,
    bits: *mut c_void,
    pub w: i32,
    pub h: i32,
    pub scale: f32,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    formats: HashMap<(bool, u32), IDWriteTextFormat>,
    blocks: HashMap<u64, Block>,
    status: Block,
    hint: Block,
    pass: u32,
    pub last_shown: usize,
    pub blocks_drawn: u64,
}

fn rgba(c: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: ((c >> 16) & 255) as f32 / 255.0, g: ((c >> 8) & 255) as f32 / 255.0, b: (c & 255) as f32 / 255.0, a }
}

/// Where the outline's copies of the text go: whole pixels around the text, so that the glyph
/// pictures DirectWrite keeps are the same ones for every copy.
fn ring(px: i32) -> Vec<(f32, f32)> {
    let n = if px <= 3 { 8 } else if px <= 6 { 16 } else { 32 };
    let mut v: Vec<(f32, f32)> = Vec::new();
    for i in 0..n {
        let a = i as f32 * std::f32::consts::TAU / n as f32;
        let d = ((px as f32 * a.cos()).round(), (px as f32 * a.sin()).round());
        if d != (0.0, 0.0) && !v.contains(&d) { v.push(d); }
    }
    v
}

unsafe fn surface(w: i32, h: i32) -> Result<(HDC, HBITMAP, *mut c_void)> {
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

impl View {
    pub unsafe fn new(w: i32, h: i32, scale: f32) -> Result<View> {
        let (w, h) = (w.max(1), h.max(1));
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0, dpiY: 96.0, // everything in here is in pixels
            ..Default::default()
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        let (memdc, dib, bits) = surface(w, h)?;
        // until the View holds them, every way out has to let the DC and the bitmap go
        if let Err(e) = rt.BindDC(memdc, &RECT { left: 0, top: 0, right: w, bottom: h }) { drop_surface(memdc, dib); return Err(e); }
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let brush = match rt.CreateSolidColorBrush(&rgba(0, 1.0), None) {
            Ok(b) => b,
            Err(e) => { drop_surface(memdc, dib); return Err(e); }
        };
        Ok(View { d2d, dw, memdc, dib, bits, w, h, scale, rt, brush, formats: HashMap::new(), blocks: HashMap::new(),
                  status: Block::empty(), hint: Block::empty(), pass: 0, last_shown: 0, blocks_drawn: 0 })
    }

    pub unsafe fn resize(&mut self, w: i32, h: i32) -> Result<()> {
        let (w, h) = (w.max(1), h.max(1));
        if w == self.w && h == self.h { return Ok(()); }
        let (memdc, dib, bits) = surface(w, h)?;
        // the old surface is kept when the new one cannot be bound; the new one goes
        if let Err(e) = self.rt.BindDC(memdc, &RECT { left: 0, top: 0, right: w, bottom: h }) { drop_surface(memdc, dib); return Err(e); }
        drop_surface(self.memdc, self.dib);
        (self.memdc, self.dib, self.bits, self.w, self.h) = (memdc, dib, bits, w, h);
        Ok(())
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 0.001 {
            self.scale = scale;
            self.formats.clear();
        }
    }

    /// Lets go of the picture and of the subtitles drawn for it, while the window is hidden.
    /// The next `resize` brings the picture back.
    pub unsafe fn release(&mut self) -> Result<()> {
        self.blocks.clear();
        self.status = Block::empty();
        self.hint = Block::empty();
        self.resize(1, 1)
    }

    /// The window's picture as it was last composed: premultiplied BGRA, top row first.
    pub fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits as *const u8, (self.w * self.h * 4) as usize) }
    }

    pub fn px(&self, v: f32) -> f32 { (v * self.scale).round() }

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

    unsafe fn layout(&mut self, text: &str, bold: bool, size: f32, max_w: f32, centred: bool) -> Result<(IDWriteTextLayout, f32)> {
        let f = self.format(bold, size)?;
        let u: Vec<u16> = text.encode_utf16().collect();
        let l = self.dw.CreateTextLayout(&u, &f, max_w.max(10.0), 100000.0)?;
        if centred { l.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?; }
        let mut m = DWRITE_TEXT_METRICS::default();
        l.GetMetrics(&mut m)?;
        Ok((l, m.height)) // not rounded: places are worked out in fractions and put on whole pixels at the end, as WPF did
    }

    fn ring_px(&self, stroke_w: f32) -> i32 { ((stroke_w / 2.0 * self.scale).round() as i32).max(1) }

    /// Draws the runs into a picture of their own, with room around them for the outline.
    unsafe fn picture(&mut self, runs: &[Run], w: f32, h: f32) -> Result<(ID2D1Bitmap, f32)> {
        let margin = runs.iter().map(|r| r.ring).max().unwrap_or(1) as f32 + 1.0;
        let size = D2D_SIZE_F { width: w + 2.0 * margin, height: h.ceil() + 2.0 * margin };
        let brt = self.rt.CreateCompatibleRenderTarget(Some(&size), None, None, D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE)?;
        brt.BeginDraw();
        brt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        brt.Clear(Some(&rgba(0, 0.0)));
        for r in runs {
            self.brush.SetColor(&rgba(0, 232.0 / 255.0));
            for (dx, dy) in ring(r.ring) {
                brt.DrawTextLayout(D2D_POINT_2F { x: margin + dx, y: margin + r.y + dy }, &r.layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
            self.brush.SetColor(&r.fill);
            brt.DrawTextLayout(D2D_POINT_2F { x: margin, y: margin + r.y }, &r.layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
        brt.EndDraw(None, None)?;
        self.blocks_drawn += 1;
        Ok((brt.GetBitmap()?, margin))
    }

    unsafe fn blit(&self, b: &Block, x: f32, y: f32) {
        let (x, y) = (x.round_ties_even(), y.round_ties_even()); // halves go to the even pixel, as .NET's Math.Round put them
        if let Some(bmp) = &b.bmp {
            let s = bmp.GetSize();
            let dst = D2D_RECT_F { left: x - b.margin, top: y - b.margin, right: x - b.margin + s.width, bottom: y - b.margin + s.height };
            self.rt.DrawBitmap(bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None);
        }
    }

    /// A line of its own (the status, the hint): drawn again only when it changes.
    unsafe fn single(&mut self, mut b: Block, text: &str, size: f32, max_w: f32, centred: bool, fill: D2D1_COLOR_F, stroke_w: f32) -> Result<Block> {
        let key = format!("{size}/{max_w}/{}/{text}", self.scale);
        if b.key != key {
            let (layout, h) = self.layout(text, false, size, max_w, centred)?;
            let ring = self.ring_px(stroke_w);
            let (bmp, margin) = self.picture(&[Run { layout, y: 0.0, fill, ring }], max_w, h)?;
            b = Block { key, bmp: Some(bmp), h, margin, pass: self.pass };
        }
        Ok(b)
    }

    /// Puts the frame together. Newest subtitle at the bottom, older ones above it for as far
    /// as the height goes; the newest is shown even when it alone does not fit.
    pub unsafe fn compose(&mut self, entries: &[Entry], status: &str, hint: &str, look: &Look) -> Result<()> {
        self.pass += 1;
        let (w, h) = (self.w as f32, self.h as f32);
        let (pad_x, pad_y) = (PAD_X * self.scale, PAD_Y * self.scale);
        let avail_w = w - 2.0 * pad_x;
        self.rt.BeginDraw();
        // The plate: one flat fill, and only the four corners drawn as curves (filling the whole
        // rounded shape costs about as much as everything else in a frame together).
        let plate = ((look.bg_percent as f32 * 2.55).round()).clamp(1.0, 255.0) / 255.0; // never fully clear: it must take the mouse
        self.rt.Clear(Some(&rgba(0, plate)));
        self.brush.SetColor(&rgba(0, plate));
        let r = self.px(CORNER);
        let shape = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: h }, radiusX: r, radiusY: r };
        for (x, y) in [(0.0, 0.0), (w - r, 0.0), (0.0, h - r), (w - r, h - r)] {
            self.rt.PushAxisAlignedClip(&D2D_RECT_F { left: x, top: y, right: x + r, bottom: y + r }, D2D1_ANTIALIAS_MODE_ALIASED);
            self.rt.Clear(Some(&rgba(0, 0.0)));
            self.rt.FillRoundedRectangle(&shape, &self.brush);
            self.rt.PopAxisAlignedClip();
        }

        let mut top_used = pad_y;
        if !status.is_empty() {
            let b = std::mem::replace(&mut self.status, Block::empty());
            self.status = self.single(b, status, STATUS_SIZE, avail_w, false, rgba(STATUS_FILL, 1.0), 1.6)?;
            self.blit(&self.status, pad_x, pad_y);
            top_used = pad_y + self.status.h + 4.0 * self.scale;
        }

        let look_key = format!("/{}/{}/{avail_w}/{}{}", look.ja_size, look.en_size, self.scale, if look.show_ja { "/j" } else { "/e" });
        let gap = JA_EN_GAP * self.scale;
        let mut y = h - pad_y;
        let mut shown = 0;
        for e in entries.iter().rev() {
            let ja = if look.show_ja && !e.ja.is_empty() { Some(e.ja) } else { None };
            let en = if look.show_en && !e.en.is_empty() { Some(e.en) } else { None };
            if ja.is_none() && en.is_none() { continue; }
            let key = format!("{}\n{}\n{}{look_key}", ja.unwrap_or(""), en.unwrap_or(""), if e.dim { "d" } else { "f" });
            let mut b = self.blocks.remove(&e.uid).unwrap_or_else(Block::empty);
            let mut runs: Vec<Run> = Vec::new();
            if b.key != key {
                let mut bh = 0.0;
                if let Some(t) = ja {
                    let (layout, th) = self.layout(t, true, look.ja_size as f32, avail_w, false)?;
                    let ring = self.ring_px((look.ja_size as f32 * 0.105).max(1.6));
                    runs.push(Run { layout, y: 0.0, fill: rgba(if e.dim { JA_DIM } else { JA_FILL }, 1.0), ring });
                    bh = th;
                }
                if let Some(t) = en {
                    let (layout, th) = self.layout(t, false, look.en_size as f32, avail_w, false)?;
                    let ring = self.ring_px((look.en_size as f32 * 0.075).max(1.0));
                    // English alone takes the main colours
                    let fill = if look.show_ja { if e.dim { EN_DIM } else { EN_FILL } } else if e.dim { JA_DIM } else { JA_FILL };
                    let at = if ja.is_some() { bh + gap } else { 0.0 };
                    runs.push(Run { layout, y: at.round_ties_even(), fill: rgba(fill, 1.0), ring });
                    bh = at + th;
                }
                b = Block { key, bmp: None, h: bh, margin: 0.0, pass: 0 };
            }
            let mut top = y - b.h;
            if shown > 0 && top < top_used {
                break; // this one does not fit; neither does anything older
            }
            if top < top_used { top = top_used; }
            if b.bmp.is_none() {
                if runs.is_empty() { b.key.clear(); } // measured earlier but never drawn: lay it out again next time
                else {
                    let (bmp, margin) = self.picture(&runs, avail_w, b.h)?;
                    b.bmp = Some(bmp);
                    b.margin = margin;
                }
            }
            b.pass = self.pass;
            self.blit(&b, pad_x, top);
            self.blocks.insert(e.uid, b);
            y = top - BLOCK_GAP * self.scale;
            shown += 1;
        }
        self.last_shown = shown;
        let pass = self.pass;
        self.blocks.retain(|_, b| b.pass == pass); // scrolled out, or cleared

        if shown == 0 && !hint.is_empty() {
            let b = std::mem::replace(&mut self.hint, Block::empty());
            self.hint = self.single(b, hint, HINT_SIZE, avail_w, true, rgba(HINT_FILL, 176.0 / 255.0), 1.2)?;
            self.blit(&self.hint, pad_x, top_used.max((h - self.hint.h) / 2.0));
        }
        if look.grip {
            // three slanted strokes in the corner that resizes
            self.brush.SetColor(&rgba(0xFFFFFF, 140.0 / 255.0));
            let s = self.scale;
            let (ox, oy) = (w - 21.0 * s, h - 21.0 * s);
            for i in 0..3 {
                let o = i as f32 * 4.5;
                self.rt.DrawLine(D2D_POINT_2F { x: ox + (13.0 - o) * s, y: oy + 15.0 * s }, D2D_POINT_2F { x: ox + 15.0 * s, y: oy + (13.0 - o) * s }, &self.brush, 1.6 * s, None);
            }
        }
        self.rt.EndDraw(None, None)
    }

    /// Hands the composed picture to a layered window. `alpha` dims the whole window (255 = as drawn).
    pub unsafe fn present(&self, hwnd: HWND, alpha: u8) -> Result<()> {
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(hwnd, None, None, Some(&SIZE { cx: self.w, cy: self.h }), self.memdc, Some(&POINT { x: 0, y: 0 }), COLORREF(0), Some(&blend), ULW_ALPHA)
    }

    /// The small button that leads back out of click-through: drawn into this view's whole
    /// surface, which is made the size the button needs.
    pub unsafe fn pill(&mut self, text: &str) -> Result<()> {
        let (layout, th) = self.layout(text, false, 13.0, 4000.0, false)?;
        let mut m = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut m)?;
        let s = self.scale;
        let (w, h) = ((m.width + 28.0 * s).ceil(), (th + 13.0 * s).ceil());
        self.resize(w as i32, h as i32)?;
        self.rt.BeginDraw();
        self.rt.Clear(Some(&rgba(0, 0.0)));
        let half = s / 2.0;
        let body = D2D1_ROUNDED_RECT { rect: D2D_RECT_F { left: half, top: half, right: w - half, bottom: h - half }, radiusX: 14.0 * s, radiusY: 14.0 * s };
        self.brush.SetColor(&rgba(0x1E66D0, 240.0 / 255.0));
        self.rt.FillRoundedRectangle(&body, &self.brush);
        self.brush.SetColor(&rgba(0x9CC8FF, 1.0));
        self.rt.DrawRoundedRectangle(&body, &self.brush, s, None);
        self.brush.SetColor(&rgba(0xFFFFFF, 1.0));
        self.rt.DrawTextLayout(D2D_POINT_2F { x: 14.0 * s, y: (6.0 * s).round() }, &layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        self.rt.EndDraw(None, None)
    }
}

impl Drop for View {
    fn drop(&mut self) {
        unsafe { drop_surface(self.memdc, self.dib) }
    }
}

// Kept so that they outlive what they made, and lent to the normal window: both views draw with
// the same DirectWrite and Direct2D.
impl View {
    pub fn factory(&self) -> &ID2D1Factory { &self.d2d }
    pub fn dwrite(&self) -> &IDWriteFactory { &self.dw }
}
