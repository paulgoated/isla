//! Hardware / OS sensors polled by the worker: mic & camera usage,
//! network throughput, NVIDIA GPU (NVML) and audio endpoints.

use std::ffi::c_void;
use std::time::Instant;
use windows::core::{s, w, Interface, Result, GUID, HRESULT, HSTRING, IUnknown, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDevicePropertyW,
    DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_FriendlyName, DEVPROPTYPE};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY, MIB_IF_TABLE2,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;

// ---------------------------------------------------------------------
// privacy: which apps use the microphone / camera right now
// ---------------------------------------------------------------------

/// `cap` is "microphone" or "webcam".
pub fn apps_using(cap: &str) -> Vec<String> {
    let base = format!(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\{cap}"
    );
    let mut out = Vec::new();
    scan(&base, false, &mut out);
    scan(&format!("{base}\\NonPackaged"), true, &mut out);
    out.sort();
    out.dedup();
    out
}

fn scan(path: &str, nonpackaged: bool, out: &mut Vec<String>) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), Some(0), KEY_READ, &mut key).is_err() {
            return;
        }
        let mut i = 0;
        loop {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let r = RegEnumKeyExW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None);
            if r.is_err() {
                break;
            }
            i += 1;
            let sub = String::from_utf16_lossy(&name[..len as usize]);
            if sub == "NonPackaged" {
                continue;
            }
            let start = qword(key, &sub, w!("LastUsedTimeStart")).unwrap_or(0);
            let stop = qword(key, &sub, w!("LastUsedTimeStop"));
            if start > 0 && stop == Some(0) {
                out.push(pretty(&sub, nonpackaged));
            }
        }
        let _ = RegCloseKey(key);
    }
}

fn qword(key: HKEY, sub: &str, value: PCWSTR) -> Option<u64> {
    let mut v: u64 = 0;
    let mut size = 8u32;
    let r = unsafe {
        RegGetValueW(
            key,
            &HSTRING::from(sub),
            value,
            RRF_RT_REG_QWORD,
            None,
            Some(&mut v as *mut u64 as *mut c_void),
            Some(&mut size),
        )
    };
    r.is_ok().then_some(v)
}

fn pretty(sub: &str, nonpackaged: bool) -> String {
    if nonpackaged {
        let file = sub.rsplit('#').next().unwrap_or(sub);
        std::path::Path::new(file)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string())
    } else {
        let family = sub.split('_').next().unwrap_or(sub);
        family.rsplit('.').next().unwrap_or(family).to_string()
    }
}

// ---------------------------------------------------------------------
// network throughput
// ---------------------------------------------------------------------

#[derive(Default)]
pub struct NetSampler {
    rx: u64,
    tx: u64,
    at: Option<Instant>,
}

impl NetSampler {
    /// (download, upload) in bytes per second since the previous call.
    pub fn sample(&mut self) -> (f64, f64) {
        let (rx, tx) = net_totals();
        let now = Instant::now();
        let rate = match self.at {
            Some(t) => {
                let dt = now.duration_since(t).as_secs_f64().max(0.001);
                (
                    rx.saturating_sub(self.rx) as f64 / dt,
                    tx.saturating_sub(self.tx) as f64 / dt,
                )
            }
            None => (0.0, 0.0),
        };
        self.rx = rx;
        self.tx = tx;
        self.at = Some(now);
        rate
    }
}

fn net_totals() -> (u64, u64) {
    unsafe {
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut t).is_err() || t.is_null() {
            return (0, 0);
        }
        let tbl = &*t;
        let rows = std::slice::from_raw_parts(tbl.Table.as_ptr(), tbl.NumEntries as usize);
        let (mut rx, mut tx) = (0u64, 0u64);
        for r in rows {
            let flags = r.InterfaceAndOperStatusFlags._bitfield;
            let hardware = flags & 1 != 0;
            let filter = flags & 2 != 0;
            // 24 = software loopback, OperStatus 1 = up
            if hardware && !filter && r.Type != 24 && r.OperStatus.0 == 1 {
                rx += r.InOctets;
                tx += r.OutOctets;
            }
        }
        FreeMibTable(t as *const c_void);
        (rx, tx)
    }
}

// ---------------------------------------------------------------------
// NVIDIA GPU via NVML (loaded dynamically; absent on non-NVIDIA PCs)
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Gpu {
    pub name: String,
    pub util: f32,
    pub temp: u32,
    pub mem_used: u64,
    pub mem_total: u64,
}

#[repr(C)]
struct NvUtil {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
struct NvMem {
    total: u64,
    free: u64,
    used: u64,
}

type NvInit = unsafe extern "C" fn() -> i32;
type NvHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NvGetUtil = unsafe extern "C" fn(*mut c_void, *mut NvUtil) -> i32;
type NvGetTemp = unsafe extern "C" fn(*mut c_void, i32, *mut u32) -> i32;
type NvGetMem = unsafe extern "C" fn(*mut c_void, *mut NvMem) -> i32;
type NvGetName = unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i32;

pub struct Nvml {
    util: NvGetUtil,
    temp: NvGetTemp,
    mem: NvGetMem,
    dev: *mut c_void,
    name: String,
}

impl Nvml {
    pub fn load() -> Option<Nvml> {
        unsafe {
            let lib: HMODULE = LoadLibraryW(w!("nvml.dll"))
                .or_else(|_| LoadLibraryW(w!("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll")))
                .ok()?;
            macro_rules! sym {
                ($name:expr, $t:ty) => {
                    std::mem::transmute::<_, $t>(GetProcAddress(lib, $name)?)
                };
            }
            let init = sym!(s!("nvmlInit_v2"), NvInit);
            if init() != 0 {
                return None;
            }
            let handle = sym!(s!("nvmlDeviceGetHandleByIndex_v2"), NvHandle);
            let mut dev = std::ptr::null_mut();
            if handle(0, &mut dev) != 0 {
                return None;
            }
            let get_name = sym!(s!("nvmlDeviceGetName"), NvGetName);
            let mut buf = [0u8; 96];
            get_name(dev, buf.as_mut_ptr(), buf.len() as u32);
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end])
                .replace("NVIDIA ", "")
                .replace("GeForce ", "");
            Some(Nvml {
                util: sym!(s!("nvmlDeviceGetUtilizationRates"), NvGetUtil),
                temp: sym!(s!("nvmlDeviceGetTemperature"), NvGetTemp),
                mem: sym!(s!("nvmlDeviceGetMemoryInfo"), NvGetMem),
                dev,
                name,
            })
        }
    }

    pub fn sample(&self) -> Option<Gpu> {
        unsafe {
            let mut u = NvUtil { gpu: 0, memory: 0 };
            let mut t = 0u32;
            let mut m = NvMem { total: 0, free: 0, used: 0 };
            if (self.util)(self.dev, &mut u) != 0 {
                return None;
            }
            (self.temp)(self.dev, 0, &mut t);
            (self.mem)(self.dev, &mut m);
            Some(Gpu {
                name: self.name.clone(),
                util: u.gpu as f32 / 100.0,
                temp: t,
                mem_used: m.used,
                mem_total: m.total,
            })
        }
    }
}

// ---------------------------------------------------------------------
// audio endpoints
// ---------------------------------------------------------------------

pub struct Audio {
    en: IMMDeviceEnumerator,
}

fn device_id(d: &IMMDevice) -> Result<String> {
    unsafe {
        let p = d.GetId()?;
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        Ok(s)
    }
}

fn device_name(d: &IMMDevice) -> Result<String> {
    unsafe {
        let store = d.OpenPropertyStore(STGM_READ)?;
        let v = store.GetValue(&PKEY_Device_FriendlyName)?;
        Ok(v.to_string())
    }
}

/// Undocumented but long-stable IPolicyConfig (Windows 7+) used by every
/// audio switcher to change the default endpoint.
const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

impl Audio {
    pub fn new() -> Result<Audio> {
        Ok(Audio {
            en: unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? },
        })
    }

    pub fn render_volume(&self) -> Result<IAudioEndpointVolume> {
        unsafe { self.en.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None) }
    }

    pub fn capture_volume(&self) -> Result<IAudioEndpointVolume> {
        unsafe { self.en.GetDefaultAudioEndpoint(eCapture, eConsole)?.Activate(CLSCTX_ALL, None) }
    }

    /// Active output devices as (id, name), plus the default device id.
    pub fn outputs(&self) -> Result<(Vec<(String, String)>, String)> {
        unsafe {
            let col = self.en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut list = Vec::new();
            for i in 0..col.GetCount()? {
                let d = col.Item(i)?;
                list.push((device_id(&d)?, device_name(&d).unwrap_or_else(|_| "Audio".into())));
            }
            let def = self
                .en
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .and_then(|d| device_id(&d))
                .unwrap_or_default();
            Ok((list, def))
        }
    }

    pub fn set_default_output(&self, id: &str) -> Result<()> {
        unsafe {
            let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;
            let mut p: *mut c_void = std::ptr::null_mut();
            unk.query(&IID_POLICY_CONFIG, &mut p).ok()?;
            let vtbl = *(p as *const *const usize);
            type SetDefault = unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT;
            type Release = unsafe extern "system" fn(*mut c_void) -> u32;
            let set: SetDefault = std::mem::transmute(*vtbl.add(13));
            let release: Release = std::mem::transmute(*vtbl.add(2));
            let wid = HSTRING::from(id);
            let mut res = Ok(());
            for role in [eConsole, eMultimedia, eCommunications] {
                if let Err(e) = set(p, PCWSTR(wid.as_ptr()), role.0).ok() {
                    res = Err(e);
                }
            }
            release(p);
            res
        }
    }
}

// ---------------------------------------------------------------------
// ping (ICMP echo)
// ---------------------------------------------------------------------

/// Round-trip time in ms to `host` (IPv4 or hostname), None on timeout.
pub fn ping(host: &str, timeout_ms: u32) -> Option<u32> {
    use std::net::{IpAddr, ToSocketAddrs};
    let addr = format!("{}:0", host.trim())
        .to_socket_addrs()
        .ok()?
        .find_map(|a| match a.ip() {
            IpAddr::V4(v4) => Some(v4),
            _ => None,
        })?;
    unsafe {
        let h = IcmpCreateFile().ok()?;
        let data = [0x49u8; 32];
        let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 16];
        let n = IcmpSendEcho(
            h,
            u32::from_le_bytes(addr.octets()), // IPAddr is in network byte order
            data.as_ptr() as *const c_void,
            data.len() as u16,
            None,
            reply.as_mut_ptr() as *mut c_void,
            reply.len() as u32,
            timeout_ms,
        );
        let _ = IcmpCloseHandle(h);
        if n == 0 {
            return None;
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        (r.Status == 0).then_some(r.RoundTripTime)
    }
}

// ---------------------------------------------------------------------
// Bluetooth battery (the value Windows shows in Settings > Bluetooth)
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct BtDevice {
    pub name: String,
    pub battery: u8,
}

const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5),
    pid: 2,
};

/// Connected devices that report a battery level, deduplicated by name.
pub fn bluetooth_batteries() -> Vec<BtDevice> {
    let mut out: Vec<BtDevice> = Vec::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_ALLCLASSES) else {
            return out;
        };
        let mut i = 0;
        loop {
            let mut d = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, i, &mut d).is_err() {
                break;
            }
            i += 1;
            let mut ty = DEVPROPTYPE::default();
            let mut val = [0u8; 1];
            if SetupDiGetDevicePropertyW(set, &d, &DEVPKEY_BLUETOOTH_BATTERY, &mut ty, Some(&mut val), None, 0).is_err() {
                continue;
            }
            let mut buf = [0u8; 512];
            let mut name = String::new();
            if SetupDiGetDevicePropertyW(set, &d, &DEVPKEY_Device_FriendlyName, &mut ty, Some(&mut buf), None, 0).is_ok() {
                let wide: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
                name = String::from_utf16_lossy(&wide[..end]);
            }
            for suffix in [" Hands-Free AG", " Hands-Free", " Avrcp Transport", " Stereo", " Audio"] {
                if let Some(n) = name.strip_suffix(suffix) {
                    name = n.to_string();
                }
            }
            if name.is_empty() || val[0] > 100 || out.iter().any(|b| b.name == name) {
                continue;
            }
            out.push(BtDevice { name, battery: val[0] });
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// Short human byte rate, e.g. "1.2 MB/s".
pub fn fmt_rate(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.1} MB/s", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} KB/s", bps / 1e3)
    } else {
        format!("{:.0} B/s", bps)
    }
}
