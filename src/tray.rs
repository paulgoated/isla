//! System tray icon and the Isla app icon (drawn in code, also exported as .ico).

use crate::draw::{paint, rrect_path, Rgba};
use std::sync::OnceLock;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, Pixmap, Point,
    Shader, SpreadMode, Stroke, Transform,
};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TRAY: u32 = WM_APP + 1;

fn rgba8(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color::from_rgba8(r, g, b, (a * 255.0) as u8)
}

fn fill(pm: &mut Pixmap, path: Option<Path>, shader: Shader) {
    if let Some(p) = path {
        let paint = Paint { shader, anti_alias: true, ..Default::default() };
        pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke(pm: &mut Pixmap, path: Option<Path>, width: f32, c: Rgba) {
    if let Some(p) = path {
        let st = Stroke { width, ..Default::default() };
        pm.stroke_path(&p, &paint(c), &st, Transform::identity(), None);
    }
}

/// The Isla app icon (palette "graphite and orange"): a safety-orange tile
/// with the graphite island centered as the mark and an orange waveform inside.
/// Below 32 px the waveform is dropped so the island stays crisp.
pub fn app_icon(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).unwrap();
    let s = size as f32;
    let small = size < 32;
    let (x0, w0) = (s * 0.04, s * 0.92);
    let orange = |a: f32| rgba8(0xFF, 0x4A, 0x1F, a);
    let tile = || rrect_path(x0, x0, w0, w0, s * 0.225);

    // Safety-orange tile, a touch lighter at the top edge.
    if let Some(g) = LinearGradient::new(
        Point::from_xy(0.0, x0),
        Point::from_xy(0.0, x0 + w0),
        vec![
            GradientStop::new(0.0, rgba8(0xFF, 0x6B, 0x42, 1.0)),
            GradientStop::new(1.0, rgba8(0xE5, 0x38, 0x10, 1.0)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        fill(&mut pm, tile(), g);
    }
    stroke(&mut pm, tile(), (s / 80.0).max(1.0), Rgba(0.0, 0.0, 0.0, 0.25));

    // The island, centered and large: the mark itself.
    let (pw, ph) = if small { (s * 0.76, s * 0.34) } else { (s * 0.7, s * 0.27) };
    let (px, py) = (s * 0.5 - pw / 2.0, s * 0.5 - ph / 2.0);
    fill(&mut pm, rrect_path(px, py, pw, ph, ph / 2.0), Shader::SolidColor(rgba8(0x12, 0x13, 0x15, 1.0)));
    if small {
        return pm;
    }

    // Live waveform inside the island, centered.
    let bars = [0.34f32, 0.62, 0.9, 0.55, 0.78, 0.4, 0.26];
    let bw = ph * 0.11;
    let gap = bw * 0.75;
    let total = bars.len() as f32 * bw + (bars.len() - 1) as f32 * gap;
    let mut bx = s * 0.5 - total / 2.0;
    for k in bars {
        let bh = ph * 0.62 * k;
        fill(
            &mut pm,
            rrect_path(bx, py + (ph - bh) / 2.0, bw, bh, bw / 2.0),
            Shader::SolidColor(orange(1.0)),
        );
        bx += bw + gap;
    }
    pm
}

/// Writes isla.ico (PNG-compressed entries, 16-256 px) and isla.png (512 px).
pub fn write_icon_files(dir: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 128, 256];
    let pngs: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| app_icon(s).encode_png().unwrap_or_default())
        .collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (size, png) in sizes.iter().zip(&pngs) {
        let b = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend_from_slice(&[b, b, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in &pngs {
        ico.extend_from_slice(png);
    }
    std::fs::write(format!("{dir}\\isla.ico"), ico)?;
    app_icon(512)
        .save_png(format!("{dir}\\isla.png"))
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn to_hicon(pm: &Pixmap) -> HICON {
    unsafe {
        let (w, h) = (pm.width() as i32, pm.height() as i32);
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
        let Ok(color) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        // Icons take straight (non-premultiplied) BGRA.
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        for (d, p) in dst.chunks_exact_mut(4).zip(pm.data().chunks_exact(4)) {
            let a = p[3] as u32;
            let un = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
            d[0] = un(p[2]);
            d[1] = un(p[1]);
            d[2] = un(p[0]);
            d[3] = p[3];
        }
        let mask = CreateBitmap(w, h, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// (small, big) app icons, created once.
pub fn icons() -> (HICON, HICON) {
    static ICONS: OnceLock<(isize, isize)> = OnceLock::new();
    let (s, b) = *ICONS.get_or_init(|| unsafe {
        let small = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
        let big = GetSystemMetrics(SM_CXICON).max(32) as u32;
        (
            to_hicon(&app_icon(small)).0 as isize,
            to_hicon(&app_icon(big)).0 as isize,
        )
    });
    (HICON(s as _), HICON(b as _))
}

/// Message Explorer broadcasts after it restarts; the icon must be re-added.
pub fn taskbar_created() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icons().0,
        ..Default::default()
    };
    let tip: Vec<u16> = "Isla — klik untuk pengaturan".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

pub fn add(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data(hwnd));
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd));
    }
}

/// Sets the window's title-bar / Alt-Tab icons.
pub fn set_window_icons(hwnd: HWND) {
    let (small, big) = icons();
    unsafe {
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_SMALL as usize)), Some(LPARAM(small.0 as isize)));
        SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(big.0 as isize)));
    }
}
