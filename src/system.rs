//! Small wrappers around Win32 system queries.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::{
    GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};

pub use windows::Win32::Foundation::SYSTEMTIME;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
    pub ac: bool,
}

pub fn battery() -> Option<Battery> {
    let mut st = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut st).ok()? };
    // 128 = no system battery, 255 = unknown status.
    if st.BatteryFlag & 128 != 0 || st.BatteryFlag == 255 || st.BatteryLifePercent > 100 {
        return None;
    }
    Some(Battery {
        percent: st.BatteryLifePercent,
        charging: st.BatteryFlag & 8 != 0,
        ac: st.ACLineStatus == 1,
    })
}

pub fn ram_load() -> f32 {
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        if GlobalMemoryStatusEx(&mut m).is_err() {
            return 0.0;
        }
    }
    m.dwMemoryLoad as f32 / 100.0
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

#[derive(Default)]
pub struct CpuSampler {
    idle: u64,
    total: u64,
}

impl CpuSampler {
    /// Busy fraction since the previous call.
    pub fn sample(&mut self) -> f32 {
        let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe {
            if GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)).is_err() {
                return 0.0;
            }
        }
        let idle = ft(i);
        let total = ft(k) + ft(u); // kernel time includes idle time
        let di = idle.saturating_sub(self.idle);
        let dt = total.saturating_sub(self.total);
        let first = self.total == 0;
        self.idle = idle;
        self.total = total;
        if first || dt == 0 {
            return 0.0;
        }
        (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0)
    }
}

/// Current UTC time as Windows ticks (100 ns since 1601), same clock as WinRT DateTime.
pub fn now_filetime() -> i64 {
    let f = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
    ft(f) as i64
}

pub fn local_time() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

/// True while a fullscreen game, video or presentation is in front.
pub fn fullscreen_active() -> bool {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(s) => s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE,
        Err(_) => false,
    }
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_NAME: PCWSTR = w!("Isla");
/// Value name used before the rename to Isla.
const LEGACY_RUN_NAME: PCWSTR = w!("RustNotch");

fn open_run(access: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut k = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), access, &mut k) };
    r.is_ok().then_some(k)
}

/// Carries over settings from the pre-rename "Rust Notch" build: the
/// %APPDATA%\RustNotch folder and the old autostart entry.
pub fn migrate_legacy() {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let old = std::path::Path::new(&appdata).join("RustNotch");
    let new = crate::config::dir();
    if old.is_dir() && !new.exists() {
        let _ = std::fs::create_dir_all(&new);
        for f in ["config.toml", "shelf.txt"] {
            let _ = std::fs::copy(old.join(f), new.join(f));
        }
    }
    if let Some(k) = open_run(KEY_READ) {
        let had = unsafe { RegQueryValueExW(k, LEGACY_RUN_NAME, None, None, None, None) }.is_ok();
        unsafe {
            let _ = RegCloseKey(k);
        }
        if had {
            set_autostart(true);
        }
    }
}

pub fn autostart_enabled() -> bool {
    let Some(k) = open_run(KEY_READ) else { return false };
    let r = unsafe { RegQueryValueExW(k, RUN_NAME, None, None, None, None) };
    unsafe {
        let _ = RegCloseKey(k);
    }
    r.is_ok()
}

pub fn set_autostart(on: bool) {
    let Some(k) = open_run(KEY_SET_VALUE) else { return };
    unsafe {
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let s = format!("\"{}\"", exe.display());
                let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let _ = RegSetValueExW(k, RUN_NAME, Some(0), REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(k, RUN_NAME);
        }
        let _ = RegDeleteValueW(k, LEGACY_RUN_NAME);
        let _ = RegCloseKey(k);
    }
}

pub const DAYS: [&str; 7] = ["Minggu", "Senin", "Selasa", "Rabu", "Kamis", "Jumat", "Sabtu"];
pub const DAYS_SHORT: [&str; 7] = ["Min", "Sen", "Sel", "Rab", "Kam", "Jum", "Sab"];
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
];

pub fn greeting(hour: u16) -> &'static str {
    match hour {
        4..=10 => "Selamat pagi",
        11..=14 => "Selamat siang",
        15..=17 => "Selamat sore",
        _ => "Selamat malam",
    }
}
