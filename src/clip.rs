//! Clipboard history (kept in memory only, never written to disk) and
//! screenshots. Content flagged by password managers is skipped.

use crate::system;
use std::path::PathBuf;
use tiny_skia::{IntSize, Pixmap};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::UI::WindowsAndMessaging::*;

pub enum Data {
    Text(String),
    /// Raw CF_DIB bytes (to restore) plus a small preview.
    Image { dib: Vec<u8>, thumb: Pixmap, w: u32, h: u32 },
}

pub struct Item {
    pub data: Data,
    pub time: String,
}

#[derive(Default)]
pub struct History {
    pub items: Vec<Item>,
    /// Skip the update we caused ourselves when restoring an item.
    ignore_next: bool,
}

const MAX_ITEMS: usize = 12;
const MAX_IMAGES: usize = 3;
const MAX_TEXT: usize = 100_000;

pub fn listen(hwnd: HWND) {
    unsafe {
        let _ = AddClipboardFormatListener(hwnd);
    }
}

fn open(hwnd: HWND) -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    false
}

unsafe fn global_bytes(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() {
        return None;
    }
    let v = std::slice::from_raw_parts(p, GlobalSize(g)).to_vec();
    let _ = GlobalUnlock(g);
    Some(v)
}

/// Password managers mark secrets with these formats.
unsafe fn is_private() -> bool {
    let exclude = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
    if exclude != 0 && IsClipboardFormatAvailable(exclude).is_ok() {
        return true;
    }
    let history = RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory"));
    if history != 0 && IsClipboardFormatAvailable(history).is_ok() {
        if let Ok(h) = GetClipboardData(history) {
            if let Some(b) = global_bytes(h) {
                return b.len() >= 4 && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 0;
            }
        }
    }
    false
}

/// Decodes CF_DIB bytes (24/32-bit BI_RGB or BI_BITFIELDS). `max` limits
/// the longest side (nearest-neighbour downscale) for previews.
pub fn dib_to_pixmap(dib: &[u8], max: Option<u32>) -> Option<Pixmap> {
    if dib.len() < 40 {
        return None;
    }
    let rd = |o: usize| u32::from_le_bytes([dib[o], dib[o + 1], dib[o + 2], dib[o + 3]]);
    let hsize = rd(0) as usize;
    let w = rd(4) as i32;
    let h = rd(8) as i32;
    let bpp = u16::from_le_bytes([dib[14], dib[15]]);
    let comp = rd(16);
    let used = rd(32) as usize;
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) || !(comp == 0 || comp == 3) {
        return None;
    }
    let (w, top_down, h) = (w as u32, h < 0, h.unsigned_abs());
    let masks = if comp == 3 && hsize == 40 { 12 } else { 0 };
    let off = hsize + masks + used * 4;
    let stride = ((w * bpp as u32 + 31) / 32 * 4) as usize;
    if dib.len() < off + stride * h as usize {
        return None;
    }
    let scale = max.map(|m| (m as f32 / w.max(h) as f32).min(1.0)).unwrap_or(1.0);
    let (tw, th) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    let alpha_used = bpp == 32 && (0..h as usize).any(|y| {
        let row = &dib[off + y * stride..];
        (0..w as usize).any(|x| row[x * 4 + 3] != 0)
    });
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for ty in 0..th {
        let sy = (ty as f32 / scale) as u32;
        let row = if top_down { sy } else { h - 1 - sy } as usize;
        let src = &dib[off + row * stride..];
        for tx in 0..tw {
            let sx = (tx as f32 / scale) as usize;
            let p = &src[sx * (bpp as usize / 8)..];
            let a = if alpha_used { p[3] } else { 255 };
            let d = &mut out[((ty * tw + tx) * 4) as usize..][..4];
            let pm = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
            d[0] = pm(p[2]);
            d[1] = pm(p[1]);
            d[2] = pm(p[0]);
            d[3] = a;
        }
    }
    Pixmap::from_vec(out, IntSize::from_wh(tw, th)?)
}

impl History {
    /// Reads the clipboard after WM_CLIPBOARDUPDATE. Returns the new image
    /// (full size) when one arrived, for the screenshot-area flow.
    pub fn on_update(&mut self, hwnd: HWND) -> Option<Pixmap> {
        if std::mem::take(&mut self.ignore_next) {
            return None;
        }
        if !open(hwnd) {
            return None;
        }
        let mut new_image = None;
        unsafe {
            if !is_private() {
                let clk = system::local_time();
                let time = format!("{:02}:{:02}", clk.wHour, clk.wMinute);
                if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
                    if let Some(b) = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| global_bytes(h)) {
                        let wide: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len()).min(MAX_TEXT);
                        let text = String::from_utf16_lossy(&wide[..end]);
                        if !text.trim().is_empty() {
                            self.items.retain(|i| !matches!(&i.data, Data::Text(t) if *t == text));
                            self.items.insert(0, Item { data: Data::Text(text), time });
                        }
                    }
                } else if IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
                    if let Some(dib) = GetClipboardData(CF_DIB.0 as u32).ok().and_then(|h| global_bytes(h)) {
                        if let Some(thumb) = dib_to_pixmap(&dib, Some(96)) {
                            let w = u32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
                            let h = (i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]])).unsigned_abs();
                            new_image = dib_to_pixmap(&dib, None);
                            self.items.insert(0, Item { data: Data::Image { dib, thumb, w, h }, time });
                        }
                    }
                }
            }
            let _ = CloseClipboard();
        }
        // Cap the list, and keep only the newest few images (they are big).
        let mut images = 0;
        self.items.retain(|i| {
            if matches!(i.data, Data::Image { .. }) {
                images += 1;
                images <= MAX_IMAGES
            } else {
                true
            }
        });
        self.items.truncate(MAX_ITEMS);
        new_image
    }

    /// Puts item `i` back on the clipboard and moves it to the top.
    pub fn restore(&mut self, i: usize, hwnd: HWND) {
        if i >= self.items.len() || !open(hwnd) {
            return;
        }
        let item = self.items.remove(i);
        unsafe {
            let _ = EmptyClipboard();
            let (fmt, bytes): (u32, Vec<u8>) = match &item.data {
                Data::Text(t) => (
                    CF_UNICODETEXT.0 as u32,
                    t.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect(),
                ),
                Data::Image { dib, .. } => (CF_DIB.0 as u32, dib.clone()),
            };
            if let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) {
                let p = GlobalLock(g) as *mut u8;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                    let _ = GlobalUnlock(g);
                    if SetClipboardData(fmt, Some(HANDLE(g.0))).is_ok() {
                        self.ignore_next = true;
                    }
                }
            }
            let _ = CloseClipboard();
        }
        self.items.insert(0, item);
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            self.items.remove(i);
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

// ---------------------------------------------------------------------
// screenshots
// ---------------------------------------------------------------------

/// Captures the whole virtual screen (all monitors) as an opaque pixmap.
pub fn capture_screen() -> Option<Pixmap> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w <= 0 || h <= 0 {
            return None;
        }
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok();
        let mut out = None;
        if let Some(bmp) = bmp {
            let old = SelectObject(mem, bmp.into());
            if BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT).is_ok() {
                let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
                let mut data = src.to_vec();
                for p in data.chunks_exact_mut(4) {
                    p.swap(0, 2);
                    p[3] = 255;
                }
                out = Pixmap::from_vec(data, IntSize::from_wh(w as u32, h as u32)?);
            }
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        out
    }
}

/// Saves into Pictures\Screenshots\Isla_YYYY-MM-DD_HH-MM-SS.png.
pub fn save_screenshot(pm: &Pixmap) -> Option<PathBuf> {
    let base = std::env::var("USERPROFILE").ok()?;
    let dir = PathBuf::from(base).join("Pictures").join("Screenshots");
    std::fs::create_dir_all(&dir).ok()?;
    let t = system::local_time();
    let name = format!(
        "Isla_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.png",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let path = dir.join(name);
    pm.save_png(&path).ok()?;
    Some(path)
}
