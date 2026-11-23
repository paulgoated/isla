//! The notch window state: hover/expand logic, HUD events, tabs, focus
//! timer, shelf drag & drop, config reload, and presenting through
//! UpdateLayeredWindow. Drawing lives in `views`.

use crate::anim::{approach, Spring};
use crate::clip;
use crate::config::{self, Config};
use crate::draw::*;
use crate::settings::{self, Settings};
use crate::shelf::{self, Shelf};
use crate::system;
use crate::text::Fonts;
use crate::views::{self, Ctx};
use crate::worker::{Cmd, Shared};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tiny_skia::{Color, FillRule, Mask, Pixmap, Transform};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_CAPITAL, VK_LBUTTON, VK_NUMLOCK};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Largest notch size (design units, 96 DPI).
pub const MAX_W: f32 = 560.0;
pub const MAX_H: f32 = 214.0;
/// Concave fillet joining the notch to the top edge.
const EAR: f32 = 8.0;
/// Room for the drop shadow and privacy dots around the notch.
const PAD: f32 = 30.0;

const LEAVE_DELAY: Duration = Duration::from_millis(320);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hud {
    Volume,
    Power,
    Track,
    TimerDone,
    Lock,
    Mic,
    Output,
    Notif,
    Config,
    Reminder,
    Alarm,
    Bluetooth,
    Update,
    Screenshot,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Layout {
    Idle,
    Music,
    /// Music with the current lyric line in the middle.
    MusicLyric,
    /// Focus timer, reminder or stopwatch running.
    Timer,
    Hud(Hud),
    Expanded,
}

impl Layout {
    pub fn size(self) -> (f32, f32) {
        match self {
            Layout::Idle => (200.0, 32.0),
            Layout::Music | Layout::Timer => (240.0, 32.0),
            Layout::MusicLyric => (420.0, 32.0),
            Layout::Hud(Hud::Track) => (372.0, 72.0),
            Layout::Hud(Hud::Notif) => (400.0, 76.0),
            Layout::Hud(Hud::Lock) => (250.0, 44.0),
            Layout::Hud(Hud::Config) | Layout::Hud(Hud::Screenshot) => (360.0, 44.0),
            Layout::Hud(Hud::Update) => (380.0, 44.0),
            Layout::Hud(_) => (320.0, 44.0),
            Layout::Expanded => (MAX_W, MAX_H),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tab {
    Home,
    System,
    Tools,
    Clip,
    Shelf,
    Notifs,
}

#[derive(Default)]
pub struct Stopwatch {
    pub since: Option<Instant>,
    pub acc: Duration,
}

impl Stopwatch {
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.acc + self.since.map_or(Duration::ZERO, |s| now.saturating_duration_since(s))
    }
    pub fn running(&self) -> bool {
        self.since.is_some()
    }
}

pub struct Alarm {
    pub hour: u16,
    pub minute: u16,
    pub armed: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Prev,
    PlayPause,
    Next,
    Seek,
    Mute,
    Mic,
    Output,
    Focus,
    Tab(Tab),
    Shortcut(usize),
    ShelfItem(usize),
    ShelfClear,
    NotifClear,
    Source,
    SwToggle,
    SwReset,
    Remind(u32),
    RemindCancel,
    Alarm,
    ShotFull,
    ShotArea,
    ClipItem(usize),
    ClipClear,
    OpenUpdate,
}

pub struct FocusTimer {
    pub end: Instant,
    pub total: Duration,
}

impl FocusTimer {
    pub fn remaining(&self, now: Instant) -> Duration {
        self.end.saturating_duration_since(now)
    }
}

/// 32-bit top-down DIB the frame is copied into for UpdateLayeredWindow.
struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl Surface {
    fn new(w: i32, h: i32) -> Option<Surface> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
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
            let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp.into());
            Some(Surface {
                dc,
                bmp,
                old,
                bits: bits as *mut u8,
                len: (w * h * 4) as usize,
            })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

pub type Buttons = Vec<([f32; 4], Action)>;

pub struct App {
    hwnd: HWND,
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Cmd>,
    fonts: Fonts,
    cfg: Arc<Config>,
    cfg_mtime: Option<SystemTime>,
    next_cfg_check: Instant,

    s: f32,
    win_x: i32,
    win_y: i32,
    win_w: i32,
    win_h: i32,
    pixmap: Pixmap,
    surface: Option<Surface>,

    w: Spring,
    h: Spring,
    vis: f32,
    shown: Layout,
    fade: f32,
    start: Instant,
    last: Instant,
    interval: u32,

    expanded: bool,
    tab: Tab,
    hover_since: Option<Instant>,
    leave_since: Option<Instant>,
    buttons: Buttons,
    hot: Option<Action>,
    pressed: Option<Action>,

    shelf: Shelf,
    file_drag: bool,
    dragging_out: bool,
    drag_start: Option<(f32, f32, usize)>,
    shortcut_icons: Vec<Option<Pixmap>>,

    hud: Option<(Hud, Instant)>,
    seen_vol: u64,
    seen_track: u64,
    seen_mic: u64,
    seen_output: u64,
    seen_notif: u64,
    last_ac: Option<bool>,
    last_locks: Option<(bool, bool)>,
    lock_hud: (&'static str, bool),
    config_msg: String,

    timer: Option<FocusTimer>,
    timer_minutes: u32,
    done_minutes: u32,

    snap: Shared,
    fullscreen: bool,
    next_fs_check: Instant,
    next_topmost: Instant,
    next_prune: Instant,

    settings: Option<Settings>,
    mon: RECT,
    /// Live position while dragging the notch or the settings slider.
    pos_override: Option<f32>,
    /// (cursor x, window x) at the start of a notch move.
    moving: Option<(i32, i32)>,

    clip: clip::History,
    stopwatch: Stopwatch,
    reminders: Vec<FocusTimer>,
    alarm: Alarm,
    /// Full-screen capture scheduled after the notch has hidden itself.
    pending_shot: Option<Instant>,
    /// Waiting for the Windows snipping overlay to put an image on the clipboard.
    screenclip_until: Option<Instant>,
    /// Text for HUDs that carry a message (screenshot, Bluetooth, update).
    hud_text: String,
    seen_bt: Vec<String>,
    seen_update: bool,
    install: Arc<Mutex<Option<crate::update::InstallState>>>,
}

impl App {
    /// `load` is the result of the initial `config::load()` done in main.
    pub fn new(hwnd: HWND, shared: Arc<Mutex<Shared>>, tx: Sender<Cmd>, load: Result<(), String>) -> App {
        let now = Instant::now();
        let cfg = config::current();
        let (w, h) = Layout::Idle.size();
        let mut app = App {
            hwnd,
            shared,
            tx,
            fonts: Fonts::new(),
            cfg_mtime: config::modified(),
            next_cfg_check: now,
            s: 1.0,
            win_x: 0,
            win_y: 0,
            win_w: 0,
            win_h: 0,
            pixmap: Pixmap::new(1, 1).unwrap(),
            surface: None,
            w: Spring::new(w * 0.6),
            h: Spring::new(h * 0.6),
            vis: 0.0,
            shown: Layout::Idle,
            fade: 0.0,
            start: now,
            last: now,
            interval: 16,
            expanded: false,
            tab: Tab::Home,
            hover_since: None,
            leave_since: None,
            buttons: Vec::new(),
            hot: None,
            pressed: None,
            shelf: Shelf::load(),
            file_drag: false,
            dragging_out: false,
            drag_start: None,
            shortcut_icons: Vec::new(),
            hud: None,
            seen_vol: 0,
            seen_track: 0,
            seen_mic: 0,
            seen_output: 0,
            seen_notif: 0,
            last_ac: None,
            last_locks: None,
            lock_hud: ("", false),
            config_msg: String::new(),
            timer: None,
            timer_minutes: cfg.focus_minutes.clamp(1, 240),
            done_minutes: 25,
            snap: Shared::default(),
            fullscreen: false,
            next_fs_check: now,
            next_topmost: now,
            next_prune: now,
            settings: None,
            mon: RECT::default(),
            pos_override: None,
            moving: None,
            clip: clip::History::default(),
            stopwatch: Stopwatch::default(),
            reminders: Vec::new(),
            alarm: Alarm { hour: 7, minute: 0, armed: false },
            pending_shot: None,
            screenclip_until: None,
            hud_text: String::new(),
            seen_bt: Vec::new(),
            seen_update: false,
            install: Arc::new(Mutex::new(None)),
            cfg,
        };
        app.load_shortcut_icons();
        if let Err(e) = load {
            app.show_config_msg(format!("Pengaturan error: {e}"), now);
        }
        app
    }

    fn load_shortcut_icons(&mut self) {
        self.shortcut_icons = self
            .cfg
            .shortcuts
            .iter()
            .take(6)
            .map(|s| {
                // An explicit icon file is shown as its own picture (thumbnail);
                // apps use their embedded icon.
                if s.icon.is_empty() {
                    shelf::shell_image(&s.path, 64, true)
                } else {
                    shelf::shell_image(&s.icon, 64, false)
                }
            })
            .collect();
    }

    fn show_config_msg(&mut self, msg: String, now: Instant) {
        self.config_msg = msg;
        self.hud = Some((Hud::Config, now + Duration::from_millis(3500)));
    }

    /// `external` = config.toml was edited by hand (show a HUD and refresh
    /// an open settings window); otherwise the change came from the app.
    fn reload_config(&mut self, now: Instant, external: bool) {
        match config::load() {
            Ok(()) => {
                let old = self.cfg.clone();
                self.cfg = config::current();
                self.cfg_mtime = config::modified();
                if self.timer.is_none() {
                    self.timer_minutes = self.cfg.focus_minutes.clamp(1, 240);
                }
                if old.shortcuts != self.cfg.shortcuts {
                    self.load_shortcut_icons();
                }
                self.relayout();
                if external {
                    self.show_config_msg("Pengaturan diperbarui".into(), now);
                    if let Some(st) = self.settings.as_mut() {
                        st.sync(&self.cfg);
                    }
                }
            }
            Err(e) => self.show_config_msg(format!("Pengaturan error: {e}"), now),
        }
    }

    /// Writes `c` to config.toml and applies it without a HUD.
    fn save_config(&mut self, c: &Config) {
        if config::save(c).is_ok() {
            self.reload_config(Instant::now(), false);
        }
    }

    // ------------------------------------------------------------------
    // settings window
    // ------------------------------------------------------------------

    pub fn open_settings(&mut self) {
        match &self.settings {
            Some(st) => unsafe {
                let _ = ShowWindow(st.hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(st.hwnd);
            },
            None => self.settings = Settings::open(&self.cfg),
        }
    }

    fn settings_commit(&mut self) {
        let Some(st) = &self.settings else { return };
        let mut draft = st.draft.clone();
        draft.weather.city = st.city_text();
        if let Some(st) = self.settings.as_mut() {
            st.draft.weather.city = draft.weather.city.clone();
            st.invalidate();
        }
        self.save_config(&draft);
    }

    pub fn settings_add_shortcut(&mut self, path: String) {
        let Some(st) = self.settings.as_mut() else { return };
        if st.draft.shortcuts.len() >= 6 {
            return;
        }
        let name = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "App".into());
        st.draft.shortcuts.push(config::Shortcut { name, path, args: String::new(), icon: String::new() });
        self.settings_commit();
    }

    pub fn settings_msg(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> settings::Out {
        use settings::{Out, SAction};
        let Some(st) = self.settings.as_mut() else { return Out::Default };
        if st.hwnd != hwnd {
            return Out::Default;
        }
        let (x, y) = (
            (lp.0 & 0xffff) as u16 as i16 as f32,
            ((lp.0 >> 16) & 0xffff) as u16 as i16 as f32,
        );
        unsafe {
            match msg {
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    st.update_tag = self.snap.update.as_ref().map(|u| u.tag.clone());
                    st.render(&mut self.fonts, &self.shortcut_icons);
                    st.blit(hdc);
                    let _ = EndPaint(hwnd, &ps);
                    Out::Handled(LRESULT(0))
                }
                WM_ERASEBKGND => Out::Handled(LRESULT(1)),
                WM_NCHITTEST => {
                    let mut p = POINT { x: x as i32, y: y as i32 };
                    let _ = ScreenToClient(hwnd, &mut p);
                    let ht = if st.is_caption(p.x as f32, p.y as f32) { HTCAPTION } else { HTCLIENT };
                    Out::Handled(LRESULT(ht as isize))
                }
                WM_CTLCOLOREDIT => Out::Handled(st.field_colors(HDC(wp.0 as _))),
                WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
                    let cur = if st.hot.is_some() { IDC_HAND } else { IDC_ARROW };
                    SetCursor(LoadCursorW(None, cur).ok());
                    Out::Handled(LRESULT(1))
                }
                WM_MOUSEMOVE => {
                    if !st.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                        st.tracking = true;
                    }
                    if let Some(k) = st.drag {
                        st.set_slider(k, x);
                        if k == settings::Slider::Position {
                            self.pos_override = Some(st.draft.position);
                            self.relayout();
                        }
                    } else {
                        let hot = st.hit(x, y);
                        if hot != st.hot {
                            st.hot = hot;
                            st.invalidate();
                        }
                    }
                    Out::Handled(LRESULT(0))
                }
                0x02A3 /* WM_MOUSELEAVE */ => {
                    st.tracking = false;
                    if st.drag.is_none() && st.hot.is_some() {
                        st.hot = None;
                        st.invalidate();
                    }
                    Out::Handled(LRESULT(0))
                }
                WM_LBUTTONDOWN => {
                    let Some(a) = st.hit(x, y) else { return Out::Handled(LRESULT(0)) };
                    let d = &mut st.draft;
                    match a {
                        SAction::Slider(k) => {
                            st.drag = Some(k);
                            SetCapture(hwnd);
                            st.set_slider(k, x);
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::Autostart => {
                            st.autostart = !st.autostart;
                            system::set_autostart(st.autostart);
                            st.invalidate();
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::OpenConfig => {
                            shelf::open("notepad.exe", &format!("\"{}\"", config::path().display()));
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::ShortcutAdd => return Out::PickShortcut,
                        SAction::Section(i) => {
                            // Commit a half-typed city before its field hides.
                            let leaving_city = st.city_text() != st.draft.weather.city;
                            st.set_section(i);
                            if leaving_city {
                                self.settings_commit();
                            }
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::OpenFolder => {
                            shelf::open(&config::dir().to_string_lossy(), "");
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::InstallUpdate => {
                            self.install_update();
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::OpenRelease => {
                            shelf::open(&format!("{}/releases", crate::update::REPOSITORY.trim_end_matches('/')), "");
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::Minimize => {
                            st.hot = None;
                            let _ = ShowWindow(hwnd, SW_MINIMIZE);
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::Close => {
                            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                            return Out::Handled(LRESULT(0));
                        }
                        SAction::HideFs => d.hide_in_fullscreen = !d.hide_in_fullscreen,
                        SAction::Monitor(i) => {
                            d.monitor = i;
                            d.position = 0.0;
                        }
                        SAction::CenterPos => d.position = 0.0,
                        SAction::FocusMinus => d.focus_minutes = d.focus_minutes.saturating_sub(5).max(5),
                        SAction::FocusPlus => d.focus_minutes = (d.focus_minutes + 5).min(120),
                        SAction::Accent(i) => d.accent = settings::PALETTE[i].to_string(),
                        SAction::Fahrenheit => d.weather.fahrenheit = !d.weather.fahrenheit,
                        SAction::Feature(i) => {
                            if let Some(f) = d.features.flag_mut(i) {
                                *f = !*f;
                            }
                        }
                        SAction::ShortcutRemove(i) => {
                            if i < d.shortcuts.len() {
                                d.shortcuts.remove(i);
                            }
                        }
                    }
                    st.invalidate();
                    self.settings_commit();
                    Out::Handled(LRESULT(0))
                }
                WM_LBUTTONUP => {
                    if st.drag.take().is_some() {
                        let _ = ReleaseCapture();
                        self.pos_override = None;
                        self.settings_commit();
                    }
                    Out::Handled(LRESULT(0))
                }
                WM_COMMAND if ((wp.0 >> 16) & 0xffff) as u32 == EN_CHANGE => {
                    // Debounce typing in the city field.
                    SetTimer(Some(hwnd), 2, 900, None);
                    Out::Handled(LRESULT(0))
                }
                WM_TIMER if wp.0 == 2 => {
                    let _ = KillTimer(Some(hwnd), 2);
                    self.settings_commit();
                    Out::Handled(LRESULT(0))
                }
                WM_CLOSE => {
                    self.settings_commit();
                    // Drop our state first: DestroyWindow re-enters the proc.
                    self.settings = None;
                    let _ = DestroyWindow(hwnd);
                    Out::Handled(LRESULT(0))
                }
                _ => Out::Default,
            }
        }
    }

    /// Size and place the window at the top center of the chosen monitor.
    pub fn relayout(&mut self) {
        self.relayout_hidden();
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                self.win_x,
                self.win_y,
                self.win_w,
                self.win_h,
                SWP_NOACTIVATE,
            );
        }
        self.render();
        self.present();
    }

    /// Computes scale, window rect and buffers without touching the window.
    pub fn relayout_hidden(&mut self) {
        unsafe {
            let mon = pick_monitor(self.cfg.monitor);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            self.s = dx.max(48) as f32 / 96.0 * self.cfg.size.clamp(0.6, 2.0);

            let w = ((MAX_W + 2.0 * (EAR + PAD)) * self.s).ceil() as i32;
            let h = ((MAX_H + PAD) * self.s).ceil() as i32;
            let r = mi.rcMonitor;
            self.mon = r;
            self.win_x = self.x_for(self.pos_override.unwrap_or(self.cfg.position), w);
            self.win_y = r.top;
            if (w, h) != (self.win_w, self.win_h) || self.surface.is_none() {
                self.win_w = w;
                self.win_h = h;
                self.pixmap = Pixmap::new(w as u32, h as u32).unwrap();
                self.surface = None;
                self.surface = Surface::new(w, h);
            }
        }
    }

    /// Window x for a position in -1..1 (0 = centered on the monitor).
    fn x_for(&self, position: f32, w: i32) -> i32 {
        let r = self.mon;
        let free = ((r.right - r.left - w) / 2) as f32;
        r.left + (r.right - r.left - w) / 2 + (position.clamp(-1.0, 1.0) * free).round() as i32
    }

    /// Inverse of `x_for`.
    fn position_for(&self, x: i32) -> f32 {
        let r = self.mon;
        let free = ((r.right - r.left - self.win_w) / 2).max(1) as f32;
        let center = r.left + (r.right - r.left - self.win_w) / 2;
        ((x - center) as f32 / free).clamp(-1.0, 1.0)
    }

    fn tabs(&self) -> Vec<Tab> {
        let f = &self.cfg.features;
        let mut t = vec![Tab::Home];
        if f.system {
            t.push(Tab::System);
        }
        if f.tools {
            t.push(Tab::Tools);
        }
        if f.clipboard {
            t.push(Tab::Clip);
        }
        if f.shelf {
            t.push(Tab::Shelf);
        }
        if f.notifications {
            t.push(Tab::Notifs);
        }
        t
    }

    // ------------------------------------------------------------------
    // input
    // ------------------------------------------------------------------

    fn hit(&self, x: f32, y: f32) -> Option<Action> {
        if !self.expanded || self.fade < 0.5 || self.shown != Layout::Expanded {
            return None;
        }
        self.buttons
            .iter()
            .find(|(r, _)| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
            .map(|(_, a)| *a)
    }

    fn button_rect(&self, a: Action) -> Option<[f32; 4]> {
        self.buttons.iter().find(|(_, b)| *b == a).map(|(r, _)| *r)
    }

    pub fn hovering_button(&self) -> bool {
        self.hot.is_some()
    }

    pub fn mouse_down(&mut self, x: i32, y: i32) {
        let (x, y) = (x as f32, y as f32);
        self.pressed = self.hit(x, y);
        self.drag_start = match self.pressed {
            Some(Action::ShelfItem(i)) => Some((x, y, i)),
            _ => None,
        };
        // Empty area of the expanded notch: start moving it along the top edge.
        if self.pressed.is_none() && self.expanded && self.shown == Layout::Expanded {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
                SetCapture(self.hwnd);
            }
            self.moving = Some((p.x, self.win_x));
        }
    }

    /// Returns files to drag out of the shelf once the cursor moved far
    /// enough with the button held.
    pub fn mouse_move(&mut self, x: i32, y: i32, lbutton: bool) -> Option<Vec<String>> {
        self.wake();
        if let Some((start, win_x)) = self.moving {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            let pos = self.position_for(win_x + p.x - start);
            // Snap to the center.
            let pos = if pos.abs() < 0.03 { 0.0 } else { pos };
            self.pos_override = Some(pos);
            self.win_x = self.x_for(pos, self.win_w);
            self.present();
            return None;
        }
        let (sx, sy, i) = self.drag_start?;
        if !lbutton {
            self.drag_start = None;
            return None;
        }
        if (x as f32 - sx).hypot(y as f32 - sy) < 6.0 * self.s {
            return None;
        }
        self.drag_start = None;
        self.pressed = None;
        let path = self.shelf.items.get(i)?.path.clone();
        self.dragging_out = true;
        Some(vec![path])
    }

    pub fn after_drag_out(&mut self) {
        self.dragging_out = false;
        self.shelf.prune();
    }

    pub fn mouse_up(&mut self, x: i32, y: i32) {
        if self.moving.take().is_some() {
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Some(pos) = self.pos_override.take() {
                let pos = (pos * 1000.0).round() / 1000.0;
                if pos != self.cfg.position {
                    let mut c = (*self.cfg).clone();
                    c.position = pos;
                    self.save_config(&c);
                    if let Some(st) = self.settings.as_mut() {
                        st.draft.position = pos;
                        st.invalidate();
                    }
                }
            }
            self.pressed = None;
            return;
        }
        let a = self.hit(x as f32, y as f32);
        if let Some(a) = a {
            if Some(a) == self.pressed {
                self.perform(a, x as f32);
            }
        }
        self.pressed = None;
        self.drag_start = None;
    }

    /// Right click on a shelf item removes it; returns false to show the menu.
    pub fn right_click(&mut self, x: i32, y: i32) -> bool {
        match self.hit(x as f32, y as f32) {
            Some(Action::ShelfItem(i)) => self.shelf.remove(i),
            Some(Action::ClipItem(i)) => self.clip.remove(i),
            _ => return false,
        }
        true
    }

    /// Wheel: adjusts the focus preset over the focus chip, otherwise volume.
    pub fn wheel(&mut self, sx: i32, sy: i32, delta: i16) {
        let (x, y) = ((sx - self.win_x) as f32, (sy - self.win_y) as f32);
        let steps = delta as f32 / 120.0;
        if self.hit(x, y) == Some(Action::Focus) && self.timer.is_none() {
            let m = self.timer_minutes as i32 + if steps > 0.0 { 5 } else { -5 };
            self.timer_minutes = m.clamp(5, 120) as u32;
            return;
        }
        if self.hit(x, y) == Some(Action::Alarm) {
            let step = if steps > 0.0 { 5 } else { -5 };
            let total = (self.alarm.hour as i32 * 60 + self.alarm.minute as i32 + step).rem_euclid(24 * 60);
            self.alarm.hour = (total / 60) as u16;
            self.alarm.minute = (total % 60) as u16;
            return;
        }
        let v = {
            let mut sh = self.shared.lock().unwrap();
            sh.volume = (sh.volume + steps * 0.02).clamp(0.0, 1.0);
            if sh.volume > 0.0 {
                sh.muted = false;
            }
            sh.volume
        };
        let _ = self.tx.send(Cmd::SetVolume(v));
        if !self.expanded && self.cfg.features.volume_hud {
            self.hud = Some((Hud::Volume, Instant::now() + Duration::from_millis(1600)));
        }
        self.wake();
    }

    fn perform(&mut self, a: Action, x: f32) {
        let now = Instant::now();
        match a {
            Action::Prev => drop(self.tx.send(Cmd::Prev)),
            Action::Next => drop(self.tx.send(Cmd::Next)),
            Action::PlayPause => {
                let _ = self.tx.send(Cmd::PlayPause);
                // Optimistic flip so the icon reacts instantly.
                let mut sh = self.shared.lock().unwrap();
                if let Some(m) = sh.media.as_mut() {
                    m.position = m.position_now(now);
                    m.stamp = now;
                    m.playing = !m.playing;
                }
            }
            Action::Seek => {
                let Some(r) = self.button_rect(Action::Seek) else { return };
                let mut sh = self.shared.lock().unwrap();
                if let Some(m) = sh.media.as_mut() {
                    if m.duration > 0.0 {
                        let f = ((x - r[0]) / r[2]).clamp(0.0, 1.0) as f64;
                        let secs = f * m.duration;
                        m.position = secs;
                        m.stamp = now;
                        let _ = self.tx.send(Cmd::Seek(secs));
                    }
                }
            }
            Action::Mute => drop(self.tx.send(Cmd::ToggleMute)),
            Action::Mic => drop(self.tx.send(Cmd::ToggleMic)),
            Action::Output => drop(self.tx.send(Cmd::NextOutput)),
            Action::Focus => {
                if self.timer.take().is_none() {
                    let total = Duration::from_secs(self.timer_minutes as u64 * 60);
                    self.timer = Some(FocusTimer { end: now + total, total });
                }
            }
            Action::Tab(t) => self.tab = t,
            Action::Shortcut(i) => {
                if let Some(sc) = self.cfg.shortcuts.get(i) {
                    shelf::open(&sc.path, &sc.args);
                }
            }
            Action::ShelfItem(i) => {
                if let Some(it) = self.shelf.items.get(i) {
                    shelf::open(&it.path, "");
                }
            }
            Action::ShelfClear => self.shelf.clear(),
            Action::NotifClear => {
                let _ = self.tx.send(Cmd::ClearNotifs);
                self.shared.lock().unwrap().notifs.clear();
            }
            Action::Source => drop(self.tx.send(Cmd::NextSource)),
            Action::SwToggle => {
                let sw = &mut self.stopwatch;
                match sw.since.take() {
                    Some(s) => sw.acc += now.saturating_duration_since(s),
                    None => sw.since = Some(now),
                }
            }
            Action::SwReset => self.stopwatch = Stopwatch::default(),
            Action::Remind(min) => {
                let total = Duration::from_secs(min as u64 * 60);
                self.reminders.push(FocusTimer { end: now + total, total });
                self.reminders.sort_by_key(|r| r.end);
            }
            Action::RemindCancel => self.reminders.clear(),
            Action::Alarm => self.alarm.armed = !self.alarm.armed,
            Action::ShotFull => {
                // Hide the notch first, then capture on a later tick.
                self.expanded = false;
                self.pending_shot = Some(now + Duration::from_millis(350));
            }
            Action::ShotArea => {
                self.expanded = false;
                self.screenclip_until = Some(now + Duration::from_secs(90));
                shelf::open("ms-screenclip:", "");
            }
            Action::ClipItem(i) => self.clip.restore(i, self.hwnd),
            Action::ClipClear => self.clip.clear(),
            Action::OpenUpdate => self.install_update(),
        }
    }

    /// Tag of an available update, for the tray menu.
    pub fn update_tag(&self) -> Option<String> {
        self.shared.lock().unwrap().update.as_ref().map(|u| u.tag.clone())
    }

    /// Starts the one-click update (download, verify, swap, restart).
    pub fn install_update(&mut self) {
        let rel = self.shared.lock().unwrap().update.clone();
        let busy = matches!(*self.install.lock().unwrap(), Some(crate::update::InstallState::Downloading));
        if let (Some(rel), false) = (rel, busy) {
            if rel.asset.is_none() {
                // A release without isla.exe: fall back to its web page.
                shelf::open(&rel.url, "");
                return;
            }
            crate::update::install(rel, self.hwnd.0 as isize, self.install.clone());
            self.hud_text = "Mengunduh pembaruan…".into();
            self.hud = Some((Hud::Update, Instant::now() + Duration::from_secs(4)));
            self.wake();
        }
    }

    /// HUD shown by the new instance right after an update.
    pub fn show_updated(&mut self) {
        self.hud_text = format!("Isla diperbarui ke v{}", crate::update::VERSION);
        self.hud = Some((Hud::Update, Instant::now() + Duration::from_secs(5)));
    }

    /// WM_CLIPBOARDUPDATE: record history; a snip taken via "Pilih area"
    /// is saved as a screenshot and parked in the shelf.
    pub fn on_clipboard(&mut self) {
        if !self.cfg.features.clipboard && self.screenclip_until.is_none() {
            return;
        }
        let img = self.clip.on_update(self.hwnd);
        if !self.cfg.features.clipboard {
            self.clip.clear();
        }
        let now = Instant::now();
        if let (Some(until), Some(img)) = (self.screenclip_until, img) {
            if now < until {
                self.screenclip_until = None;
                self.finish_screenshot(&img, now);
            }
        }
    }

    fn finish_screenshot(&mut self, img: &Pixmap, now: Instant) {
        match clip::save_screenshot(img) {
            Some(path) => {
                self.hud_text = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.shelf.add(vec![path.to_string_lossy().into_owned()]);
            }
            None => self.hud_text = "Gagal menyimpan screenshot".into(),
        }
        self.hud = Some((Hud::Screenshot, now + Duration::from_millis(2600)));
        self.wake();
    }

    // ---- file drag & drop (called by the OLE drop target) ----

    pub fn file_drag_enter(&mut self, has_files: bool) -> bool {
        if !has_files || self.dragging_out || !self.cfg.features.shelf {
            return false;
        }
        self.file_drag = true;
        self.expanded = true;
        self.tab = Tab::Shelf;
        self.wake();
        true
    }

    pub fn file_drag_active(&self) -> bool {
        self.file_drag
    }

    pub fn file_drag_leave(&mut self) {
        self.file_drag = false;
    }

    pub fn file_drop(&mut self, files: Vec<String>) -> bool {
        self.file_drag = false;
        if !self.cfg.features.shelf || self.dragging_out {
            return false;
        }
        self.shelf.add(files);
        self.tab = Tab::Shelf;
        true
    }

    /// Switch to the fast frame rate right away (mouse entered).
    pub fn wake(&mut self) {
        if self.interval != 16 {
            self.set_interval(16);
        }
    }

    fn set_interval(&mut self, ms: u32) {
        self.interval = ms;
        unsafe {
            SetTimer(Some(self.hwnd), 1, ms, None);
        }
    }

    // ------------------------------------------------------------------
    // frame
    // ------------------------------------------------------------------

    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.05);
        self.last = now;
        self.snap = self.shared.lock().unwrap().clone();
        if !self.cfg.features.media {
            self.snap.media = None;
        }

        if now >= self.next_cfg_check {
            self.next_cfg_check = now + Duration::from_secs(1);
            let m = config::modified();
            if m != self.cfg_mtime {
                self.cfg_mtime = m;
                self.reload_config(now, true);
            }
        }
        if now >= self.next_fs_check {
            self.fullscreen = self.cfg.hide_in_fullscreen && system::fullscreen_active();
            self.next_fs_check = now + Duration::from_secs(1);
        }
        if now >= self.next_prune {
            self.next_prune = now + Duration::from_secs(5);
            self.shelf.prune();
        }
        if now >= self.next_topmost {
            self.next_topmost = now + Duration::from_secs(3);
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                );
            }
        }
        if !self.tabs().contains(&self.tab) {
            self.tab = Tab::Home;
        }

        self.update_hover(now);
        self.check_events(now);

        // Size animation toward the target layout.
        let target = self.target_layout(now);
        let (tw, th) = target.size();
        self.w.target = tw;
        self.h.target = th;
        self.w.step(dt, 300.0, 27.0);
        self.h.step(dt, 300.0, 27.0);

        // Content cross-fade: fade the old content out, swap, fade in once
        // the shape is close to its new size.
        if self.shown != target {
            self.fade -= dt * 12.0;
            if self.fade <= 0.0 {
                self.fade = 0.0;
                self.shown = target;
            }
        } else {
            let close = (self.w.v - tw).abs() < tw * 0.12 && (self.h.v - th).abs() < th * 0.22;
            if close || self.fade > 0.0 {
                self.fade = (self.fade + dt * 7.0).min(1.0);
            }
        }

        self.vis = approach(self.vis, if self.fullscreen { 0.0 } else { 1.0 }, 10.0, dt);

        self.render();
        self.present();

        let animating = !self.w.settled(0.05)
            || !self.h.settled(0.05)
            || self.shown != target
            || self.fade < 1.0
            || (self.vis > 0.0 && self.vis < 1.0);
        let playing = self.snap.media.as_ref().is_some_and(|m| m.playing);
        let iv = if animating || self.expanded || self.hud.is_some() || self.hover_since.is_some() {
            16
        } else if playing {
            40
        } else if self.activity() {
            100
        } else {
            150
        };
        if iv != self.interval {
            self.set_interval(iv);
        }
    }

    fn update_hover(&mut self, now: Instant) {
        if self.file_drag || self.moving.is_some() {
            self.expanded = true;
            self.leave_since = None;
            self.hot = None;
            return;
        }
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        let x = (p.x - self.win_x) as f32;
        let y = (p.y - self.win_y) as f32;
        let s = self.s;
        let cx = self.win_w as f32 / 2.0;
        let m = 6.0 * s;
        let half = self.w.v * s / 2.0;
        let inside = !self.fullscreen
            && x >= cx - half - m
            && x <= cx + half + m
            && y >= -2.0
            && y <= self.h.v * s + m;
        let dragging = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
        let delay = Duration::from_millis(self.cfg.hover_delay_ms.min(2000));

        if inside {
            self.leave_since = None;
            let since = *self.hover_since.get_or_insert(now);
            if !self.expanded && !dragging && now - since >= delay {
                self.expanded = true;
            }
        } else {
            self.hover_since = None;
            if self.expanded && !self.dragging_out {
                let since = *self.leave_since.get_or_insert(now);
                if now - since >= LEAVE_DELAY {
                    self.expanded = false;
                    self.leave_since = None;
                }
            }
        }
        self.hot = if inside { self.hit(x, y) } else { None };
    }

    fn check_events(&mut self, now: Instant) {
        let f = self.cfg.features.clone();
        let quiet = self.expanded;

        if self.snap.vol_seq != self.seen_vol {
            if self.seen_vol != 0 && !quiet && f.volume_hud {
                self.hud = Some((Hud::Volume, now + Duration::from_millis(1600)));
            }
            self.seen_vol = self.snap.vol_seq;
        }

        if self.snap.mic_seq != self.seen_mic {
            if self.snap.mic_muted.is_some() && !quiet {
                self.hud = Some((Hud::Mic, now + Duration::from_millis(1800)));
            }
            self.seen_mic = self.snap.mic_seq;
        }

        if self.snap.output_seq != self.seen_output {
            if !quiet {
                self.hud = Some((Hud::Output, now + Duration::from_millis(2200)));
            }
            self.seen_output = self.snap.output_seq;
        }

        let (tid, playing) = self
            .snap
            .media
            .as_ref()
            .map_or((0, false), |m| (m.track_id, m.playing));
        if tid != self.seen_track {
            if self.seen_track != 0 && tid != 0 && playing && !quiet {
                self.hud = Some((Hud::Track, now + Duration::from_millis(3200)));
            }
            self.seen_track = tid;
        }

        if self.snap.notif_seq != self.seen_notif {
            if self.seen_notif != 0 && f.notifications && !self.snap.notifs.is_empty() && !quiet {
                self.hud = Some((Hud::Notif, now + Duration::from_millis(4500)));
            }
            self.seen_notif = self.snap.notif_seq;
        }

        if let Some(b) = self.snap.battery {
            if self.last_ac.is_some_and(|ac| ac != b.ac) && !quiet {
                self.hud = Some((Hud::Power, now + Duration::from_millis(2600)));
            }
            self.last_ac = Some(b.ac);
        }

        if f.lock_keys {
            let (caps, num) = unsafe {
                (
                    GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0,
                    GetKeyState(VK_NUMLOCK.0 as i32) & 1 != 0,
                )
            };
            if let Some((pc, pn)) = self.last_locks {
                if pc != caps {
                    self.lock_hud = ("Caps Lock", caps);
                    self.hud = Some((Hud::Lock, now + Duration::from_millis(1400)));
                } else if pn != num {
                    self.lock_hud = ("Num Lock", num);
                    self.hud = Some((Hud::Lock, now + Duration::from_millis(1400)));
                }
            }
            self.last_locks = Some((caps, num));
        }

        if self.timer.as_ref().is_some_and(|t| now >= t.end) {
            let t = self.timer.take().unwrap();
            self.done_minutes = (t.total.as_secs() / 60) as u32;
            self.hud = Some((Hud::TimerDone, now + Duration::from_secs(7)));
            unsafe {
                let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(MB_ICONASTERISK);
            }
        }

        let beep = || unsafe {
            let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(MB_ICONASTERISK);
        };

        // Quick reminders.
        if self.reminders.first().is_some_and(|r| now >= r.end) {
            let r = self.reminders.remove(0);
            self.done_minutes = (r.total.as_secs() / 60) as u32;
            self.hud = Some((Hud::Reminder, now + Duration::from_secs(7)));
            beep();
        }

        // Alarm (one-shot at hh:mm).
        let clk = system::local_time();
        if self.alarm.armed && clk.wHour == self.alarm.hour && clk.wMinute == self.alarm.minute {
            self.alarm.armed = false;
            self.hud = Some((Hud::Alarm, now + Duration::from_secs(12)));
            beep();
        }

        // Full-screen capture once the notch has faded out.
        if self.pending_shot.is_some_and(|t| now >= t) {
            self.pending_shot = None;
            match clip::capture_screen() {
                Some(img) => self.finish_screenshot(&img, now),
                None => {
                    self.hud_text = "Gagal mengambil screenshot".into();
                    self.hud = Some((Hud::Screenshot, now + Duration::from_millis(2600)));
                }
            }
        }
        if self.screenclip_until.is_some_and(|t| now >= t) {
            self.screenclip_until = None;
        }

        // Bluetooth device connected (reports a battery).
        let names: Vec<String> = self.snap.bluetooth.iter().map(|b| b.name.clone()).collect();
        if let Some(b) = self.snap.bluetooth.iter().find(|b| !self.seen_bt.contains(&b.name)) {
            if !quiet && f.bluetooth {
                self.hud_text = format!("{} · {}%", b.name, b.battery);
                self.hud = Some((Hud::Bluetooth, now + Duration::from_millis(3000)));
            }
        }
        self.seen_bt = names;

        if self.snap.update.is_some() && !self.seen_update {
            self.seen_update = true;
            if let Some(u) = &self.snap.update {
                self.hud_text = format!("Isla {} tersedia", u.tag);
            }
            self.hud = Some((Hud::Update, now + Duration::from_secs(6)));
        }

        let failed = match &*self.install.lock().unwrap() {
            Some(crate::update::InstallState::Failed(e)) => Some(e.clone()),
            _ => None,
        };
        if let Some(e) = failed {
            *self.install.lock().unwrap() = None;
            self.hud_text = format!("Update gagal: {e}");
            self.hud = Some((Hud::Update, now + Duration::from_secs(6)));
        }

        if self.hud.is_some_and(|(_, until)| now >= until) {
            self.hud = None;
        }
    }

    /// The lyric line for the current playback position, if any.
    fn lyric_line(&self, now: Instant) -> String {
        if !self.cfg.features.lyrics {
            return String::new();
        }
        match (&self.snap.media, &self.snap.lyrics) {
            (Some(m), Some(l)) if m.playing && l.track_id == m.track_id => {
                l.line_at(m.position_now(now)).to_string()
            }
            _ => String::new(),
        }
    }

    fn activity(&self) -> bool {
        self.timer.is_some() || !self.reminders.is_empty() || self.stopwatch.running()
    }

    fn target_layout(&self, now: Instant) -> Layout {
        if self.expanded {
            return Layout::Expanded;
        }
        if let Some((h, until)) = self.hud {
            if now < until {
                return Layout::Hud(h);
            }
        }
        if self.snap.media.as_ref().is_some_and(|m| m.playing) {
            // Stay wide for the whole song once lyrics exist, so the notch
            // does not pulse between verses.
            let has_lyrics = self.cfg.features.lyrics
                && self
                    .snap
                    .lyrics
                    .as_ref()
                    .zip(self.snap.media.as_ref())
                    .is_some_and(|(l, m)| l.track_id == m.track_id && !l.lines.is_empty());
            if has_lyrics {
                Layout::MusicLyric
            } else {
                Layout::Music
            }
        } else if self.activity() {
            Layout::Timer
        } else {
            Layout::Idle
        }
    }

    // ------------------------------------------------------------------
    // rendering
    // ------------------------------------------------------------------

    fn render(&mut self) {
        let now = Instant::now();
        let s = self.s;
        self.pixmap.fill(Color::TRANSPARENT);
        self.buttons.clear();
        let vis = self.vis;
        // Invisible while fullscreen, and while a screenshot is pending.
        if vis <= 0.004 || self.pending_shot.is_some() {
            return;
        }
        let cx = self.win_w as f32 / 2.0;
        let (w, h) = (self.w.v.max(60.0), self.h.v.max(EAR + 8.0));
        let wp = w * s;
        let hp = h * s;
        let r = (h * 0.5).min(32.0) * s;
        let ear = EAR * s;

        // Soft drop shadow, growing with the expansion.
        let ex = ((h - 32.0) / (MAX_H - 32.0)).clamp(0.0, 1.0);
        if ex > 0.01 {
            for i in 1..=8 {
                let k = i as f32 * 2.6 * s;
                let a = 0.05 * ex * vis * (1.0 - i as f32 / 9.0);
                if let Some(p) = rrect_path(cx - wp / 2.0 - k * 0.7, -k, wp + k * 1.4, hp + k * 1.5, r + k) {
                    self.pixmap.fill_path(&p, &paint(Rgba(0.0, 0.0, 0.0, a)), FillRule::Winding, Transform::identity(), None);
                }
            }
        }

        let Some(notch) = notch_path(cx, wp, hp, r, ear) else { return };
        self.pixmap.fill_path(&notch, &paint(Rgba(0.0, 0.0, 0.0, vis)), FillRule::Winding, Transform::identity(), None);

        // Privacy dots floating beside the notch, iPhone style.
        if self.cfg.features.privacy {
            let mut dx = cx + wp / 2.0 + ear + 7.0 * s;
            for (active, col) in [
                (!self.snap.mic_apps.is_empty(), ORANGE),
                (!self.snap.cam_apps.is_empty(), GREEN),
            ] {
                if active {
                    if let Some(p) = tiny_skia::PathBuilder::from_circle(dx, 9.0 * s, 4.0 * s) {
                        self.pixmap.fill_path(&p, &paint(col.a(vis)), FillRule::Winding, Transform::identity(), None);
                    }
                    dx += 11.0 * s;
                }
            }
        }

        let alpha = self.fade * vis;
        if alpha <= 0.01 {
            return;
        }
        let Some(mut mask) = Mask::new(self.win_w as u32, self.win_h as u32) else { return };
        mask.fill_path(&notch, FillRule::Winding, true, Transform::identity());

        let layout = self.shown;
        let (tw, th) = layout.size();
        let accent = config::parse_hex(&self.cfg.accent)
            .map(|c| Rgba::rgb(c[0], c[1], c[2]))
            .unwrap_or(BLUE);
        let tabs = self.tabs();
        let lyric = self.lyric_line(now);
        let installing = matches!(*self.install.lock().unwrap(), Some(crate::update::InstallState::Downloading));
        let ctx = Ctx {
            installing,
            stopwatch: &self.stopwatch,
            reminders: &self.reminders,
            alarm: &self.alarm,
            clip: &self.clip,
            hud_text: &self.hud_text,
            lyric: &lyric,
            snap: &self.snap,
            cfg: &self.cfg,
            timer: self.timer.as_ref(),
            timer_minutes: self.timer_minutes,
            done_minutes: self.done_minutes,
            hot: self.hot,
            now,
            t: now.duration_since(self.start).as_secs_f32(),
            clock: system::local_time(),
            default_accent: accent,
            tab: self.tab,
            tabs: &tabs,
            shelf: &self.shelf,
            shortcut_icons: &self.shortcut_icons,
            file_drag: self.file_drag,
            lock: self.lock_hud,
            config_msg: &self.config_msg,
        };
        let mut cv = Canvas {
            pm: &mut self.pixmap,
            mask: Some(&mask),
            fonts: &mut self.fonts,
            ox: cx - tw * s / 2.0,
            oy: 0.0,
            s,
            alpha,
        };
        views::draw(&mut cv, &ctx, &mut self.buttons, layout, tw, th);
    }

    /// Dev aid (`--snapshot <dir>`): renders every view to PNG files
    /// without touching the mouse.
    pub fn snapshot(&mut self, dir: &str) {
        let _ = std::fs::create_dir_all(dir);
        self.snap = self.shared.lock().unwrap().clone();
        self.vis = 1.0;
        self.fade = 1.0;
        self.lock_hud = ("Caps Lock", true);
        self.config_msg = "Pengaturan diperbarui".into();
        let shoot = |app: &mut App, layout: Layout, name: &str| {
            let (w, h) = layout.size();
            app.w = Spring::new(w);
            app.h = Spring::new(h);
            app.shown = layout;
            app.expanded = layout == Layout::Expanded;
            app.render();
            let _ = app.pixmap.save_png(format!("{dir}\\{name}.png"));
        };
        // Demo state for the tools / clip views (no real clipboard content).
        let now = Instant::now();
        self.stopwatch = Stopwatch { since: Some(now - Duration::from_millis(83_400)), acc: Duration::ZERO };
        self.reminders = vec![FocusTimer { end: now + Duration::from_secs(724), total: Duration::from_secs(900) }];
        self.alarm = Alarm { hour: 6, minute: 30, armed: true };
        for (i, t) in ["Contoh teks yang di-copy", "https://github.com/", "Riwayat clipboard tidak disimpan ke disk"]
            .iter()
            .enumerate()
        {
            self.clip.items.push(clip::Item { data: clip::Data::Text(t.to_string()), time: format!("21:0{i}") });
        }
        if let Some(m) = &self.snap.media {
            self.snap.lyrics = Some(Arc::new(crate::lyrics::Lyrics {
                track_id: m.track_id,
                lines: vec![(0.0, "Contoh baris lirik yang sedang dinyanyikan".into())],
            }));
            if let Some(mm) = self.snap.media.as_mut() {
                mm.playing = true;
            }
        }
        self.hud_text = "Isla_2026-11-23_21-40-12.png".into();
        for (tab, name) in [
            (Tab::Home, "tab_home"),
            (Tab::System, "tab_system"),
            (Tab::Tools, "tab_tools"),
            (Tab::Clip, "tab_clip"),
            (Tab::Shelf, "tab_shelf"),
            (Tab::Notifs, "tab_notif"),
        ] {
            self.tab = tab;
            shoot(self, Layout::Expanded, name);
        }
        for (hud, name) in [
            (Hud::Volume, "hud_volume"),
            (Hud::Notif, "hud_notif"),
            (Hud::Lock, "hud_lock"),
            (Hud::Mic, "hud_mic"),
            (Hud::Output, "hud_output"),
            (Hud::Config, "hud_config"),
            (Hud::Screenshot, "hud_screenshot"),
            (Hud::Reminder, "hud_reminder"),
            (Hud::Alarm, "hud_alarm"),
        ] {
            shoot(self, Layout::Hud(hud), name);
        }
        shoot(self, Layout::Idle, "idle");
        shoot(self, Layout::Music, "music");
        shoot(self, Layout::MusicLyric, "music_lyric");
        shoot(self, Layout::Timer, "timer");
        if let Some(mut st) = Settings::open(&self.cfg) {
            for i in 0..6 {
                st.set_section(i);
                st.render(&mut self.fonts, &self.shortcut_icons);
                st.save_png(&format!("{dir}\\settings_{i}.png"));
            }
            unsafe {
                let _ = DestroyWindow(st.hwnd);
            }
        }
    }

    fn present(&mut self) {
        let Some(sf) = &self.surface else { return };
        unsafe {
            let _ = GdiFlush();
            let dst = std::slice::from_raw_parts_mut(sf.bits, sf.len);
            for (d, p) in dst.chunks_exact_mut(4).zip(self.pixmap.data().chunks_exact(4)) {
                d[0] = p[2];
                d[1] = p[1];
                d[2] = p[0];
                d[3] = p[3];
            }
            let pos = POINT { x: self.win_x, y: self.win_y };
            let size = SIZE { cx: self.win_w, cy: self.win_h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pos),
                Some(&size),
                Some(sf.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

/// Monitor by index: 0 = primary, then the others in system order.
fn pick_monitor(index: usize) -> HMONITOR {
    unsafe extern "system" fn collect(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> windows::core::BOOL {
        let v = &mut *(data.0 as *mut Vec<HMONITOR>);
        v.push(m);
        windows::core::BOOL(1)
    }
    unsafe {
        let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if index == 0 {
            return primary;
        }
        let mut all: Vec<HMONITOR> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut all as *mut _ as isize));
        all.retain(|m| *m != primary);
        all.get(index - 1).copied().unwrap_or(primary)
    }
}
