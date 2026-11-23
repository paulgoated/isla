//! Layout drawing (design units, origin = top-left of the target notch).

use crate::config::Config;
use crate::draw::*;
use crate::sensors::fmt_rate;
use crate::shelf::Shelf;
use crate::system::{self, SYSTEMTIME};
use crate::text::Weight;
use crate::clip;
use crate::ui::{Action, Alarm, Buttons, FocusTimer, Hud, Layout, Stopwatch, Tab};
use crate::weather;
use crate::worker::{Media, Shared};
use std::time::Instant;
use tiny_skia::Pixmap;

pub struct Ctx<'a> {
    /// One-click update download in progress.
    pub installing: bool,
    pub stopwatch: &'a Stopwatch,
    pub reminders: &'a [FocusTimer],
    pub alarm: &'a Alarm,
    pub clip: &'a clip::History,
    pub hud_text: &'a str,
    pub lyric: &'a str,
    pub snap: &'a Shared,
    pub cfg: &'a Config,
    pub timer: Option<&'a FocusTimer>,
    pub timer_minutes: u32,
    pub done_minutes: u32,
    pub hot: Option<Action>,
    pub now: Instant,
    pub t: f32,
    pub clock: SYSTEMTIME,
    pub default_accent: Rgba,
    pub tab: Tab,
    pub tabs: &'a [Tab],
    pub shelf: &'a Shelf,
    pub shortcut_icons: &'a [Option<Pixmap>],
    pub file_drag: bool,
    pub lock: (&'static str, bool),
    pub config_msg: &'a str,
}

impl Ctx<'_> {
    fn accent(&self) -> Rgba {
        self.snap
            .media
            .as_ref()
            .and_then(|m| m.accent)
            .map(|c| Rgba::rgb(c[0], c[1], c[2]))
            .unwrap_or(self.default_accent)
    }
    fn hhmm(&self) -> String {
        format!("{:02}:{:02}", self.clock.wHour, self.clock.wMinute)
    }
    fn is_hot(&self, a: Action) -> bool {
        self.hot == Some(a)
    }
}

pub fn draw(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, layout: Layout, w: f32, h: f32) {
    match layout {
        Layout::Idle => idle(cv, c, w, h),
        Layout::Music => music(cv, c, w, h),
        Layout::MusicLyric => music_lyric(cv, c, w, h),
        Layout::Timer => timer(cv, c, w, h),
        Layout::Hud(k) => hud(cv, c, k, w, h),
        Layout::Expanded => {
            header(cv, c, btn, w);
            match c.tab {
                Tab::Home => home(cv, c, btn, w),
                Tab::System => system_tab(cv, c, btn, w),
                Tab::Tools => tools_tab(cv, c, btn, w),
                Tab::Clip => clip_tab(cv, c, btn, w),
                Tab::Shelf => shelf_tab(cv, c, btn, w),
                Tab::Notifs => notif_tab(cv, c, btn, w),
            }
        }
    }
}

// ----------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------

fn mmss(d: f64) -> String {
    let t = d.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

fn timer_text(t: &FocusTimer, now: Instant) -> String {
    // Round up so the display reaches 0:00 exactly when it fires.
    mmss(t.remaining(now).as_secs_f64().ceil())
}

fn timer_progress(t: &FocusTimer, now: Instant) -> f32 {
    t.remaining(now).as_secs_f32() / t.total.as_secs_f32().max(1.0)
}

fn battery_widget(cv: &mut Canvas, right: f32, cy: f32, size: f32, b: system::Battery) -> f32 {
    let col = if b.charging || b.ac { GREEN } else { WHITE };
    let pct = format!("{}%", b.percent);
    let bw = 22.0;
    cv.battery(right - bw - 2.0, cy, bw, 11.0, b.percent as f32 / 100.0, b.charging);
    let tw = cv.text_right(right - bw - 8.0, cy + size * 0.36, size, Weight::Semibold, col, &pct);
    let mut used = bw + 8.0 + tw;
    if b.charging {
        cv.bolt(right - used - 7.0, cy, 12.0, GREEN);
        used += 14.0;
    }
    used
}

fn icon_or_letter(cv: &mut Canvas, img: Option<&Pixmap>, label: &str, x: f32, y: f32, size: f32, r: f32, col: Rgba) {
    match img {
        Some(i) => cv.image(i, x, y, size, r),
        None => {
            cv.rrect(x, y, size, size, r, col.a(0.3));
            let l: String = label.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            cv.text_center(x + size / 2.0, y + size * 0.68, size * 0.5, Weight::Bold, WHITE, &l);
        }
    }
}

fn art_or_note(cv: &mut Canvas, m: &Media, x: f32, y: f32, size: f32, r: f32, accent: Rgba) {
    match &m.art {
        Some(img) => cv.image(img, x, y, size, r),
        None => {
            cv.rrect(x, y, size, size, r, accent.a(0.25));
            let sz = size * 0.42;
            let cx = x + size / 2.0;
            let cy = y + size / 2.0;
            cv.circle(cx - sz * 0.2, cy + sz * 0.38, sz * 0.24, accent);
            cv.rrect(cx, cy - sz * 0.5, sz * 0.12, sz * 0.9, 0.0, accent);
            cv.rrect(cx, cy - sz * 0.5, sz * 0.42, sz * 0.14, sz * 0.05, accent);
        }
    }
}

/// Log-scale fill for byte rates: 1 KB/s ~ 0, 100 MB/s = 1.
fn rate_progress(bps: f64) -> f32 {
    ((bps / 1000.0 + 1.0).log10() / 5.0).clamp(0.0, 1.0) as f32
}

#[allow(clippy::too_many_arguments)]
fn chip(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, label: &str, value: &str, hot: bool) {
    cv.rrect(x, y, w, h, 12.0, WHITE.a(if hot { 0.14 } else { 0.07 }));
    let l = cv.fit(9.5, Weight::Semibold, label, w - 46.0);
    cv.text(x + 11.0, y + 17.0, 9.5, Weight::Semibold, GRAY, &l);
    let v = cv.fit(13.5, Weight::Semibold, value, w - 44.0);
    cv.text(x + 11.0, y + 34.0, 13.5, Weight::Semibold, WHITE, &v);
}

fn chip_ring(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, prog: f32, col: Rgba) {
    cv.ring(x + w - 20.0, y + h / 2.0, 8.5, 3.0, prog, col);
}

// ----------------------------------------------------------------------
// collapsed
// ----------------------------------------------------------------------

fn idle(cv: &mut Canvas, c: &Ctx, w: f32, h: f32) {
    let base = h / 2.0 + 4.7;
    cv.text(16.0, base, 13.0, Weight::Semibold, WHITE, &c.hhmm());
    if let Some(b) = c.snap.battery {
        battery_widget(cv, w - 14.0, h / 2.0, 12.0, b);
    } else if let Some(wx) = &c.snap.weather {
        let t = format!("{:.0}°", wx.temp);
        let tw = cv.text_right(w - 16.0, base, 12.5, Weight::Semibold, WHITE, &t);
        cv.weather(w - 16.0 - tw - 11.0, h / 2.0, 16.0, wx.code, wx.is_day);
    } else {
        let d = format!(
            "{} {}",
            system::DAYS_SHORT[c.clock.wDayOfWeek as usize % 7],
            c.clock.wDay
        );
        cv.text_right(w - 16.0, base, 12.5, Weight::Regular, GRAY, &d);
    }
}

fn music(cv: &mut Canvas, c: &Ctx, w: f32, h: f32) {
    let Some(m) = &c.snap.media else { return idle(cv, c, w, h) };
    let acc = c.accent();
    art_or_note(cv, m, 9.0, 6.0, 20.0, 6.0, acc);
    match c.timer {
        Some(t) => {
            cv.text_right(w - 14.0, h / 2.0 + 4.7, 13.0, Weight::Semibold, ORANGE, &timer_text(t, c.now));
        }
        None => cv.equalizer(w - 32.0, 9.0, 18.0, 14.0, c.t, m.playing, acc),
    }
}

/// Collapsed activity: focus timer, else the next reminder, else the stopwatch.
fn timer(cv: &mut Canvas, c: &Ctx, w: f32, h: f32) {
    let (cy, base) = (h / 2.0, h / 2.0 + 4.7);
    if let Some(t) = c.timer.or(c.reminders.first()) {
        let (label, col) = if c.timer.is_some() { ("Fokus", ORANGE) } else { ("Pengingat", CYAN) };
        cv.ring(19.0, cy, 7.0, 2.4, timer_progress(t, c.now), col);
        cv.text(34.0, cy + 4.5, 12.0, Weight::Semibold, GRAY, label);
        cv.text_right(w - 14.0, base, 13.0, Weight::Semibold, col, &timer_text(t, c.now));
    } else if c.stopwatch.running() {
        let pulse = 0.5 + 0.5 * (c.t * 3.0).sin().abs();
        cv.circle(19.0, cy, 4.0, GREEN.a(pulse));
        cv.text(34.0, cy + 4.5, 12.0, Weight::Semibold, GRAY, "Stopwatch");
        cv.text_right(w - 14.0, base, 13.0, Weight::Semibold, GREEN, &stopwatch_text(c.stopwatch.elapsed(c.now)));
    } else {
        idle(cv, c, w, h)
    }
}

fn stopwatch_text(d: std::time::Duration) -> String {
    let t = d.as_secs_f64();
    let m = (t / 60.0) as u64;
    if m >= 60 {
        format!("{}:{:02}:{:02}", m / 60, m % 60, t as u64 % 60)
    } else {
        format!("{:02}:{:04.1}", m, t % 60.0)
    }
}

/// Collapsed music with the current lyric line between art and equalizer.
fn music_lyric(cv: &mut Canvas, c: &Ctx, w: f32, h: f32) {
    let Some(m) = &c.snap.media else { return idle(cv, c, w, h) };
    let acc = c.accent();
    art_or_note(cv, m, 9.0, 6.0, 20.0, 6.0, acc);
    cv.equalizer(w - 32.0, 9.0, 18.0, 14.0, c.t, m.playing, acc);
    let (x0, x1) = (40.0, w - 42.0);
    let line = if c.lyric.is_empty() { "♪".to_string() } else { cv.fit(12.5, Weight::Semibold, c.lyric, x1 - x0) };
    cv.text_center((x0 + x1) / 2.0, h / 2.0 + 4.5, 12.5, Weight::Semibold, WHITE.a(0.95), &line);
}

fn hud(cv: &mut Canvas, c: &Ctx, kind: Hud, w: f32, h: f32) {
    let cy = h / 2.0;
    let base = cy + 4.7;
    match kind {
        Hud::Volume => {
            let (v, muted) = (c.snap.volume, c.snap.muted);
            let col = if muted { GRAY } else { WHITE };
            cv.speaker(16.0, cy, 20.0, if muted { 0.0 } else { v }, muted, col);
            let (x0, x1) = (52.0, w - 62.0);
            cv.rrect(x0, cy - 3.0, x1 - x0, 6.0, 3.0, WHITE.a(0.18));
            let fw = (x1 - x0) * if muted { 0.0 } else { v };
            if fw > 0.5 {
                cv.rrect(x0, cy - 3.0, fw.max(6.0), 6.0, 3.0, col);
            }
            let label = if muted { "Mute".to_string() } else { format!("{}%", (v * 100.0).round()) };
            cv.text_right(w - 16.0, base, 13.0, Weight::Semibold, col, &label);
        }
        Hud::Power => {
            let Some(b) = c.snap.battery else { return };
            if b.ac {
                cv.bolt(24.0, cy, 18.0, GREEN);
                cv.text(40.0, base, 13.0, Weight::Semibold, WHITE, "Mengisi daya");
            } else {
                cv.text(18.0, base, 13.0, Weight::Semibold, WHITE, "Pakai baterai");
            }
            battery_widget(cv, w - 16.0, cy, 13.0, b);
        }
        Hud::Track => {
            let Some(m) = &c.snap.media else { return };
            let acc = c.accent();
            art_or_note(cv, m, 12.0, 12.0, 48.0, 12.0, acc);
            cv.marquee(72.0, w - 52.0, 33.0, 14.0, Weight::Semibold, WHITE, &m.title, c.t);
            let artist = cv.fit(12.0, Weight::Regular, &m.artist, w - 52.0 - 72.0);
            cv.text(72.0, 52.0, 12.0, Weight::Regular, GRAY, &artist);
            cv.equalizer(w - 38.0, 26.0, 22.0, 20.0, c.t, m.playing, acc);
        }
        Hud::TimerDone => {
            let pulse = 0.55 + 0.45 * (c.t * 6.0).sin().abs();
            cv.ring(24.0, cy, 8.0, 2.6, 1.0, ORANGE.a(pulse));
            cv.text(42.0, base, 13.0, Weight::Semibold, ORANGE, "Waktu fokus selesai!");
            cv.text_right(w - 16.0, cy + 4.5, 12.0, Weight::Regular, GRAY, &format!("{} mnt", c.done_minutes));
        }
        Hud::Lock => {
            let (name, on) = c.lock;
            let col = if on { c.default_accent } else { GRAY };
            cv.stroke(rrect_path(14.0, cy - 10.0, 20.0, 20.0, 6.0), 1.6, col);
            cv.text_center(24.0, cy + 4.5, 12.0, Weight::Bold, col, if name.starts_with('C') { "A" } else { "1" });
            cv.text(44.0, base, 13.0, Weight::Semibold, WHITE, name);
            cv.text_right(w - 16.0, base, 13.0, Weight::Bold, col, if on { "ON" } else { "OFF" });
        }
        Hud::Mic => {
            let muted = c.snap.mic_muted.unwrap_or(false);
            cv.mic(24.0, cy, 20.0, muted, if muted { GRAY } else { WHITE });
            cv.text(44.0, base, 13.0, Weight::Semibold, WHITE, "Mikrofon");
            let (t, col) = if muted { ("Mute", RED) } else { ("Aktif", GREEN) };
            cv.text_right(w - 16.0, base, 13.0, Weight::Semibold, col, t);
        }
        Hud::Output => {
            cv.speaker(16.0, cy, 20.0, 0.7, false, WHITE);
            cv.text(52.0, cy - 2.0, 10.0, Weight::Semibold, GRAY, "OUTPUT AUDIO");
            let name = cv.fit(13.0, Weight::Semibold, &c.snap.output, w - 52.0 - 16.0);
            cv.text(52.0, cy + 13.0, 13.0, Weight::Semibold, WHITE, &name);
        }
        Hud::Notif => {
            let Some(n) = c.snap.notifs.first() else { return };
            icon_or_letter(cv, n.icon.as_deref(), &n.app, 14.0, 16.0, 44.0, 12.0, c.default_accent);
            let x = 70.0;
            let app = cv.fit(11.0, Weight::Semibold, &n.app, w - x - 60.0);
            cv.text(x, 28.0, 11.0, Weight::Semibold, GRAY, &app);
            cv.text_right(w - 16.0, 28.0, 10.5, Weight::Regular, GRAY, &n.time);
            let title = cv.fit(13.5, Weight::Semibold, &n.title, w - x - 16.0);
            cv.text(x, 46.0, 13.5, Weight::Semibold, WHITE, &title);
            let body = cv.fit(12.0, Weight::Regular, &n.body, w - x - 16.0);
            cv.text(x, 63.0, 12.0, Weight::Regular, GRAY, &body);
        }
        Hud::Config => {
            let ok = !c.config_msg.starts_with("Pengaturan error");
            let col = if ok { GREEN } else { RED };
            cv.circle(22.0, cy, 5.0, col);
            let msg = cv.fit(12.5, Weight::Semibold, c.config_msg, w - 36.0 - 16.0);
            cv.text(36.0, cy + 4.5, 12.5, Weight::Semibold, WHITE, &msg);
        }
        Hud::Reminder | Hud::Alarm => {
            let pulse = 0.55 + 0.45 * (c.t * 6.0).sin().abs();
            let col = if kind == Hud::Alarm { ORANGE } else { CYAN };
            bell(cv, 24.0, cy, 18.0, col.a(pulse));
            let (title, right) = if kind == Hud::Alarm {
                ("Alarm!".to_string(), format!("{:02}:{:02}", c.alarm.hour, c.alarm.minute))
            } else {
                ("Pengingat selesai".to_string(), format!("{} mnt", c.done_minutes))
            };
            cv.text(42.0, base, 13.0, Weight::Semibold, col, &title);
            cv.text_right(w - 16.0, cy + 4.5, 12.0, Weight::Regular, GRAY, &right);
        }
        Hud::Bluetooth => {
            headphones(cv, 24.0, cy, 18.0, CYAN);
            let t = cv.fit(13.0, Weight::Semibold, c.hud_text, w - 44.0 - 16.0);
            cv.text(44.0, base, 13.0, Weight::Semibold, WHITE, &t);
        }
        Hud::Update => {
            cv.circle(22.0, cy, 5.0, c.default_accent);
            let t = cv.fit(13.0, Weight::Semibold, c.hud_text, w - 36.0 - 110.0);
            cv.text(36.0, base, 13.0, Weight::Semibold, WHITE, &t);
            if c.snap.update.is_some() && !c.installing && !c.hud_text.starts_with("Update gagal") {
                cv.text_right(w - 16.0, cy + 4.5, 11.5, Weight::Regular, GRAY, "buka notch → Update");
            }
        }
        Hud::Screenshot => {
            camera_shutter(cv, 24.0, cy, 18.0, WHITE);
            cv.text(44.0, cy - 2.0, 10.0, Weight::Semibold, GRAY, "SCREENSHOT DISIMPAN · ADA DI SHELF");
            let t = cv.fit(12.5, Weight::Semibold, c.hud_text, w - 44.0 - 16.0);
            cv.text(44.0, cy + 13.0, 12.5, Weight::Semibold, WHITE, &t);
        }
    }
}

fn bell(cv: &mut Canvas, cx: f32, cy: f32, sz: f32, c: Rgba) {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(cx - sz * 0.38, cy + sz * 0.22);
    pb.cubic_to(cx - sz * 0.3, cy + sz * 0.05, cx - sz * 0.32, cy - sz * 0.42, cx, cy - sz * 0.42);
    pb.cubic_to(cx + sz * 0.32, cy - sz * 0.42, cx + sz * 0.3, cy + sz * 0.05, cx + sz * 0.38, cy + sz * 0.22);
    pb.close();
    cv.fill(pb.finish(), c);
    cv.circle(cx, cy + sz * 0.34, sz * 0.09, c);
}

fn headphones(cv: &mut Canvas, cx: f32, cy: f32, sz: f32, c: Rgba) {
    let mut pb = tiny_skia::PathBuilder::new();
    let r = sz * 0.38;
    for i in 0..=24 {
        let a = std::f32::consts::PI + std::f32::consts::PI * i as f32 / 24.0;
        let (x, y) = (cx + r * a.cos(), cy + sz * 0.08 + r * a.sin());
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    cv.stroke(pb.finish(), sz * 0.1, c);
    cv.rrect(cx - r - sz * 0.08, cy + sz * 0.02, sz * 0.2, sz * 0.36, sz * 0.06, c);
    cv.rrect(cx + r - sz * 0.12, cy + sz * 0.02, sz * 0.2, sz * 0.36, sz * 0.06, c);
}

fn camera_shutter(cv: &mut Canvas, cx: f32, cy: f32, sz: f32, c: Rgba) {
    cv.rrect(cx - sz * 0.5, cy - sz * 0.3, sz, sz * 0.66, sz * 0.14, c);
    cv.rrect(cx - sz * 0.18, cy - sz * 0.42, sz * 0.36, sz * 0.16, sz * 0.05, c);
    cv.circle(cx, cy + sz * 0.03, sz * 0.2, Rgba(0.0, 0.0, 0.0, 1.0));
    cv.circle(cx, cy + sz * 0.03, sz * 0.12, c);
}

// ----------------------------------------------------------------------
// expanded
// ----------------------------------------------------------------------

const PAD: f32 = 22.0;

fn tab_label(t: Tab) -> &'static str {
    match t {
        Tab::Home => "Beranda",
        Tab::System => "Sistem",
        Tab::Tools => "Alat",
        Tab::Clip => "Clip",
        Tab::Shelf => "Shelf",
        Tab::Notifs => "Notif",
    }
}

fn header(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    let base = 25.5;
    // Left: weather (or the date when there is none).
    match &c.snap.weather {
        Some(wx) => {
            cv.weather(PAD + 8.0, 21.0, 17.0, wx.code, wx.is_day);
            cv.text(PAD + 19.0, base, 12.0, Weight::Semibold, WHITE, &format!("{:.0}°", wx.temp));
        }
        None => {
            let date = format!(
                "{} {}",
                system::DAYS_SHORT[c.clock.wDayOfWeek as usize % 7],
                c.clock.wDay
            );
            cv.text(PAD, base, 12.0, Weight::Regular, GRAY, &date);
        }
    }

    // Right: time, battery, Bluetooth battery, privacy indicators.
    let mut r = w - PAD;
    r -= cv.text_right(r, base, 12.5, Weight::Semibold, WHITE, &c.hhmm()) + 10.0;
    if let Some(b) = c.snap.battery {
        r -= battery_widget(cv, r, 21.0, 11.5, b) + 8.0;
    }
    if c.cfg.features.bluetooth {
        if let Some(bt) = c.snap.bluetooth.first() {
            let col = if bt.battery <= 20 { RED } else { CYAN };
            let tw = cv.text_right(r, base, 11.5, Weight::Semibold, col, &format!("{}%", bt.battery));
            headphones(cv, r - tw - 9.0, 20.0, 13.0, col);
            r -= tw + 26.0;
        }
    }
    if c.snap.update.is_some() {
        let hot = c.is_hot(Action::OpenUpdate);
        let label = if c.installing { "Mengunduh…" } else { "Update" };
        let lw = cv.measure(10.5, Weight::Semibold, label) + 16.0;
        cv.rrect(r - lw, 12.0, lw, 18.0, 9.0, c.default_accent.a(if hot { 1.0 } else { 0.8 }));
        cv.text_center(r - lw / 2.0, 24.5, 10.5, Weight::Semibold, WHITE, label);
        btn.push((cv.phys(r - lw, 8.0, lw, 26.0), Action::OpenUpdate));
        r -= lw + 8.0;
    }
    if c.cfg.features.privacy {
        if !c.snap.cam_apps.is_empty() {
            cv.camera(r - 8.0, 21.0, 15.0, GREEN);
            r -= 20.0;
        }
        if !c.snap.mic_apps.is_empty() {
            cv.mic(r - 6.0, 21.0, 14.0, false, ORANGE);
        }
    }

    // Center: tab pills.
    if c.tabs.len() > 1 {
        let labels: Vec<(Tab, String)> = c
            .tabs
            .iter()
            .map(|t| {
                let n = c.snap.notifs.len();
                let l = if *t == Tab::Notifs && n > 0 {
                    format!("{} {}", tab_label(*t), n)
                } else {
                    tab_label(*t).to_string()
                };
                (*t, l)
            })
            .collect();
        let widths: Vec<f32> = labels.iter().map(|(_, l)| cv.measure(11.0, Weight::Semibold, l) + 18.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + 4.0 * (widths.len() as f32 - 1.0);
        let mut tx = w / 2.0 - total / 2.0;
        for ((t, l), tw) in labels.iter().zip(widths) {
            let active = *t == c.tab;
            let bg = if active {
                0.16
            } else if c.is_hot(Action::Tab(*t)) {
                0.08
            } else {
                0.0
            };
            cv.rrect(tx, 10.0, tw, 22.0, 11.0, WHITE.a(bg));
            cv.text_center(tx + tw / 2.0, 25.0, 11.0, Weight::Semibold, if active { WHITE } else { GRAY }, l);
            btn.push((cv.phys(tx, 6.0, tw, 30.0), Action::Tab(*t)));
            tx += tw + 4.0;
        }
    }
}

// ---------------- Beranda ----------------

fn home(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    match &c.snap.media {
        Some(m) => media_block(cv, c, btn, m, w),
        None => clock_block(cv, c, w),
    }

    // Bottom row: shortcuts + focus chip.
    let y = 158.0;
    let fw = 132.0;
    let fx = w - PAD - fw;
    let hot = c.is_hot(Action::Focus);
    let (val, prog) = match c.timer {
        Some(t) => (timer_text(t, c.now), timer_progress(t, c.now)),
        None => (format!("{} mnt", c.timer_minutes), 0.0),
    };
    let label = if c.timer.is_some() { "FOKUS · klik stop" } else { "FOKUS · klik mulai" };
    chip(cv, fx, y, fw, 44.0, label, &val, hot);
    chip_ring(cv, fx, y, fw, 44.0, prog, ORANGE);
    btn.push((cv.phys(fx, y, fw, 44.0), Action::Focus));

    if c.cfg.features.shortcuts {
        if c.cfg.shortcuts.is_empty() {
            cv.text(PAD, y + 27.0, 11.5, Weight::Regular, GRAY, "Tambah shortcut di pengaturan (klik kanan)");
        }
        for (i, sc) in c.cfg.shortcuts.iter().take(6).enumerate() {
            let x = PAD + i as f32 * 52.0;
            let hot = c.is_hot(Action::Shortcut(i));
            cv.rrect(x, y, 44.0, 44.0, 12.0, WHITE.a(if hot { 0.16 } else { 0.07 }));
            let icon = c.shortcut_icons.get(i).and_then(|o| o.as_ref());
            let sz = if hot { 30.0 } else { 28.0 };
            icon_or_letter(cv, icon, &sc.name, x + 22.0 - sz / 2.0, y + 22.0 - sz / 2.0, sz, 6.0, c.default_accent);
            btn.push((cv.phys(x, y, 44.0, 44.0), Action::Shortcut(i)));
        }
    }
}

fn media_block(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, m: &Media, w: f32) {
    let acc = c.accent();
    let art = 92.0;
    let ay = 44.0;
    art_or_note(cv, m, PAD, ay, art, 16.0, acc);

    let x0 = PAD + art + 16.0;
    let x1 = w - PAD;
    cv.marquee(x0, x1 - 30.0, 64.0, 16.0, Weight::Semibold, WHITE, &m.title, c.t);
    cv.equalizer(x1 - 18.0, 52.0, 18.0, 14.0, c.t, m.playing, acc);
    let artist = cv.fit(13.0, Weight::Regular, &m.artist, x1 - x0);
    cv.text(x0, 82.0, 13.0, Weight::Regular, GRAY, &artist);

    // Progress bar (click to seek).
    let pos = m.position_now(c.now);
    let by = 97.0;
    if m.duration > 0.0 {
        let hot = c.is_hot(Action::Seek);
        let bh = if hot { 6.0 } else { 4.0 };
        cv.rrect(x0, by - bh / 2.0, x1 - x0, bh, bh / 2.0, WHITE.a(0.16));
        let f = (pos / m.duration).clamp(0.0, 1.0) as f32;
        if f > 0.0 {
            cv.rrect(x0, by - bh / 2.0, ((x1 - x0) * f).max(bh), bh, bh / 2.0, WHITE.a(0.92));
        }
        if hot {
            cv.circle(x0 + (x1 - x0) * f, by, 6.0, WHITE);
        }
        cv.text(x0, by + 15.0, 10.5, Weight::Regular, GRAY, &mmss(pos));
        cv.text_right(x1, by + 15.0, 10.5, Weight::Regular, GRAY, &format!("-{}", mmss(m.duration - pos)));
    }

    // Source switcher when several apps have a media session.
    if c.snap.sources.len() > 1 && !c.snap.source.is_empty() {
        let hot = c.is_hot(Action::Source);
        let label = format!("{}  ›", c.snap.source);
        let lw = cv.measure(10.5, Weight::Semibold, &label) + 16.0;
        let lx = (x0 + x1) / 2.0 - lw / 2.0;
        cv.rrect(lx, by + 4.0, lw, 16.0, 8.0, WHITE.a(if hot { 0.18 } else { 0.09 }));
        cv.text_center((x0 + x1) / 2.0, by + 15.5, 10.5, Weight::Semibold, if hot { WHITE } else { GRAY }, &label);
        btn.push((cv.phys(lx, by + 2.0, lw, 20.0), Action::Source));
        btn.push((cv.phys(x0 - 4.0, by - 9.0, x1 - x0 + 8.0, 18.0), Action::Seek));
    }

    // Transport controls.
    let mid = (x0 + x1) / 2.0;
    let cy = 130.0;
    for (x, a) in [(mid - 58.0, Action::Prev), (mid, Action::PlayPause), (mid + 58.0, Action::Next)] {
        if c.is_hot(a) {
            cv.circle(x, cy, 18.0, WHITE.a(0.12));
        }
        match a {
            Action::Prev => cv.skip(x, cy, 20.0, -1.0, WHITE),
            Action::Next => cv.skip(x, cy, 20.0, 1.0, WHITE),
            _ if m.playing => cv.pause(x, cy, 22.0, WHITE),
            _ => cv.play(x + 2.0, cy, 22.0, WHITE),
        }
        btn.push((cv.phys(x - 22.0, cy - 17.0, 44.0, 34.0), a));
    }
}

fn clock_block(cv: &mut Canvas, c: &Ctx, _w: f32) {
    let clk = &c.clock;
    let tw = cv.text(PAD - 2.0, 112.0, 58.0, Weight::Bold, WHITE, &c.hhmm());
    cv.text(PAD + tw + 2.0, 112.0, 20.0, Weight::Semibold, GRAY, &format!(":{:02}", clk.wSecond));
    let x = 268.0;
    cv.text(x, 68.0, 17.0, Weight::Semibold, WHITE, system::greeting(clk.wHour));
    let date = format!(
        "{}, {} {} {}",
        system::DAYS[clk.wDayOfWeek as usize % 7],
        clk.wDay,
        system::MONTHS[(clk.wMonth as usize).clamp(1, 12) - 1],
        clk.wYear
    );
    cv.text(x, 89.0, 12.5, Weight::Regular, GRAY, &date);
    match (&c.snap.weather, &c.snap.weather_err) {
        (Some(wx), _) => {
            cv.weather(x + 9.0, 107.0, 18.0, wx.code, wx.is_day);
            let line = format!(
                "{:.0}° {} · {:.0}°/{:.0}°",
                wx.temp,
                weather::describe(wx.code),
                wx.max,
                wx.min
            );
            let line = cv.fit(12.5, Weight::Semibold, &line, 230.0);
            cv.text(x + 24.0, 111.0, 12.5, Weight::Semibold, WHITE, &line);
            if !wx.place.is_empty() {
                cv.text(x, 130.0, 11.0, Weight::Regular, GRAY.a(0.85), &wx.place);
            }
        }
        (None, Some(e)) if c.cfg.features.weather => {
            let e = cv.fit(11.0, Weight::Regular, e, 230.0);
            cv.text(x, 111.0, 11.0, Weight::Regular, GRAY.a(0.85), &e);
        }
        _ => {
            cv.text(x, 111.0, 11.5, Weight::Regular, GRAY.a(0.8), "Scroll di sini untuk volume");
        }
    }
}

// ---------------- Sistem ----------------

fn system_tab(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    let s = c.snap;
    let gap = 8.0;
    let cw = (w - 2.0 * PAD - 3.0 * gap) / 4.0;
    let ch = 44.0;
    let col_x = |i: usize| PAD + i as f32 * (cw + gap);
    let rows = [42.0, 94.0, 146.0];

    // Row 1: CPU, RAM, GPU, VRAM.
    let y = rows[0];
    chip(cv, col_x(0), y, cw, ch, "CPU", &format!("{:.0}%", s.cpu * 100.0), false);
    chip_ring(cv, col_x(0), y, cw, ch, s.cpu, BLUE);
    chip(cv, col_x(1), y, cw, ch, "RAM", &format!("{:.0}%", s.ram * 100.0), false);
    chip_ring(cv, col_x(1), y, cw, ch, s.ram, PURPLE);
    match &s.gpu {
        Some(g) => {
            chip(cv, col_x(2), y, cw, ch, &format!("GPU · {}°C", g.temp), &format!("{:.0}%", g.util * 100.0), false);
            chip_ring(cv, col_x(2), y, cw, ch, g.util, GREEN);
            let used = g.mem_used as f64 / 1e9;
            let total = g.mem_total as f64 / 1e9;
            let label = format!("VRAM · {}", g.name.to_uppercase());
            chip(cv, col_x(3), y, cw, ch, &label, &format!("{used:.1}/{total:.0} GB"), false);
            chip_ring(cv, col_x(3), y, cw, ch, (used / total.max(0.1)) as f32, ORANGE);
        }
        None => {
            chip(cv, col_x(2), y, cw, ch, "GPU", "—", false);
            chip(cv, col_x(3), y, cw, ch, "VRAM", "—", false);
        }
    }

    // Row 2: download, upload, volume, mic.
    let y = rows[1];
    chip(cv, col_x(0), y, cw, ch, "UNDUH", &fmt_rate(s.net_down), false);
    chip_ring(cv, col_x(0), y, cw, ch, rate_progress(s.net_down), BLUE);
    chip(cv, col_x(1), y, cw, ch, "UNGGAH", &fmt_rate(s.net_up), false);
    chip_ring(cv, col_x(1), y, cw, ch, rate_progress(s.net_up), PURPLE);

    let hot = c.is_hot(Action::Mute);
    let vol = if s.muted { "Mute".to_string() } else { format!("{:.0}%", s.volume * 100.0) };
    chip(cv, col_x(2), y, cw, ch, "VOLUME · klik", &vol, hot);
    chip_ring(cv, col_x(2), y, cw, ch, if s.muted { 0.0 } else { s.volume }, if s.muted { GRAY } else { GREEN });
    btn.push((cv.phys(col_x(2), y, cw, ch), Action::Mute));

    let hot = c.is_hot(Action::Mic);
    let (mic_val, mic_col) = match s.mic_muted {
        None => ("—", GRAY),
        Some(true) => ("Mute", RED),
        Some(false) => ("Aktif", GREEN),
    };
    chip(cv, col_x(3), y, cw, ch, "MIC · klik", mic_val, hot);
    if s.mic_muted.is_some() {
        cv.mic(col_x(3) + cw - 20.0, y + ch / 2.0, 18.0, s.mic_muted == Some(true), mic_col);
        btn.push((cv.phys(col_x(3), y, cw, ch), Action::Mic));
    }

    // Row 3: output device (wide), weather, ping.
    let y = rows[2];
    let ww = cw * 2.0 + gap;
    let hot = c.is_hot(Action::Output);
    let label = if s.outputs.len() > 1 { "OUTPUT AUDIO · klik ganti" } else { "OUTPUT AUDIO" };
    let name = if s.output.is_empty() { "—" } else { s.output.as_str() };
    chip(cv, col_x(0), y, ww, ch, label, name, hot);
    cv.speaker(col_x(0) + ww - 30.0, y + ch / 2.0, 18.0, 0.7, false, WHITE.a(0.8));
    if s.outputs.len() > 1 {
        btn.push((cv.phys(col_x(0), y, ww, ch), Action::Output));
    }

    // Weather (one column).
    let wx_x = col_x(2);
    match (&s.weather, &s.weather_err) {
        (Some(wx), _) => {
            let label = if wx.place.is_empty() { "CUACA".to_string() } else { wx.place.to_uppercase() };
            let value = format!("{:.0}° {}", wx.temp, weather::describe(wx.code));
            chip(cv, wx_x, y, cw, ch, &label, &value, false);
        }
        (None, Some(_)) => chip(cv, wx_x, y, cw, ch, "CUACA", "Atur kota", false),
        _ => chip(cv, wx_x, y, cw, ch, "CUACA", if c.cfg.features.weather { "Memuat…" } else { "Nonaktif" }, false),
    }

    // Ping with a sparkline.
    let px = col_x(3);
    if c.cfg.features.ping {
        let (val, col) = match s.ping {
            Some(ms) if ms < 60 => (format!("{ms} ms"), GREEN),
            Some(ms) if ms < 120 => (format!("{ms} ms"), ORANGE),
            Some(ms) => (format!("{ms} ms"), RED),
            None if s.ping_hist.is_empty() => ("…".into(), GRAY),
            None => ("timeout".into(), RED),
        };
        let host = c.cfg.ping_host.to_uppercase();
        chip(cv, px, y, cw, ch, &format!("PING · {host}"), &val, false);
        sparkline(cv, px + cw - 44.0, y + 12.0, 34.0, 22.0, &s.ping_hist, col);
    } else {
        chip(cv, px, y, cw, ch, "PING", "Nonaktif", false);
    }
}

/// Mini bar chart of recent pings (timeouts drawn as red ticks).
fn sparkline(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, hist: &[Option<u32>], col: Rgba) {
    let n = 14usize;
    let start = hist.len().saturating_sub(n);
    let recent = &hist[start..];
    let max = recent.iter().flatten().copied().max().unwrap_or(1).max(40) as f32;
    let bw = w / n as f32;
    for (i, v) in recent.iter().enumerate() {
        let bx = x + (n - recent.len() + i) as f32 * bw;
        match v {
            Some(ms) => {
                let bh = (*ms as f32 / max * h).clamp(2.0, h);
                cv.rrect(bx, y + h - bh, (bw - 1.0).max(1.0), bh, 1.0, col.a(0.85));
            }
            None => cv.rrect(bx, y, (bw - 1.0).max(1.0), h, 1.0, RED.a(0.6)),
        }
    }
}

// ---------------- Alat ----------------

fn tools_tab(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    let gap = 10.0;
    let colw = (w - 2.0 * PAD - gap) / 2.0;
    let (lx, rx) = (PAD, PAD + colw + gap);
    let (y1, h1) = (42.0, 76.0);
    let (y2, h2) = (126.0, 74.0);
    let panel = |cv: &mut Canvas, x: f32, y: f32, h: f32, title: &str| {
        cv.rrect(x, y, colw, h, 14.0, WHITE.a(0.06));
        cv.text(x + 12.0, y + 17.0, 9.5, Weight::Semibold, GRAY, title);
    };
    let round_btn = |cv: &mut Canvas, btn: &mut Buttons, cx: f32, cy: f32, a: Action, hot: bool, col: Rgba| {
        cv.circle(cx, cy, 16.0, col.a(if hot { 0.35 } else { 0.22 }));
        btn.push((cv.phys(cx - 17.0, cy - 17.0, 34.0, 34.0), a));
    };

    // Stopwatch.
    panel(cv, lx, y1, h1, "STOPWATCH");
    let sw = c.stopwatch;
    let col = if sw.running() { GREEN } else { WHITE };
    cv.text(lx + 12.0, y1 + 56.0, 26.0, Weight::Bold, col, &stopwatch_text(sw.elapsed(c.now)));
    let cy = y1 + 42.0;
    let (bx1, bx2) = (lx + colw - 62.0, lx + colw - 24.0);
    round_btn(cv, btn, bx1, cy, Action::SwToggle, c.is_hot(Action::SwToggle), GREEN);
    if sw.running() {
        cv.pause(bx1, cy, 13.0, WHITE);
    } else {
        cv.play(bx1 + 1.5, cy, 13.0, WHITE);
    }
    round_btn(cv, btn, bx2, cy, Action::SwReset, c.is_hot(Action::SwReset), WHITE);
    cv.rrect(bx2 - 5.0, cy - 5.0, 10.0, 10.0, 2.0, WHITE);

    // Quick reminders.
    let title = match c.reminders.first() {
        Some(r) => format!("PENGINGAT · {} aktif · {} lagi", c.reminders.len(), timer_text(r, c.now)),
        None => "PENGINGAT CEPAT".into(),
    };
    panel(cv, lx, y2, h2, &title);
    let mut x = lx + 12.0;
    let presets: [(u32, &str); 5] = [(5, "5m"), (10, "10m"), (15, "15m"), (30, "30m"), (60, "1j")];
    // Make room for "Batal" while reminders are running.
    let shown = if c.reminders.is_empty() { 5 } else { 4 };
    for (min, label) in presets.into_iter().take(shown) {
        let cw = 38.0;
        let a = Action::Remind(min);
        cv.rrect(x, y2 + 32.0, cw, 28.0, 9.0, WHITE.a(if c.is_hot(a) { 0.18 } else { 0.08 }));
        cv.text_center(x + cw / 2.0, y2 + 50.5, 12.0, Weight::Semibold, WHITE, label);
        btn.push((cv.phys(x, y2 + 32.0, cw, 28.0), a));
        x += cw + 6.0;
    }
    if !c.reminders.is_empty() {
        let a = Action::RemindCancel;
        let cw = lx + colw - 12.0 - x;
        cv.rrect(x, y2 + 32.0, cw, 28.0, 9.0, RED.a(if c.is_hot(a) { 0.45 } else { 0.25 }));
        cv.text_center(x + cw / 2.0, y2 + 50.5, 11.5, Weight::Semibold, WHITE, "Batal");
        btn.push((cv.phys(x, y2 + 32.0, cw, 28.0), a));
    }

    // Screenshot.
    panel(cv, rx, y1, h1, "SCREENSHOT · tersimpan di Pictures\\Screenshots");
    let bw = (colw - 24.0 - 8.0) / 2.0;
    for (i, (a, label)) in [(Action::ShotFull, "Layar penuh"), (Action::ShotArea, "Pilih area")].into_iter().enumerate() {
        let bx = rx + 12.0 + i as f32 * (bw + 8.0);
        let by = y1 + 30.0;
        cv.rrect(bx, by, bw, 34.0, 10.0, c.default_accent.a(if c.is_hot(a) { 0.55 } else { 0.32 }));
        camera_shutter(cv, bx + 18.0, by + 17.0, 14.0, WHITE);
        cv.text(bx + 32.0, by + 21.5, 12.0, Weight::Semibold, WHITE, label);
        btn.push((cv.phys(bx, by, bw, 34.0), a));
    }

    // Alarm (scroll to change the time, click to arm).
    let al = c.alarm;
    panel(cv, rx, y2, h2, "ALARM · scroll ubah jam, klik aktifkan");
    let hot = c.is_hot(Action::Alarm);
    let col = if al.armed { ORANGE } else { WHITE.a(0.7) };
    bell(cv, rx + 24.0, y2 + 47.0, 18.0, col);
    cv.text(rx + 42.0, y2 + 56.0, 26.0, Weight::Bold, col, &format!("{:02}:{:02}", al.hour, al.minute));
    let (pw, ph) = (64.0, 26.0);
    let (px, py) = (rx + colw - 12.0 - pw, y2 + 33.0);
    cv.rrect(px, py, pw, ph, 13.0, if al.armed { ORANGE } else { WHITE.a(if hot { 0.2 } else { 0.1 }) });
    cv.text_center(px + pw / 2.0, py + 17.5, 11.5, Weight::Semibold, WHITE, if al.armed { "AKTIF" } else { "MATI" });
    btn.push((cv.phys(rx, y2, colw, h2), Action::Alarm));
}

// ---------------- Clip ----------------

fn clip_tab(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    let items = &c.clip.items;
    if items.is_empty() {
        cv.text_center(w / 2.0, 104.0, 14.0, Weight::Semibold, WHITE, "Riwayat clipboard kosong");
        cv.text_center(
            w / 2.0,
            125.0,
            11.5,
            Weight::Regular,
            GRAY,
            "Copy teks atau gambar, muncul di sini (hanya di memori, tidak disimpan)",
        );
        return;
    }
    let rh = 31.0;
    for (i, it) in items.iter().take(4).enumerate() {
        let y = 42.0 + i as f32 * rh;
        let a = Action::ClipItem(i);
        let hot = c.is_hot(a);
        cv.rrect(PAD, y, w - 2.0 * PAD, rh - 4.0, 9.0, WHITE.a(if hot { 0.13 } else { 0.05 }));
        let x = PAD + 40.0;
        let max = w - PAD - 60.0 - x;
        match &it.data {
            clip::Data::Text(t) => {
                cv.rrect(PAD + 8.0, y + 5.0, 22.0, 17.0, 5.0, c.default_accent.a(0.3));
                cv.text_center(PAD + 19.0, y + 17.5, 10.0, Weight::Bold, WHITE, "T");
                let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let one = cv.fit(12.0, Weight::Regular, &one, max);
                cv.text(x, y + 18.0, 12.0, Weight::Regular, WHITE, &one);
            }
            clip::Data::Image { thumb, w: iw, h: ih, .. } => {
                cv.image(thumb, PAD + 8.0, y + 2.5, 22.0, 4.0);
                cv.text(x, y + 18.0, 12.0, Weight::Regular, WHITE, &format!("Gambar {iw}×{ih}"));
            }
        }
        cv.text_right(w - PAD - 10.0, y + 18.0, 10.5, Weight::Regular, GRAY, if hot { "klik = salin" } else { &it.time });
        btn.push((cv.phys(PAD, y, w - 2.0 * PAD, rh - 4.0), a));
    }
    let by = 172.0;
    cv.text(PAD, by + 17.0, 10.5, Weight::Regular, GRAY, "Klik = salin lagi · klik kanan = hapus");
    let bw = 120.0;
    let bx = w - PAD - bw;
    let hot = c.is_hot(Action::ClipClear);
    cv.rrect(bx, by, bw, 26.0, 13.0, WHITE.a(if hot { 0.16 } else { 0.08 }));
    cv.text_center(bx + bw / 2.0, by + 17.5, 11.0, Weight::Semibold, if hot { WHITE } else { GRAY }, "Bersihkan semua");
    btn.push((cv.phys(bx, by, bw, 26.0), Action::ClipClear));
}

// ---------------- Shelf ----------------

fn shelf_tab(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    let items = &c.shelf.items;
    let (ax, ay, aw, ah) = (PAD, 44.0, w - 2.0 * PAD, 150.0);
    if items.is_empty() || c.file_drag {
        let col = if c.file_drag { c.default_accent } else { WHITE.a(0.3) };
        if c.file_drag {
            cv.rrect(ax, ay, aw, ah, 16.0, c.default_accent.a(0.1));
        }
        cv.stroke_dashed(rrect_path(ax, ay, aw, ah, 16.0), 1.5, 7.0, col);
        let cx = w / 2.0;
        let bob = if c.file_drag { (c.t * 6.0).sin() * 3.0 } else { 0.0 };
        let ay2 = ay + 50.0 + bob;
        let acol = if c.file_drag { c.default_accent } else { WHITE.a(0.7) };
        cv.line(cx, ay2 - 18.0, cx, ay2 + 6.0, 2.4, acol);
        cv.line(cx - 9.0, ay2 - 3.0, cx, ay2 + 6.0, 2.4, acol);
        cv.line(cx + 9.0, ay2 - 3.0, cx, ay2 + 6.0, 2.4, acol);
        let title = if c.file_drag { "Lepas untuk menyimpan di Shelf" } else { "Seret file ke notch" };
        cv.text_center(cx, ay + 96.0, 14.0, Weight::Semibold, WHITE, title);
        cv.text_center(
            cx,
            ay + 117.0,
            11.0,
            Weight::Regular,
            GRAY,
            "Klik = buka · seret keluar = salin/pindah · klik kanan = hapus",
        );
        return;
    }
    let (tw, th, gap) = (80.0, 72.0, 8.0);
    let cols = 5;
    let x0 = (w - (cols as f32 * tw + (cols - 1) as f32 * gap)) / 2.0;
    let slots = items.len() + 1; // + "clear all" tile
    for i in 0..slots.min(10) {
        let x = x0 + (i % cols) as f32 * (tw + gap);
        let y = 44.0 + (i / cols) as f32 * (th + gap);
        if i == items.len() {
            let hot = c.is_hot(Action::ShelfClear);
            cv.rrect(x, y, tw, th, 12.0, WHITE.a(if hot { 0.12 } else { 0.04 }));
            let col = if hot { RED } else { GRAY };
            let (cx, cy) = (x + tw / 2.0, y + 28.0);
            cv.line(cx - 8.0, cy - 8.0, cx + 8.0, cy + 8.0, 2.2, col);
            cv.line(cx + 8.0, cy - 8.0, cx - 8.0, cy + 8.0, 2.2, col);
            cv.text_center(cx, y + 60.0, 10.5, Weight::Semibold, col, "Hapus semua");
            btn.push((cv.phys(x, y, tw, th), Action::ShelfClear));
            continue;
        }
        let it = &items[i];
        let hot = c.is_hot(Action::ShelfItem(i));
        cv.rrect(x, y, tw, th, 12.0, WHITE.a(if hot { 0.14 } else { 0.06 }));
        icon_or_letter(cv, it.icon.as_ref(), &it.name, x + 20.0, y + 6.0, 40.0, 6.0, c.default_accent);
        let name = cv.fit(10.5, Weight::Regular, &it.name, tw - 8.0);
        cv.text_center(x + tw / 2.0, y + 62.0, 10.5, Weight::Regular, WHITE.a(0.9), &name);
        btn.push((cv.phys(x, y, tw, th), Action::ShelfItem(i)));
    }
}

// ---------------- Notif ----------------

fn notif_tab(cv: &mut Canvas, c: &Ctx, btn: &mut Buttons, w: f32) {
    let list = &c.snap.notifs;
    if list.is_empty() {
        cv.text_center(w / 2.0, 104.0, 14.0, Weight::Semibold, WHITE, "Belum ada notifikasi baru");
        cv.text_center(
            w / 2.0,
            125.0,
            11.5,
            Weight::Regular,
            GRAY,
            "Notifikasi Windows (Discord, WhatsApp, dll) muncul di sini",
        );
        return;
    }
    let rh = 40.0;
    for (i, n) in list.iter().take(3).enumerate() {
        let y = 42.0 + i as f32 * rh;
        icon_or_letter(cv, n.icon.as_deref(), &n.app, PAD, y + 6.0, 28.0, 8.0, c.default_accent);
        let x = PAD + 38.0;
        let meta = format!("{} · {}", n.app, n.time);
        let meta = cv.fit(10.5, Weight::Regular, &meta, 130.0);
        let mw = cv.text_right(w - PAD, y + 17.0, 10.5, Weight::Regular, GRAY, &meta);
        let title = cv.fit(12.5, Weight::Semibold, &n.title, w - PAD - mw - 10.0 - x);
        cv.text(x, y + 17.0, 12.5, Weight::Semibold, WHITE, &title);
        let body = cv.fit(11.5, Weight::Regular, &n.body, w - PAD - x);
        cv.text(x, y + 34.0, 11.5, Weight::Regular, GRAY, &body);
    }
    let bw = 132.0;
    let bx = w / 2.0 - bw / 2.0;
    let by = 168.0;
    let hot = c.is_hot(Action::NotifClear);
    cv.rrect(bx, by, bw, 26.0, 13.0, WHITE.a(if hot { 0.16 } else { 0.08 }));
    cv.text_center(w / 2.0, by + 17.5, 11.0, Weight::Semibold, if hot { WHITE } else { GRAY }, "Bersihkan semua");
    btn.push((cv.phys(bx, by, bw, 26.0), Action::NotifClear));
}
