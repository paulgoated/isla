//! Settings window, custom-drawn with tiny-skia.
//!
//! Design (dealt with the taste skill, palette "graphite and orange", radius soft):
//! a sidebar of sections; each section is a column of rows with a label, a
//! one-line explanation and one control on the right. Orange is used only for
//! "on" states and active marks; everything else is graphite greys. Titles and
//! numerals use Bahnschrift, body text Segoe UI. Every change is written to
//! config.toml right away and applied live.

use crate::config::{self, Config};
use crate::draw::*;
use crate::text::{Fonts, Weight};
use std::ffi::c_void;
use tiny_skia::{Color, Pixmap};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------------
// tokens
// ---------------------------------------------------------------------

/// Client size in design units.
const W: f32 = 820.0;
const H: f32 = 600.0;

const fn hex(v: u32) -> Rgba {
    Rgba(
        ((v >> 16) & 255) as f32 / 255.0,
        ((v >> 8) & 255) as f32 / 255.0,
        (v & 255) as f32 / 255.0,
        1.0,
    )
}

const BG: Rgba = hex(0x131416); // sidebar, window
const PANEL: Rgba = hex(0x18191B); // content (the palette's ground)
const SURFACE: Rgba = hex(0x222427); // controls, active nav
const SURFACE_HOT: Rgba = hex(0x2C2F33);
const FIELD: Rgba = hex(0x131416);
const LINE: Rgba = hex(0x25272B); // dividers
const BORDER: Rgba = hex(0x34373C);
const TRACK: Rgba = hex(0x34373C);
const TRACK_HOT: Rgba = hex(0x3F4248);
const NAV_HOT: Rgba = hex(0x1C1D20);
const MUTED: Rgba = hex(0x8E9196);
const FAINT: Rgba = hex(0x5F6268);
const INK: Rgba = hex(0xEFF0F1);
const ACCENT: Rgba = hex(0xFF4A1F);
const PAPER: Rgba = hex(0xFFFFFF);
const DANGER: Rgba = hex(0xC42B1C);
/// Notch accent choices (no violet).
pub const PALETTE: [&str; 8] = [
    "#FF4A1F", "#0A84FF", "#30D158", "#FF9F0A", "#2456D6", "#FF6B9A", "#64D2FF", "#DAFA9C",
];

const SIDE_W: f32 = 208.0;
const X0: f32 = SIDE_W + 36.0;
const X1: f32 = W - 36.0;
const ROWS_Y: f32 = 112.0;
const ROW_H: f32 = 56.0;

/// Height of the drawn caption strip (drag area).
pub const CAPTION_H: f32 = 40.0;

const SECTIONS: [&str; 6] = ["Umum", "Tampilan", "Musik", "Sistem", "Produktivitas", "Tentang"];
const SEC_SYSTEM: usize = 3;
const CITY_ROW: usize = 6;

fn row_cy(i: usize) -> f32 {
    ROWS_Y + i as f32 * ROW_H + ROW_H / 2.0
}

/// City field rect (design units) in the Sistem section.
fn city_rect() -> (f32, f32, f32, f32) {
    (X1 - 220.0, row_cy(CITY_ROW) - 16.0, 220.0, 32.0)
}

const PREVIEW_Y: f32 = 110.0;
const PREVIEW_H: f32 = 140.0;

fn preview_rows_cy(i: usize) -> f32 {
    PREVIEW_Y + PREVIEW_H + 14.0 + i as f32 * ROW_H + ROW_H / 2.0
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Slider {
    Position,
    Size,
    Hover,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SAction {
    Section(usize),
    Autostart,
    HideFs,
    Monitor(usize),
    Slider(Slider),
    CenterPos,
    FocusMinus,
    FocusPlus,
    Accent(usize),
    Fahrenheit,
    Feature(usize),
    ShortcutRemove(usize),
    ShortcutAdd,
    OpenConfig,
    OpenFolder,
    OpenRelease,
    InstallUpdate,
    Minimize,
    Close,
}

pub struct Settings {
    pub hwnd: HWND,
    edit: HWND,
    font: HFONT,
    pub brush: HBRUSH,
    s: f32,
    cw: i32,
    ch: i32,
    pixmap: Pixmap,
    bgra: Vec<u8>,
    buttons: Vec<([f32; 4], SAction)>,
    pub hot: Option<SAction>,
    pub drag: Option<Slider>,
    pub draft: Config,
    pub autostart: bool,
    monitors: usize,
    pub tracking: bool,
    section: usize,
    logo: Pixmap,
    /// Newer release tag, set by the app before each paint.
    pub update_tag: Option<String>,
}

impl Drop for Settings {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.brush.into());
        }
    }
}

fn rgb(c: Rgba) -> COLORREF {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
    COLORREF(b(c.0) | (b(c.1) << 8) | (b(c.2) << 16))
}

impl Settings {
    pub fn open(cfg: &Config) -> Option<Settings> {
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            let class = w!("IslaSettings");
            let wc = WNDCLASSW {
                style: CS_DROPSHADOW,
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
                ..Default::default()
            };
            RegisterClassW(&wc); // fails harmlessly when already registered

            let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let work = mi.rcWork;
            let dpi_s = windows::Win32::UI::HiDpi::GetDpiForSystem() as f32 / 96.0;
            let fit = ((work.bottom - work.top) as f32 - 80.0) / H;
            let s = dpi_s.min(fit).max(0.6);
            let (cw, ch) = ((W * s).round() as i32, (H * s).round() as i32);

            // Borderless: the caption strip is drawn by us.
            // WS_SYSMENU | WS_MINIMIZEBOX keep taskbar minimize/restore working.
            let style = WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
            let x = work.left + ((work.right - work.left) - cw) / 2;
            let y = work.top + ((work.bottom - work.top) - ch) / 2;
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Pengaturan Isla"),
                style,
                x,
                y,
                cw,
                ch,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .ok()?;
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(20), &dark as *const i32 as *const c_void, 4);
            let round: i32 = 2; // DWMWCP_ROUND (Windows 11)
            let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(33), &round as *const i32 as *const c_void, 4);
            crate::tray::set_window_icons(hwnd);

            let font = CreateFontW(
                -(13.5 * s) as i32,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Segoe UI"),
            );
            let brush = CreateSolidBrush(rgb(FIELD));
            let (fx, fy, fw, fh) = city_rect();
            let eh = (19.0 * s) as i32;
            let edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                &HSTRING::from(cfg.weather.city.as_str()),
                WS_CHILD | WINDOW_STYLE(0x80), // ES_AUTOHSCROLL; shown with its section
                ((fx + 12.0) * s) as i32,
                ((fy + fh / 2.0) * s) as i32 - eh / 2,
                ((fw - 24.0) * s) as i32,
                eh,
                Some(hwnd),
                Some(HMENU(100 as _)),
                Some(hinst.into()),
                None,
            )
            .ok()?;
            SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));

            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            Some(Settings {
                hwnd,
                edit,
                font,
                brush,
                s,
                cw,
                ch,
                pixmap: Pixmap::new(cw as u32, ch as u32)?,
                bgra: vec![0; (cw * ch * 4) as usize],
                buttons: Vec::new(),
                hot: None,
                drag: None,
                draft: cfg.clone(),
                autostart: crate::system::autostart_enabled(),
                monitors: GetSystemMetrics(SM_CMONITORS).max(1) as usize,
                tracking: false,
                section: 0,
                logo: crate::tray::app_icon(128),
                update_tag: None,
            })
        }
    }

    pub fn set_section(&mut self, i: usize) {
        self.section = i.min(SECTIONS.len() - 1);
        unsafe {
            let _ = ShowWindow(self.edit, if self.section == SEC_SYSTEM { SW_SHOW } else { SW_HIDE });
        }
        self.invalidate();
    }

    pub fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    pub fn city_text(&self) -> String {
        unsafe {
            let n = GetWindowTextLengthW(self.edit) as usize;
            let mut buf = vec![0u16; n + 1];
            let got = GetWindowTextW(self.edit, &mut buf) as usize;
            String::from_utf16_lossy(&buf[..got]).trim().to_string()
        }
    }

    /// Re-syncs the form after config.toml was edited by hand.
    pub fn sync(&mut self, cfg: &Config) {
        if self.draft.weather.city != cfg.weather.city {
            unsafe {
                let _ = SetWindowTextW(self.edit, &HSTRING::from(cfg.weather.city.as_str()));
            }
        }
        self.draft = cfg.clone();
        self.invalidate();
    }

    /// Physical client point is on the draggable caption (not a control).
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        y < CAPTION_H * self.s && self.hit(x, y).is_none()
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<SAction> {
        self.buttons
            .iter()
            .find(|(r, _)| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
            .map(|(_, a)| *a)
    }

    /// (x0, x1, cy) of a slider track in design units.
    fn slider_track(kind: Slider) -> (f32, f32, f32) {
        let (x1, cy) = match kind {
            Slider::Hover => (X1, row_cy(2)),
            Slider::Position => (X1 - 36.0, preview_rows_cy(1)),
            Slider::Size => (X1, preview_rows_cy(2)),
        };
        (x1 - 190.0, x1, cy)
    }

    /// Updates the draft from a slider drag at physical x.
    pub fn set_slider(&mut self, kind: Slider, x: f32) {
        let (x0, x1, _) = Self::slider_track(kind);
        let f = ((x / self.s - x0) / (x1 - x0)).clamp(0.0, 1.0);
        match kind {
            Slider::Position => {
                let p = f * 2.0 - 1.0;
                self.draft.position = if p.abs() < 0.04 { 0.0 } else { (p * 100.0).round() / 100.0 };
            }
            Slider::Size => self.draft.size = ((0.8 + f * 0.7) * 20.0).round() / 20.0,
            Slider::Hover => self.draft.hover_delay_ms = ((f * 600.0) / 20.0).round() as u64 * 20,
        }
        self.invalidate();
    }

    pub fn save_png(&self, path: &str) {
        let _ = self.pixmap.save_png(path);
    }

    pub fn field_colors(&self, hdc: HDC) -> LRESULT {
        unsafe {
            SetTextColor(hdc, rgb(INK));
            SetBkColor(hdc, rgb(FIELD));
        }
        LRESULT(self.brush.0 as isize)
    }

    // ------------------------------------------------------------------
    // drawing
    // ------------------------------------------------------------------

    pub fn render(&mut self, fonts: &mut Fonts, icons: &[Option<Pixmap>]) {
        self.pixmap.fill(Color::from_rgba(BG.0, BG.1, BG.2, 1.0).unwrap());
        let mut ui = Ui {
            cv: Canvas {
                pm: &mut self.pixmap,
                mask: None,
                fonts,
                ox: 0.0,
                oy: 0.0,
                s: self.s,
                alpha: 1.0,
            },
            btn: Vec::new(),
            hot: self.hot,
            drag: self.drag,
        };
        let d = &self.draft;

        // Content panel and the sidebar.
        ui.cv.rrect(SIDE_W, 0.0, W - SIDE_W, H, 0.0, PANEL);
        ui.cv.rrect(SIDE_W, 0.0, 1.0, H, 0.0, LINE);
        sidebar(&mut ui, self.section, &self.logo);
        window_buttons(&mut ui);

        let (title, lead) = match self.section {
            0 => ("Umum", "Kapan Isla muncul dan bagaimana ia bereaksi."),
            1 => ("Tampilan", "Letak, ukuran, dan warna notch. Pratinjau mengikuti perubahanmu."),
            2 => ("Musik", "Apa yang tampil saat lagu diputar."),
            3 => ("Sistem", "Angka di tab Sistem, dan dari mana cuaca diambil."),
            4 => ("Produktivitas", "Alat kecil di dalam notch."),
            _ => ("", ""),
        };
        if !title.is_empty() {
            ui.cv.text(X0, 66.0, 26.0, Weight::Display, INK, title);
            ui.cv.text(X0, 90.0, 12.5, Weight::Regular, MUTED, lead);
        }

        match self.section {
            0 => {
                let rows: [(&str, &str); 6] = [
                    ("Jalankan saat Windows mulai", "Isla terbuka sendiri setelah kamu login."),
                    ("Sembunyi saat layar penuh", "Notch hilang selama game, video, atau presentasi."),
                    ("Jeda sebelum membesar", "Lama kursor diam di notch sebelum notch terbuka."),
                    ("Durasi fokus", "Nilai awal tombol Fokus di tab Beranda."),
                    ("Indikator mic dan kamera", "Titik oranye atau hijau saat app memakai mic atau kamera."),
                    ("Kedip Caps Lock dan Num Lock", "Notch memberi tahu saat tombolnya ditekan."),
                ];
                rows_with_dividers(&mut ui, &rows, ROWS_Y);
                ui.toggle(row_cy(0), self.autostart, SAction::Autostart);
                ui.toggle(row_cy(1), d.hide_in_fullscreen, SAction::HideFs);
                ui.slider(Slider::Hover, d.hover_delay_ms as f32 / 600.0, &format!("{} ms", d.hover_delay_ms));
                ui.stepper(row_cy(3), &format!("{} mnt", d.focus_minutes), SAction::FocusMinus, SAction::FocusPlus);
                ui.toggle(row_cy(4), d.features.privacy, SAction::Feature(5));
                ui.toggle(row_cy(5), d.features.lock_keys, SAction::Feature(6));
            }
            1 => {
                preview(&mut ui, d);
                let rows: [(&str, &str); 4] = [
                    ("Monitor", "Layar tempat notch menempel."),
                    ("Posisi", "Geser kiri atau kanan di tepi atas layar."),
                    ("Ukuran", "Skala notch di atas skala tampilan Windows."),
                    ("Warna aksen", "Tab aktif, ikon tanpa gambar, dan sorotan."),
                ];
                rows_with_dividers(&mut ui, &rows, preview_rows_cy(0) - ROW_H / 2.0);
                ui.monitors(preview_rows_cy(0), self.monitors.max(d.monitor + 1).min(4), d.monitor);
                let pos = if d.position == 0.0 {
                    "Tengah".to_string()
                } else if d.position < 0.0 {
                    format!("Kiri {:.0}%", -d.position * 100.0)
                } else {
                    format!("Kanan {:.0}%", d.position * 100.0)
                };
                ui.slider(Slider::Position, (d.position + 1.0) / 2.0, &pos);
                ui.center_button(preview_rows_cy(1));
                ui.slider(Slider::Size, (d.size - 0.8) / 0.7, &format!("{:.2}×", d.size));
                ui.swatches(preview_rows_cy(3), &d.accent);
            }
            2 => {
                let rows: [(&str, &str); 3] = [
                    ("Info dan kontrol musik", "Sampul, judul, dan tombol putar dari Spotify, YouTube, dan app lain."),
                    ("Lirik sinkron", "Baris lirik di notch. Judul dan artis dikirim ke LRCLIB."),
                    ("Bar volume", "Muncul sebentar saat volume berubah."),
                ];
                rows_with_dividers(&mut ui, &rows, ROWS_Y);
                for (i, f) in [0usize, 13, 10].into_iter().enumerate() {
                    ui.toggle(row_cy(i), d.features.flag(f), SAction::Feature(f));
                }
            }
            3 => {
                let ping_desc = format!("Latensi ke {}, diukur tiap 2 detik.", d.ping_host);
                let rows: [(&str, &str); 8] = [
                    ("Tab Sistem", "CPU, RAM, jaringan, dan audio dalam satu tab."),
                    ("GPU NVIDIA", "Pemakaian, suhu, dan VRAM dari driver NVIDIA."),
                    ("Kecepatan internet", "Unduh dan unggah saat ini."),
                    ("Ping", &ping_desc),
                    ("Baterai Bluetooth", "Headset dan earbuds yang tersambung."),
                    ("Cuaca", "Suhu dan kondisi dari Open-Meteo."),
                    ("Kota", "Kosongkan untuk memakai lokasi Windows."),
                    ("Fahrenheit", "Tampilkan suhu dalam °F."),
                ];
                rows_with_dividers(&mut ui, &rows, ROWS_Y);
                for (i, f) in [1usize, 2, 3, 14, 15, 4].into_iter().enumerate() {
                    ui.toggle(row_cy(i), d.features.flag(f), SAction::Feature(f));
                }
                let (fx, fy, fw, fh) = city_rect();
                ui.cv.rrect(fx, fy, fw, fh, 6.0, FIELD);
                ui.cv.stroke(rrect_path(fx, fy, fw, fh, 6.0), 1.0, BORDER);
                ui.toggle(row_cy(7), d.weather.fahrenheit, SAction::Fahrenheit);
            }
            4 => {
                let rows: [(&str, &str); 5] = [
                    ("Tab Alat", "Stopwatch, pengingat, alarm, dan screenshot."),
                    ("Riwayat clipboard", "12 salinan terakhir, hanya di memori."),
                    ("Shelf", "Seret file ke notch untuk disimpan sementara."),
                    ("Notifikasi Windows", "Notifikasi baru tampil sebentar di notch."),
                    ("Shortcut app", "Deretan app di tab Beranda, diatur di bawah."),
                ];
                rows_with_dividers(&mut ui, &rows, ROWS_Y);
                for (i, f) in [11usize, 12, 8, 9, 7].into_iter().enumerate() {
                    ui.toggle(row_cy(i), d.features.flag(f), SAction::Feature(f));
                }
                shortcuts(&mut ui, d, icons, row_cy(4) + ROW_H / 2.0 + 16.0);
            }
            _ => about(&mut ui, &self.logo, self.update_tag.as_deref()),
        }

        // Hairline edge so the borderless window holds on dark wallpapers.
        ui.cv.stroke(rrect_path(0.5, 0.5, W - 1.0, H - 1.0, 0.0), 1.0, BORDER);
        self.buttons = ui.btn;
    }

    pub fn blit(&mut self, hdc: HDC) {
        for (d, p) in self.bgra.chunks_exact_mut(4).zip(self.pixmap.data().chunks_exact(4)) {
            d[0] = p[2];
            d[1] = p[1];
            d[2] = p[0];
            d[3] = 255;
        }
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.cw,
                biHeight: -self.ch,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            SetDIBitsToDevice(
                hdc,
                0,
                0,
                self.cw as u32,
                self.ch as u32,
                0,
                0,
                0,
                self.ch as u32,
                self.bgra.as_ptr() as *const c_void,
                &bi,
                DIB_RGB_COLORS,
            );
        }
    }
}

// ---------------------------------------------------------------------
// components: one style each for toggle, slider, stepper, segment, button
// ---------------------------------------------------------------------

struct Ui<'a> {
    cv: Canvas<'a>,
    btn: Vec<([f32; 4], SAction)>,
    hot: Option<SAction>,
    drag: Option<Slider>,
}

impl Ui<'_> {
    fn is_hot(&self, a: SAction) -> bool {
        self.hot == Some(a)
    }

    fn push(&mut self, x: f32, y: f32, w: f32, h: f32, a: SAction) {
        let r = self.cv.phys(x, y, w, h);
        self.btn.push((r, a));
    }

    fn toggle(&mut self, cy: f32, on: bool, a: SAction) {
        let (w, h) = (40.0, 22.0);
        let x = X1 - w;
        let hot = self.is_hot(a);
        let bg = if on { ACCENT } else if hot { TRACK_HOT } else { TRACK };
        self.cv.rrect(x, cy - h / 2.0, w, h, h / 2.0, bg);
        let kx = if on { x + w - h / 2.0 } else { x + h / 2.0 };
        self.cv.circle(kx, cy, h / 2.0 - 3.0, PAPER);
        self.push(x - 6.0, cy - 16.0, w + 12.0, 32.0, a);
    }

    fn slider(&mut self, kind: Slider, frac: f32, value: &str) {
        let (x0, x1, cy) = Settings::slider_track(kind);
        let a = SAction::Slider(kind);
        let f = frac.clamp(0.0, 1.0);
        self.cv.rrect(x0, cy - 2.0, x1 - x0, 4.0, 2.0, TRACK);
        if kind == Slider::Position {
            let mid = (x0 + x1) / 2.0;
            let kx = x0 + (x1 - x0) * f;
            let (l, r) = if kx < mid { (kx, mid) } else { (mid, kx) };
            self.cv.rrect(l, cy - 2.0, (r - l).max(0.1), 4.0, 2.0, ACCENT);
            self.cv.rrect(mid - 0.75, cy - 6.0, 1.5, 12.0, 0.75, FAINT);
        } else {
            self.cv.rrect(x0, cy - 2.0, ((x1 - x0) * f).max(4.0), 4.0, 2.0, ACCENT);
        }
        let active = self.is_hot(a) || self.drag == Some(kind);
        let (kx, kr) = (x0 + (x1 - x0) * f, if active { 8.5 } else { 7.5 });
        self.cv.circle(kx, cy, kr, PAPER);
        self.cv.stroke(tiny_skia::PathBuilder::from_circle(kx, cy, kr), 1.0, BORDER);
        self.cv.text_right(x0 - 14.0, cy + 4.5, 13.0, Weight::Display, MUTED, value);
        self.push(x0 - 10.0, cy - 14.0, x1 - x0 + 20.0, 28.0, a);
    }

    fn center_button(&mut self, cy: f32) {
        let a = SAction::CenterPos;
        let (x, s) = (X1 - 24.0, 24.0);
        let hot = self.is_hot(a);
        self.cv.rrect(x, cy - s / 2.0, s, s, 6.0, if hot { SURFACE_HOT } else { SURFACE });
        self.cv.rrect(x + s / 2.0 - 0.75, cy - 6.0, 1.5, 12.0, 0.75, if hot { INK } else { MUTED });
        self.push(x, cy - s / 2.0, s, s, a);
    }

    fn stepper(&mut self, cy: f32, value: &str, minus: SAction, plus: SAction) {
        let bs = 28.0;
        let vw = 72.0;
        let x = X1 - (bs * 2.0 + vw);
        self.cv.rrect(x, cy - bs / 2.0, bs * 2.0 + vw, bs, 6.0, SURFACE);
        for (bx, a, glyph) in [(x, minus, "−"), (X1 - bs, plus, "+")] {
            if self.is_hot(a) {
                self.cv.rrect(bx, cy - bs / 2.0, bs, bs, 6.0, SURFACE_HOT);
            }
            self.cv.text_center(bx + bs / 2.0, cy + 5.5, 15.0, Weight::Regular, INK, glyph);
            self.push(bx, cy - bs / 2.0, bs, bs, a);
        }
        self.cv.text_center(x + bs + vw / 2.0, cy + 4.5, 13.0, Weight::Display, INK, value);
    }

    fn monitors(&mut self, cy: f32, n: usize, current: usize) {
        let h = 28.0;
        let labels: Vec<String> =
            (0..n).map(|i| if i == 0 { "Utama".into() } else { format!("Layar {}", i + 1) }).collect();
        let widths: Vec<f32> = labels.iter().map(|l| self.cv.measure(12.0, Weight::Semibold, l) + 22.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + 4.0;
        let mut x = X1 - total;
        self.cv.rrect(x, cy - h / 2.0, total, h, 7.0, BG);
        x += 2.0;
        for (i, (l, w)) in labels.iter().zip(widths).enumerate() {
            let a = SAction::Monitor(i);
            let on = i == current;
            if on || self.is_hot(a) {
                self.cv.rrect(x, cy - h / 2.0 + 2.0, w, h - 4.0, 5.0, if on { SURFACE_HOT } else { SURFACE });
            }
            self.cv.text_center(x + w / 2.0, cy + 4.5, 12.0, Weight::Semibold, if on { INK } else { MUTED }, l);
            self.push(x, cy - h / 2.0, w, h, a);
            x += w;
        }
    }

    fn swatches(&mut self, cy: f32, current: &str) {
        let step = 26.0;
        for (i, hex_s) in PALETTE.iter().enumerate() {
            let cx = X1 - 10.0 - (PALETTE.len() - 1 - i) as f32 * step;
            let c = config::parse_hex(hex_s).map(|c| Rgba::rgb(c[0], c[1], c[2])).unwrap_or(INK);
            let a = SAction::Accent(i);
            let selected = current.eq_ignore_ascii_case(hex_s);
            if selected || self.is_hot(a) {
                self.cv.stroke(
                    tiny_skia::PathBuilder::from_circle(cx, cy, 12.0),
                    1.5,
                    if selected { INK } else { FAINT },
                );
            }
            self.cv.circle(cx, cy, 9.0, c);
            self.push(cx - 12.0, cy - 12.0, 24.0, 24.0, a);
        }
    }

    fn button(&mut self, x: f32, y: f32, label: &str, a: SAction) -> f32 {
        let w = self.cv.measure(12.5, Weight::Semibold, label) + 28.0;
        let hot = self.is_hot(a);
        self.cv.rrect(x, y, w, 32.0, 6.0, if hot { SURFACE_HOT } else { SURFACE });
        self.cv.text_center(x + w / 2.0, y + 20.5, 12.5, Weight::Semibold, INK, label);
        self.push(x, y, w, 32.0, a);
        w
    }
}

fn row_text(ui: &mut Ui, cy: f32, label: &str, desc: &str) {
    ui.cv.text(X0, cy - 2.0, 13.5, Weight::Regular, INK, label);
    let d = ui.cv.fit(11.5, Weight::Regular, desc, 300.0);
    ui.cv.text(X0, cy + 15.0, 11.5, Weight::Regular, MUTED, &d);
}

fn divider(ui: &mut Ui, y: f32) {
    ui.cv.rrect(X0, y - 0.5, X1 - X0, 1.0, 0.0, LINE);
}

/// Rows from `top`, each ROW_H tall, with hairlines between them.
fn rows_with_dividers(ui: &mut Ui, rows: &[(&str, &str)], top: f32) {
    for (i, (l, d)) in rows.iter().enumerate() {
        let cy = top + i as f32 * ROW_H + ROW_H / 2.0;
        row_text(ui, cy, l, d);
        if i + 1 < rows.len() {
            divider(ui, cy + ROW_H / 2.0);
        }
    }
}

// ---------------------------------------------------------------------
// regions
// ---------------------------------------------------------------------

fn nav_icon(cv: &mut Canvas, i: usize, cx: f32, cy: f32, c: Rgba) {
    match i {
        // Umum: two sliders.
        0 => {
            for (dy, kx) in [(-3.5f32, 3.0f32), (3.5, -3.0)] {
                cv.line(cx - 7.0, cy + dy, cx + 7.0, cy + dy, 1.4, c);
                cv.circle(cx + kx, cy + dy, 2.4, c);
            }
        }
        // Tampilan: a screen with the island.
        1 => {
            cv.stroke(rrect_path(cx - 7.5, cy - 6.0, 15.0, 12.0, 3.0), 1.4, c);
            cv.rrect(cx - 3.5, cy - 4.0, 7.0, 2.6, 1.3, c);
        }
        // Musik: a note.
        2 => {
            cv.circle(cx - 2.5, cy + 4.0, 2.6, c);
            cv.line(cx - 0.1, cy + 4.0, cx - 0.1, cy - 6.0, 1.4, c);
            cv.line(cx - 0.1, cy - 6.0, cx + 5.0, cy - 4.0, 1.4, c);
        }
        // Sistem: bars.
        3 => {
            for (k, h) in [(-5.0f32, 6.0f32), (0.0, 11.0), (5.0, 8.0)] {
                cv.rrect(cx + k - 1.2, cy + 6.0 - h, 2.4, h, 1.2, c);
            }
        }
        // Produktivitas: 2x2 grid.
        4 => {
            for (dx, dy) in [(-4.0f32, -4.0f32), (4.0, -4.0), (-4.0, 4.0), (4.0, 4.0)] {
                cv.rrect(cx + dx - 3.0, cy + dy - 3.0, 6.0, 6.0, 1.5, c);
            }
        }
        // Tentang: the island.
        _ => cv.rrect(cx - 7.0, cy - 3.0, 14.0, 6.0, 3.0, c),
    }
}

fn sidebar(ui: &mut Ui, section: usize, logo: &Pixmap) {
    ui.cv.image(logo, 20.0, 18.0, 28.0, 0.0);
    let tw = ui.cv.text(58.0, 38.0, 19.0, Weight::Display, INK, "Isla");
    ui.cv.text(58.0 + tw + 8.0, 38.0, 11.5, Weight::Regular, FAINT, &format!("v{}", crate::update::VERSION));

    for (i, label) in SECTIONS.iter().enumerate() {
        let y = 76.0 + i as f32 * 38.0;
        let a = SAction::Section(i);
        let on = i == section;
        if on || ui.is_hot(a) {
            ui.cv.rrect(12.0, y, SIDE_W - 24.0, 34.0, 7.0, if on { SURFACE } else { NAV_HOT });
        }
        nav_icon(&mut ui.cv, i, 34.0, y + 17.0, if on { ACCENT } else { FAINT });
        let (wt, col) = if on { (Weight::Semibold, INK) } else { (Weight::Regular, MUTED) };
        ui.cv.text(54.0, y + 21.5, 13.0, wt, col, label);
        ui.push(12.0, y, SIDE_W - 24.0, 34.0, a);
    }

    ui.cv.text(20.0, H - 22.0, 11.0, Weight::Regular, FAINT, "Disimpan otomatis");
}

fn window_buttons(ui: &mut Ui) {
    for (i, a) in [SAction::Minimize, SAction::Close].into_iter().enumerate() {
        let bx = W - 92.0 + i as f32 * 46.0;
        let hot = ui.is_hot(a);
        if hot {
            ui.cv.rrect(bx, 0.0, 46.0, 34.0, 0.0, if a == SAction::Close { DANGER } else { SURFACE });
        }
        let (cx, cy) = (bx + 23.0, 17.0);
        let col = if hot && a == SAction::Close { PAPER } else if hot { INK } else { MUTED };
        if a == SAction::Close {
            ui.cv.line(cx - 5.0, cy - 5.0, cx + 5.0, cy + 5.0, 1.2, col);
            ui.cv.line(cx + 5.0, cy - 5.0, cx - 5.0, cy + 5.0, 1.2, col);
        } else {
            ui.cv.line(cx - 5.0, cy, cx + 5.0, cy, 1.2, col);
        }
        ui.push(bx, 0.0, 46.0, 34.0, a);
    }
}

/// A miniature desktop with the notch at the chosen position, size and accent.
fn preview(ui: &mut Ui, d: &Config) {
    let (x, y, w, h) = (X0, PREVIEW_Y, X1 - X0, PREVIEW_H);
    ui.cv.rrect(x, y, w, h, 10.0, hex(0x202226));
    // Two faint windows so the scene reads as a desktop.
    ui.cv.rrect(x + 30.0, y + 44.0, 190.0, 78.0, 6.0, hex(0x2B2E33));
    ui.cv.rrect(x + 250.0, y + 58.0, 220.0, 70.0, 6.0, hex(0x282A2F));
    ui.cv.stroke(rrect_path(x, y, w, h, 10.0), 1.0, LINE);

    let scale = 0.85 * d.size.clamp(0.6, 2.0);
    let (nw, nh) = (200.0 * scale, 32.0 * scale);
    let free = w / 2.0 - nw / 2.0 - 14.0;
    let cx = x + w / 2.0 + d.position.clamp(-1.0, 1.0) * free;
    let accent = config::parse_hex(&d.accent).map(|c| Rgba::rgb(c[0], c[1], c[2])).unwrap_or(ACCENT);
    let path = notch_path(cx, nw, nh, nh * 0.5, 5.0 * scale)
        .and_then(|p| p.transform(tiny_skia::Transform::from_translate(0.0, y)));
    ui.cv.fill(path, hex(0x000000));
    ui.cv.text(cx - nw / 2.0 + 10.0 * scale, y + nh / 2.0 + 4.0 * scale, 12.0 * scale, Weight::Semibold, hex(0xF2F2F2), "21:40");
    ui.cv.equalizer(cx + nw / 2.0 - 24.0 * scale, y + 9.0 * scale, 14.0 * scale, 14.0 * scale, 1.3, true, accent);
}

fn shortcuts(ui: &mut Ui, d: &Config, icons: &[Option<Pixmap>], top: f32) {
    let (tw, th, gap) = (76.0, 72.0, 10.0);
    let count = d.shortcuts.len().min(6);
    for i in 0..=count {
        let x = X0 + i as f32 * (tw + gap);
        if i == count {
            if count < 6 {
                let a = SAction::ShortcutAdd;
                let hot = ui.is_hot(a);
                ui.cv.stroke_dashed(rrect_path(x, top, tw, th, 10.0), 1.2, 5.0, if hot { INK } else { FAINT });
                let col = if hot { INK } else { MUTED };
                ui.cv.line(x + tw / 2.0 - 7.0, top + 28.0, x + tw / 2.0 + 7.0, top + 28.0, 1.6, col);
                ui.cv.line(x + tw / 2.0, top + 21.0, x + tw / 2.0, top + 35.0, 1.6, col);
                ui.cv.text_center(x + tw / 2.0, top + 58.0, 11.5, Weight::Regular, col, "Tambah");
                ui.push(x, top, tw, th, a);
            }
            break;
        }
        let sc = &d.shortcuts[i];
        let a = SAction::ShortcutRemove(i);
        let hot = ui.is_hot(a);
        ui.cv.rrect(x, top, tw, th, 10.0, if hot { SURFACE_HOT } else { SURFACE });
        match icons.get(i).and_then(|o| o.as_ref()) {
            Some(img) => ui.cv.image(img, x + tw / 2.0 - 15.0, top + 11.0, 30.0, 0.0),
            None => {
                let l: String = sc.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                ui.cv.text_center(x + tw / 2.0, top + 34.0, 18.0, Weight::Display, MUTED, &l);
            }
        }
        let name = ui.cv.fit(11.5, Weight::Regular, &sc.name, tw - 10.0);
        ui.cv.text_center(x + tw / 2.0, top + 59.0, 11.5, Weight::Regular, if hot { INK } else { MUTED }, &name);
        if hot {
            // The remove affordance shows on hover only.
            let (bx, by) = (x + tw - 10.0, top + 10.0);
            ui.cv.circle(bx, by, 7.0, DANGER);
            ui.cv.line(bx - 2.5, by - 2.5, bx + 2.5, by + 2.5, 1.4, PAPER);
            ui.cv.line(bx + 2.5, by - 2.5, bx - 2.5, by + 2.5, 1.4, PAPER);
        }
        ui.push(x, top, tw, th, a);
    }
}

fn about(ui: &mut Ui, logo: &Pixmap, update: Option<&str>) {
    let y = 64.0;
    ui.cv.image(logo, X0, y, 72.0, 0.0);
    ui.cv.text(X0 + 92.0, y + 36.0, 30.0, Weight::Display, INK, "Isla");
    ui.cv.text(X0 + 92.0, y + 59.0, 12.5, Weight::Regular, MUTED, &format!("Versi {}", crate::update::VERSION));

    let body = [
        "Dynamic Island untuk Windows. Ditulis dengan Rust, tanpa runtime,",
        "satu file exe. Data cuaca dari Open-Meteo, lirik dari LRCLIB.",
    ];
    for (i, l) in body.iter().enumerate() {
        ui.cv.text(X0, y + 112.0 + i as f32 * 20.0, 13.0, Weight::Regular, INK, l);
    }

    let by = y + 158.0;
    let mut x = X0;
    if let Some(tag) = update {
        let label = format!("Perbarui ke {tag}");
        let a = SAction::InstallUpdate;
        let w = ui.cv.measure(12.5, Weight::Semibold, &label) + 28.0;
        let hot = ui.is_hot(a);
        ui.cv.rrect(x, by, w, 32.0, 6.0, if hot { hex(0xFF6A45) } else { ACCENT });
        ui.cv.text_center(x + w / 2.0, by + 20.5, 12.5, Weight::Semibold, PAPER, &label);
        ui.push(x, by, w, 32.0, a);
        x += w + 8.0;
    }
    x += ui.button(x, by, "Buka config.toml", SAction::OpenConfig) + 8.0;
    x += ui.button(x, by, "Buka folder data", SAction::OpenFolder) + 8.0;
    if !crate::update::REPOSITORY.is_empty() {
        ui.button(x, by, "Halaman rilis", SAction::OpenRelease);
    }

    divider(ui, by + 60.0);
    let tips = [
        ("Geser notch", "Saat notch terbuka, tahan klik di area kosong lalu seret."),
        ("Volume", "Scroll di atas notch."),
        ("Shelf", "Seret file ke notch. Klik kanan item untuk menghapus."),
        ("Menu", "Klik kanan notch atau ikon tray."),
    ];
    for (i, (k, v)) in tips.iter().enumerate() {
        let ty = by + 92.0 + i as f32 * 26.0;
        ui.cv.text(X0, ty, 12.5, Weight::Semibold, INK, k);
        ui.cv.text(X0 + 110.0, ty, 12.5, Weight::Regular, MUTED, v);
    }
}

// ---------------------------------------------------------------------
// file picker + window proc
// ---------------------------------------------------------------------

/// File picker for a new shortcut (starts in the Start menu programs).
/// Runs a modal loop: call it outside the app borrow.
pub fn pick_file(owner: HWND) -> Option<String> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let types = [
            COMDLG_FILTERSPEC { pszName: w!("Aplikasi dan shortcut"), pszSpec: w!("*.exe;*.lnk;*.url;*.bat;*.cmd") },
            COMDLG_FILTERSPEC { pszName: w!("Semua file"), pszSpec: w!("*.*") },
        ];
        let _ = dlg.SetFileTypes(&types);
        let _ = dlg.SetTitle(w!("Pilih aplikasi untuk shortcut"));
        let programs = std::env::var("ProgramData")
            .map(|p| format!("{p}\\Microsoft\\Windows\\Start Menu\\Programs"))
            .unwrap_or_default();
        if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(programs), None) {
            let _ = dlg.SetFolder(&folder);
        }
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

pub enum Out {
    Handled(LRESULT),
    Default,
    PickShortcut,
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match crate::with_app(|a| a.settings_msg(hwnd, msg, wp, lp)) {
            Some(Out::Handled(r)) => r,
            Some(Out::PickShortcut) => {
                if let Some(path) = pick_file(hwnd) {
                    crate::with_app(|a| a.settings_add_shortcut(path));
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
