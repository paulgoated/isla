//! Background thread (COM MTA) that polls media sessions, audio, system
//! sensors and notifications, publishes them in `Shared`, and executes
//! UI commands.

use crate::config;
use crate::notify::{Notif, Notifier};
use crate::lyrics::Lyrics;
use crate::sensors::{self, Audio, BtDevice, Gpu, NetSampler, Nvml};
use crate::update::Release;
use crate::system::{self, Battery, CpuSampler};
use crate::weather::WeatherNow;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::{IntSize, Pixmap};
use windows::core::Result;
use windows::Graphics::Imaging::*;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as Props,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::IRandomAccessStreamWithContentType;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

#[derive(Clone)]
pub struct Media {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// Seconds into the track at `stamp`.
    pub position: f64,
    pub duration: f64,
    pub stamp: Instant,
    pub art: Option<Arc<Pixmap>>,
    pub accent: Option<[f32; 3]>,
    pub track_id: u64,
}

impl Media {
    pub fn position_now(&self, now: Instant) -> f64 {
        let mut p = self.position;
        if self.playing {
            p += now.saturating_duration_since(self.stamp).as_secs_f64();
        }
        if self.duration > 0.0 {
            p = p.min(self.duration);
        }
        p.max(0.0)
    }
}

#[derive(Clone, Default)]
pub struct Shared {
    pub media: Option<Media>,
    pub volume: f32,
    pub muted: bool,
    /// Bumped whenever volume or mute changes (0 = not read yet).
    pub vol_seq: u64,
    /// None = no microphone.
    pub mic_muted: Option<bool>,
    pub mic_seq: u64,
    pub outputs: Vec<String>,
    pub output: String,
    pub output_seq: u64,
    pub cpu: f32,
    pub ram: f32,
    pub battery: Option<Battery>,
    pub gpu: Option<Gpu>,
    pub net_down: f64,
    pub net_up: f64,
    pub mic_apps: Vec<String>,
    pub cam_apps: Vec<String>,
    pub weather: Option<WeatherNow>,
    pub weather_err: Option<String>,
    /// Newest first.
    pub notifs: Vec<Arc<Notif>>,
    pub notif_seq: u64,
    /// Latest ping in ms (None = timeout) and recent history, oldest first.
    pub ping: Option<u32>,
    pub ping_hist: Vec<Option<u32>>,
    pub bluetooth: Vec<BtDevice>,
    /// Media apps with a session, and the one being shown/controlled.
    pub sources: Vec<String>,
    pub source: String,
    pub lyrics: Option<Arc<Lyrics>>,
    pub update: Option<Release>,
}

pub enum Cmd {
    PlayPause,
    Next,
    Prev,
    Seek(f64),
    SetVolume(f32),
    ToggleMute,
    ToggleMic,
    NextOutput,
    ClearNotifs,
    NextSource,
}

pub fn spawn(rx: Receiver<Cmd>) -> Arc<Mutex<Shared>> {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let s2 = shared.clone();
    std::thread::Builder::new()
        .name("notch-worker".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            Worker::new(s2).run(rx);
        })
        .expect("spawn worker");
    crate::weather::spawn(shared.clone());
    crate::lyrics::spawn(shared.clone());
    crate::update::spawn(shared.clone());
    spawn_ping(shared.clone());
    shared
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    manager: Option<Manager>,
    manager_retry: Instant,
    session: Option<Session>,
    start_ticks: i64,
    key: String,
    track_id: u64,
    key_polls: u32,
    art: Option<Arc<Pixmap>>,
    accent: Option<[f32; 3]>,
    art_tries: u32,
    audio: Option<Audio>,
    vol: Option<IAudioEndpointVolume>,
    mic: Option<IAudioEndpointVolume>,
    endpoints_refresh: Instant,
    last_vol: Option<(f32, bool)>,
    vol_seq: u64,
    last_mic: Option<Option<bool>>,
    mic_seq: u64,
    output_ids: Vec<String>,
    output_id: String,
    output_seq: u64,
    cpu: CpuSampler,
    net: NetSampler,
    nvml: Option<Option<Nvml>>,
    notifier: Notifier,
    notif_seq: u64,
    next_media: Instant,
    next_stats: Instant,
    next_notif: Instant,
    next_bt: Instant,
    /// Media app the user picked with the source switcher.
    pinned: Option<String>,
}

fn ticks_to_secs(t: i64) -> f64 {
    t as f64 / 1e7
}

impl Worker {
    fn new(shared: Arc<Mutex<Shared>>) -> Worker {
        let now = Instant::now();
        Worker {
            shared,
            manager: None,
            manager_retry: now,
            session: None,
            start_ticks: 0,
            key: String::new(),
            track_id: 0,
            key_polls: 0,
            art: None,
            accent: None,
            art_tries: 0,
            audio: None,
            vol: None,
            mic: None,
            endpoints_refresh: now,
            last_vol: None,
            vol_seq: 0,
            last_mic: None,
            mic_seq: 0,
            output_ids: Vec::new(),
            output_id: String::new(),
            output_seq: 0,
            cpu: CpuSampler::default(),
            net: NetSampler::default(),
            nvml: None,
            notifier: Notifier::default(),
            notif_seq: 0,
            next_media: now,
            next_stats: now,
            next_notif: now,
            next_bt: now,
            pinned: None,
        }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let cfg = config::current();
            let f = &cfg.features;
            let now = Instant::now();
            self.refresh_endpoints(now);
            self.poll_volume();
            if now >= self.next_media {
                self.next_media = now + Duration::from_millis(400);
                if f.media {
                    self.poll_media(now);
                } else {
                    self.shared.lock().unwrap().media = None;
                }
            }
            if now >= self.next_stats {
                self.next_stats = now + Duration::from_secs(1);
                self.poll_stats(f);
            }
            if now >= self.next_bt {
                self.next_bt = now + Duration::from_secs(20);
                let bt = if f.bluetooth { sensors::bluetooth_batteries() } else { Vec::new() };
                self.shared.lock().unwrap().bluetooth = bt;
            }
            if f.notifications && now >= self.next_notif {
                self.next_notif = now + Duration::from_millis(1500);
                self.poll_notifs();
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        let res: Result<()> = (|| {
            match cmd {
                Cmd::PlayPause | Cmd::Next | Cmd::Prev | Cmd::Seek(_) => {
                    let Some(s) = &self.session else { return Ok(()) };
                    let op = match cmd {
                        Cmd::PlayPause => s.TryTogglePlayPauseAsync()?,
                        Cmd::Next => s.TrySkipNextAsync()?,
                        Cmd::Prev => s.TrySkipPreviousAsync()?,
                        Cmd::Seek(secs) => s.TryChangePlaybackPositionAsync(
                            self.start_ticks + (secs * 1e7) as i64,
                        )?,
                        _ => unreachable!(),
                    };
                    op.join()?;
                    // Refresh soon so the UI reflects the change quickly.
                    self.next_media = Instant::now() + Duration::from_millis(150);
                }
                Cmd::SetVolume(v) => {
                    if let Some(vol) = &self.vol {
                        unsafe {
                            vol.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null())?;
                            if v > 0.0 && vol.GetMute()?.as_bool() {
                                vol.SetMute(false, std::ptr::null())?;
                            }
                        }
                    }
                }
                Cmd::ToggleMute => {
                    if let Some(vol) = &self.vol {
                        unsafe {
                            let m = vol.GetMute()?.as_bool();
                            vol.SetMute(!m, std::ptr::null())?;
                        }
                    }
                }
                Cmd::ToggleMic => {
                    if let Some(mic) = &self.mic {
                        unsafe {
                            let m = mic.GetMute()?.as_bool();
                            mic.SetMute(!m, std::ptr::null())?;
                        }
                    }
                }
                Cmd::NextOutput => {
                    if let Some(audio) = &self.audio {
                        if self.output_ids.len() > 1 {
                            let i = self.output_ids.iter().position(|x| *x == self.output_id).unwrap_or(0);
                            let next = self.output_ids[(i + 1) % self.output_ids.len()].clone();
                            audio.set_default_output(&next)?;
                            // Re-read everything for the new default device.
                            self.endpoints_refresh = Instant::now();
                        }
                    }
                }
                Cmd::NextSource => {
                    if let Some(mgr) = &self.manager {
                        let ids = session_ids(mgr);
                        if ids.len() > 1 {
                            let cur = self.session.as_ref().and_then(|s| s.SourceAppUserModelId().ok()).map(|h| h.to_string_lossy());
                            let i = cur.and_then(|c| ids.iter().position(|x| *x == c)).unwrap_or(0);
                            self.pinned = Some(ids[(i + 1) % ids.len()].clone());
                            self.next_media = Instant::now();
                        }
                    }
                }
                Cmd::ClearNotifs => {
                    self.shared.lock().unwrap().notifs.clear();
                }
            }
            Ok(())
        })();
        let _ = res;
    }

    // ---------------- audio ----------------

    fn refresh_endpoints(&mut self, now: Instant) {
        if now < self.endpoints_refresh {
            return;
        }
        // Re-acquire periodically so device switches are picked up.
        self.endpoints_refresh = now + Duration::from_secs(3);
        if self.audio.is_none() {
            self.audio = Audio::new().ok();
        }
        let Some(audio) = &self.audio else { return };
        self.vol = audio.render_volume().ok();
        self.mic = audio.capture_volume().ok();
        if let Ok((list, def)) = audio.outputs() {
            let changed = def != self.output_id;
            if changed && !self.output_id.is_empty() {
                self.output_seq += 1;
            }
            self.output_ids = list.iter().map(|(id, _)| id.clone()).collect();
            self.output_id = def.clone();
            let name = list
                .iter()
                .find(|(id, _)| *id == def)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            let mut sh = self.shared.lock().unwrap();
            sh.outputs = list.into_iter().map(|(_, n)| n).collect();
            sh.output = name;
            sh.output_seq = self.output_seq;
        }
    }

    fn poll_volume(&mut self) {
        let read = |v: &Option<IAudioEndpointVolume>| -> Option<(f32, bool)> {
            let v = v.as_ref()?;
            unsafe {
                match (v.GetMasterVolumeLevelScalar(), v.GetMute()) {
                    (Ok(l), Ok(m)) => Some((l, m.as_bool())),
                    _ => None,
                }
            }
        };
        if let Some((level, muted)) = read(&self.vol) {
            let changed = match self.last_vol {
                None => true,
                Some((l, m)) => (l - level).abs() > 0.004 || m != muted,
            };
            if changed {
                self.last_vol = Some((level, muted));
                self.vol_seq += 1;
                let mut sh = self.shared.lock().unwrap();
                sh.volume = level;
                sh.muted = muted;
                sh.vol_seq = self.vol_seq;
            }
        }
        let mic = read(&self.mic).map(|(_, m)| m);
        if self.last_mic != Some(mic) {
            if self.last_mic.is_some() {
                self.mic_seq += 1;
            }
            self.last_mic = Some(mic);
            let mut sh = self.shared.lock().unwrap();
            sh.mic_muted = mic;
            sh.mic_seq = self.mic_seq;
        }
    }

    // ---------------- stats ----------------

    fn poll_stats(&mut self, f: &config::Features) {
        let cpu = self.cpu.sample();
        let ram = system::ram_load();
        let bat = system::battery();
        let (down, up) = if f.network { self.net.sample() } else { (0.0, 0.0) };
        let gpu = if f.gpu {
            self.nvml.get_or_insert_with(Nvml::load).as_ref().and_then(|n| n.sample())
        } else {
            None
        };
        let (mic_apps, cam_apps) = if f.privacy {
            (sensors::apps_using("microphone"), sensors::apps_using("webcam"))
        } else {
            (Vec::new(), Vec::new())
        };
        let mut sh = self.shared.lock().unwrap();
        sh.cpu = cpu;
        sh.ram = ram;
        sh.battery = bat;
        sh.net_down = down;
        sh.net_up = up;
        sh.gpu = gpu;
        sh.mic_apps = mic_apps;
        sh.cam_apps = cam_apps;
    }

    fn poll_notifs(&mut self) {
        let fresh = self.notifier.poll();
        if fresh.is_empty() {
            return;
        }
        let mut sh = self.shared.lock().unwrap();
        for n in fresh {
            sh.notifs.insert(0, Arc::new(n));
            self.notif_seq += 1;
        }
        sh.notifs.truncate(20);
        sh.notif_seq = self.notif_seq;
    }

    // ---------------- media ----------------

    fn poll_media(&mut self, now: Instant) {
        if self.manager.is_none() && now >= self.manager_retry {
            self.manager = Manager::RequestAsync().and_then(|op| op.join()).ok();
            self.manager_retry = now + Duration::from_secs(5);
        }
        let ids = self.manager.as_ref().map(session_ids).unwrap_or_default();
        if self.pinned.as_ref().is_some_and(|p| !ids.contains(p)) {
            self.pinned = None; // the pinned app closed
        }
        let session = self.manager.as_ref().and_then(|m| pick_session(m, self.pinned.as_deref()));
        self.session = session.clone();
        {
            let cur = session.as_ref().and_then(|s| s.SourceAppUserModelId().ok()).map(|h| h.to_string_lossy());
            let mut sh = self.shared.lock().unwrap();
            sh.sources = ids.iter().map(|i| pretty_app(i)).collect();
            sh.source = cur.map(|c| pretty_app(&c)).unwrap_or_default();
        }
        let media = match session {
            Some(s) => match self.read_media(&s, now) {
                Ok(m) => m,
                Err(_) => return, // transient failure: keep previous state
            },
            None => None,
        };
        self.shared.lock().unwrap().media = media;
    }

    fn read_media(&mut self, s: &Session, now: Instant) -> Result<Option<Media>> {
        let props = s.TryGetMediaPropertiesAsync()?.join()?;
        let title = props.Title()?.to_string_lossy();
        let artist = props.Artist()?.to_string_lossy();
        if title.trim().is_empty() {
            return Ok(None);
        }
        let playing = s.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;

        let tl = s.GetTimelineProperties()?;
        let start = tl.StartTime()?.Duration;
        let end = tl.EndTime()?.Duration;
        self.start_ticks = start;
        let duration = ticks_to_secs(end - start).max(0.0);
        let mut position = ticks_to_secs(tl.Position()?.Duration - start);
        let updated = tl.LastUpdatedTime()?.UniversalTime;
        if playing && updated > 0 {
            let elapsed = ticks_to_secs(system::now_filetime() - updated);
            if (0.0..21600.0).contains(&elapsed) {
                position += elapsed;
            }
        }
        if duration > 0.0 {
            position = position.clamp(0.0, duration);
        }

        let app = s.SourceAppUserModelId().map(|h| h.to_string_lossy()).unwrap_or_default();
        let key = format!("{app}\u{1}{title}\u{1}{artist}");
        if key != self.key {
            self.key = key;
            self.track_id += 1;
            self.key_polls = 0;
            self.art = None;
            self.accent = None;
            self.art_tries = 0;
        } else {
            self.key_polls += 1;
        }
        // Wait one poll after a track change: apps often update the
        // thumbnail slightly after the title.
        if self.art.is_none() && self.key_polls >= 1 && self.art_tries < 8 {
            self.art_tries += 1;
            if let Ok(pm) = load_art(&props) {
                self.accent = accent_of(&pm);
                self.art = Some(Arc::new(pm));
            }
        }

        Ok(Some(Media {
            title,
            artist,
            playing,
            position,
            duration,
            stamp: now,
            art: self.art.clone(),
            accent: self.accent,
            track_id: self.track_id,
        }))
    }
}

/// App ids of all media sessions, in system order.
fn session_ids(mgr: &Manager) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(list) = mgr.GetSessions() {
        for i in 0..list.Size().unwrap_or(0) {
            if let Ok(id) = list.GetAt(i).and_then(|s| s.SourceAppUserModelId()) {
                let id = id.to_string_lossy();
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// Spotify.exe -> Spotify, MSEdge -> Edge, Chrome -> Chrome.
pub fn pretty_app(id: &str) -> String {
    let base = id.rsplit(['!', '\\']).next().unwrap_or(id);
    let base = base.trim_end_matches(".exe").trim_end_matches(".EXE");
    match base.to_lowercase().as_str() {
        "msedge" => "Edge".into(),
        "chrome" => "Chrome".into(),
        "308046b0af4a39cb" | "firefox" => "Firefox".into(),
        _ => {
            let mut c = base.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

fn pick_session(mgr: &Manager, pinned: Option<&str>) -> Option<Session> {
    if let (Some(p), Ok(list)) = (pinned, mgr.GetSessions()) {
        for i in 0..list.Size().unwrap_or(0) {
            if let Ok(s) = list.GetAt(i) {
                if s.SourceAppUserModelId().is_ok_and(|h| h.to_string_lossy() == p) {
                    return Some(s);
                }
            }
        }
    }
    let current = mgr.GetCurrentSession().ok();
    if let Some(c) = &current {
        if is_playing(c) {
            return current;
        }
    }
    if let Ok(list) = mgr.GetSessions() {
        let n = list.Size().unwrap_or(0);
        for i in 0..n {
            if let Ok(s) = list.GetAt(i) {
                if is_playing(&s) {
                    return Some(s);
                }
            }
        }
    }
    current
}

fn is_playing(s: &Session) -> bool {
    s.GetPlaybackInfo()
        .and_then(|i| i.PlaybackStatus())
        .map(|st| st == Status::Playing)
        .unwrap_or(false)
}

fn load_art(props: &Props) -> Result<Pixmap> {
    let stream = props.Thumbnail()?.OpenReadAsync()?.join()?;
    decode_square(&stream, 160)
}

/// Decodes an image stream, center-cropped to a `size` square, as a
/// premultiplied RGBA pixmap.
pub fn decode_square(stream: &IRandomAccessStreamWithContentType, size: u32) -> Result<Pixmap> {
    let dec = BitmapDecoder::CreateAsync(stream)?.join()?;
    let (w, h) = (dec.PixelWidth()?.max(1), dec.PixelHeight()?.max(1));
    let m = w.min(h) as u64;
    let sw = ((w as u64 * size as u64) / m).max(size as u64) as u32;
    let sh = ((h as u64 * size as u64) / m).max(size as u64) as u32;
    let t = BitmapTransform::new()?;
    t.SetScaledWidth(sw)?;
    t.SetScaledHeight(sh)?;
    t.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
    t.SetBounds(BitmapBounds {
        X: (sw - size) / 2,
        Y: (sh - size) / 2,
        Width: size,
        Height: size,
    })?;
    let px = dec
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &t,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )?
        .join()?;
    let mut data = px.DetachPixelData()?.to_vec();
    for p in data.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    let isz = IntSize::from_wh(size, size).expect("size");
    Pixmap::from_vec(data, isz)
        .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
}

/// A vivid accent color from the artwork, weighted toward saturated pixels.
fn accent_of(pm: &Pixmap) -> Option<[f32; 3]> {
    let mut sum = [0f32; 3];
    let mut wsum = 0f32;
    for p in pm.data().chunks_exact(4) {
        let c = [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0];
        let mx = c[0].max(c[1]).max(c[2]);
        let mn = c[0].min(c[1]).min(c[2]);
        let sat = mx - mn;
        let w = sat * sat * mx + 0.002;
        for i in 0..3 {
            sum[i] += c[i] * w;
        }
        wsum += w;
    }
    if wsum <= 0.0 {
        return None;
    }
    let mut c = sum.map(|v| v / wsum);
    let l = (c[0] + c[1] + c[2]) / 3.0;
    c = c.map(|v| (l + (v - l) * 1.6).clamp(0.0, 1.0));
    let mx = c[0].max(c[1]).max(c[2]);
    if mx < 0.02 {
        return None;
    }
    let k = 0.95 / mx;
    Some(c.map(|v| (v * k).clamp(0.0, 1.0)))
}

/// Pings `ping_host` every 2 s on its own thread (ICMP can block up to 1 s).
fn spawn_ping(shared: Arc<Mutex<Shared>>) {
    std::thread::Builder::new()
        .name("isla-ping".into())
        .spawn(move || loop {
            let cfg = config::current();
            if cfg.features.ping {
                let ms = sensors::ping(&cfg.ping_host, 1000);
                let mut sh = shared.lock().unwrap();
                sh.ping = ms;
                sh.ping_hist.push(ms);
                let extra = sh.ping_hist.len().saturating_sub(40);
                sh.ping_hist.drain(..extra);
            } else {
                let mut sh = shared.lock().unwrap();
                sh.ping = None;
                sh.ping_hist.clear();
            }
            std::thread::sleep(Duration::from_secs(2));
        })
        .expect("spawn ping");
}
