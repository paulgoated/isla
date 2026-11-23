//! User settings in %APPDATA%\Isla\config.toml, hot-reloaded on save.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub monitor: usize,
    /// Horizontal position along the top edge: -1 left, 0 center, 1 right.
    pub position: f32,
    pub size: f32,
    pub accent: String,
    pub hide_in_fullscreen: bool,
    pub hover_delay_ms: u64,
    pub focus_minutes: u32,
    /// Host for the ping / latency meter.
    pub ping_host: String,
    pub features: Features,
    pub weather: WeatherCfg,
    #[serde(rename = "shortcut")]
    pub shortcuts: Vec<Shortcut>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            monitor: 0,
            position: 0.0,
            size: 1.0,
            accent: "#0A84FF".into(),
            hide_in_fullscreen: true,
            hover_delay_ms: 140,
            focus_minutes: 25,
            ping_host: "1.1.1.1".into(),
            features: Features::default(),
            weather: WeatherCfg::default(),
            shortcuts: Vec::new(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Features {
    pub media: bool,
    pub system: bool,
    pub gpu: bool,
    pub network: bool,
    pub weather: bool,
    pub privacy: bool,
    pub lock_keys: bool,
    pub shortcuts: bool,
    pub shelf: bool,
    pub notifications: bool,
    pub volume_hud: bool,
    pub tools: bool,
    pub clipboard: bool,
    pub lyrics: bool,
    pub ping: bool,
    pub bluetooth: bool,
}

impl Default for Features {
    fn default() -> Self {
        Features {
            media: true,
            system: true,
            gpu: true,
            network: true,
            weather: true,
            privacy: true,
            lock_keys: true,
            shortcuts: true,
            shelf: true,
            notifications: true,
            volume_hud: true,
            tools: true,
            clipboard: true,
            lyrics: true,
            ping: true,
            bluetooth: true,
        }
    }
}

impl Features {
    pub fn flag_mut(&mut self, i: usize) -> Option<&mut bool> {
        Some(match i {
            0 => &mut self.media,
            1 => &mut self.system,
            2 => &mut self.gpu,
            3 => &mut self.network,
            4 => &mut self.weather,
            5 => &mut self.privacy,
            6 => &mut self.lock_keys,
            7 => &mut self.shortcuts,
            8 => &mut self.shelf,
            9 => &mut self.notifications,
            10 => &mut self.volume_hud,
            11 => &mut self.tools,
            12 => &mut self.clipboard,
            13 => &mut self.lyrics,
            14 => &mut self.ping,
            15 => &mut self.bluetooth,
            _ => return None,
        })
    }

    pub fn flag(&self, i: usize) -> bool {
        self.clone().flag_mut(i).map(|b| *b).unwrap_or(false)
    }
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct WeatherCfg {
    pub city: String,
    pub fahrenheit: bool,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Shortcut {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub args: String,
    /// Optional icon file (.ico / .exe / image); defaults to `path`.
    #[serde(default)]
    pub icon: String,
}

static CURRENT: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<Config>> {
    CURRENT.get_or_init(|| RwLock::new(Arc::new(Config::default())))
}

pub fn current() -> Arc<Config> {
    cell().read().unwrap().clone()
}

pub fn dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("Isla")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

pub fn modified() -> Option<SystemTime> {
    std::fs::metadata(path()).ok()?.modified().ok()
}

/// Loads the config, writing a commented default file first if missing.
/// On a parse error the previous config stays active and the message is returned.
pub fn load() -> Result<(), String> {
    let p = path();
    if !p.exists() {
        let _ = std::fs::create_dir_all(dir());
        let _ = std::fs::write(&p, default_text());
    }
    let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let cfg: Config = toml::from_str(&text).map_err(|e| e.message().to_string())?;
    *cell().write().unwrap() = Arc::new(cfg);
    Ok(())
}

pub fn parse_hex(s: &str) -> Option<[f32; 3]> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([
        ((v >> 16) & 255) as f32 / 255.0,
        ((v >> 8) & 255) as f32 / 255.0,
        (v & 255) as f32 / 255.0,
    ])
}

fn detected_shortcuts() -> Vec<(String, String, String, String)> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let local = env("LOCALAPPDATA");
    let roaming = env("APPDATA");
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let none = String::new;
    let candidates = [
        ("Explorer", format!("{windir}\\explorer.exe"), "", none()),
        ("Chrome", "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe".into(), "", none()),
        ("Edge", "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe".into(), "", none()),
        (
            "Discord",
            format!("{local}\\Discord\\Update.exe"),
            "--processStart Discord.exe",
            format!("{local}\\Discord\\app.ico"),
        ),
        ("Spotify", format!("{roaming}\\Spotify\\Spotify.exe"), "", none()),
        ("Steam", "C:\\Program Files (x86)\\Steam\\steam.exe".into(), "", none()),
        ("VS Code", format!("{local}\\Programs\\Microsoft VS Code\\Code.exe"), "", none()),
        ("Notepad", format!("{windir}\\System32\\notepad.exe"), "", none()),
        ("Kalkulator", format!("{windir}\\System32\\calc.exe"), "", none()),
    ];
    candidates
        .into_iter()
        .filter(|(_, p, _, _)| Path::new(p).exists())
        .take(6)
        .map(|(n, p, a, i)| (n.to_string(), p, a.to_string(), i))
        .collect()
}

fn default_text() -> String {
    let mut c = Config::default();
    c.shortcuts = detected_shortcuts()
        .into_iter()
        .map(|(name, path, args, icon)| Shortcut {
            name,
            path,
            args,
            icon: if Path::new(&icon).exists() { icon } else { String::new() },
        })
        .collect();
    to_text(&c)
}

/// TOML string: literal ('...') when possible so Windows paths stay readable.
fn q(s: &str) -> String {
    if !s.contains('\'') && !s.contains('\n') && !s.contains('\r') {
        format!("'{s}'")
    } else {
        toml::Value::String(s.to_string()).to_string()
    }
}

/// Writes `c` as the commented config file (used by the settings window).
pub fn save(c: &Config) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    std::fs::write(path(), to_text(c))
}

fn to_text(c: &Config) -> String {
    let f = &c.features;
    let mut s = format!(
        r##"# ================================================
#  Isla - pengaturan
#  Simpan file ini, perubahan langsung diterapkan.
#  (Bisa juga diatur lewat ikon tray -> Pengaturan.)
# ================================================

# Monitor: 0 = monitor utama, 1, 2, ... = monitor lain
monitor = {monitor}
# Posisi horizontal: 0 = tengah, -1 = paling kiri, 1 = paling kanan
position = {position:.3}
# Ukuran notch (1.0 = normal, 1.2 = lebih besar)
size = {size:.2}
# Warna aksen (hex)
accent = {accent}
# Sembunyikan notch saat game / video / presentasi fullscreen
hide_in_fullscreen = {hide}
# Jeda sebelum notch membesar saat disentuh kursor (milidetik)
hover_delay_ms = {hover}
# Durasi awal timer fokus (menit)
focus_minutes = {focus}
# Alamat untuk ukur ping (mis. 1.1.1.1 atau server game)
ping_host = {ping_host}

[features]
media = {media}          # info lagu + kontrol media
system = {system}         # tab Sistem (CPU, RAM, GPU, jaringan, audio)
gpu = {gpu}            # statistik GPU NVIDIA
network = {network}        # kecepatan internet
weather = {weather}        # cuaca dari Open-Meteo
privacy = {privacy}        # titik indikator mic (oranye) / kamera (hijau)
lock_keys = {lock_keys}      # HUD Caps Lock / Num Lock
shortcuts = {shortcuts}      # deretan shortcut app di tab Beranda
shelf = {shelf}          # tab Shelf: seret file ke notch
notifications = {notifications}  # notifikasi Windows tampil di notch
volume_hud = {volume_hud}     # HUD saat volume berubah
tools = {tools}          # tab Alat: stopwatch, pengingat, alarm, screenshot
clipboard = {clipboard}      # tab Clip: riwayat clipboard (tidak disimpan ke disk)
lyrics = {lyrics}         # lirik sinkron dari LRCLIB
ping = {ping}           # ping / latensi di tab Sistem
bluetooth = {bluetooth}      # baterai perangkat Bluetooth

[weather]
# Nama kota, contoh: "Jakarta". Kosong = pakai lokasi Windows.
city = {city}
fahrenheit = {fahrenheit}

# Shortcut app (maks 6 tampil). Isi: name, path, args dan icon (opsional).
"##,
        monitor = c.monitor,
        position = c.position.clamp(-1.0, 1.0),
        size = c.size,
        accent = q(&c.accent),
        hide = c.hide_in_fullscreen,
        hover = c.hover_delay_ms,
        focus = c.focus_minutes,
        media = f.media,
        system = f.system,
        gpu = f.gpu,
        network = f.network,
        weather = f.weather,
        privacy = f.privacy,
        lock_keys = f.lock_keys,
        shortcuts = f.shortcuts,
        shelf = f.shelf,
        notifications = f.notifications,
        volume_hud = f.volume_hud,
        tools = f.tools,
        clipboard = f.clipboard,
        lyrics = f.lyrics,
        ping = f.ping,
        bluetooth = f.bluetooth,
        ping_host = q(&c.ping_host),
        city = q(&c.weather.city),
        fahrenheit = c.weather.fahrenheit,
    );
    for sc in &c.shortcuts {
        s.push_str(&format!("\n[[shortcut]]\nname = {}\npath = {}\n", q(&sc.name), q(&sc.path)));
        if !sc.args.is_empty() {
            s.push_str(&format!("args = {}\n", q(&sc.args)));
        }
        if !sc.icon.is_empty() {
            s.push_str(&format!("icon = {}\n", q(&sc.icon)));
        }
    }
    s
}
