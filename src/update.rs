//! Update check and one-click install from GitHub Releases.
//!
//! The check uses the `repository` URL from Cargo.toml (compiled in) and
//! does nothing until that is set. Installing downloads the release's
//! `isla.exe`, verifies its SHA-256 against the digest GitHub publishes,
//! swaps it with the running exe (a running exe can be renamed on Windows)
//! and starts the new one, which waits for this instance to exit.

use crate::net;
use crate::worker::Shared;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Security::Cryptography::{
    BCryptCloseAlgorithmProvider, BCryptHash, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE,
    BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS, BCRYPT_SHA256_ALGORITHM,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Posted to the notch window once the new exe is in place and started.
pub const WM_UPDATE_READY: u32 = WM_APP + 2;

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub url: String,
    /// Download URL of the `isla.exe` asset.
    pub asset: Option<String>,
    /// Lowercase hex SHA-256 of that asset, when GitHub provides it.
    pub sha256: Option<String>,
}

#[derive(Clone, Debug)]
pub enum InstallState {
    Downloading,
    Failed(String),
}

fn version_tuple(v: &str) -> Vec<u64> {
    v.trim_start_matches(['v', 'V'])
        .split(['.', '-'])
        .map_while(|p| p.parse().ok())
        .collect()
}

fn repo() -> Option<String> {
    REPOSITORY
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")
        .map(str::to_string)
}

fn latest(repo: &str) -> Option<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let v: Value = serde_json::from_str(&net::get(&url)?).ok()?;
    let tag = v["tag_name"].as_str()?.to_string();
    let asset = v["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some("isla.exe")));
    Some(Release {
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        asset: asset.and_then(|a| a["browser_download_url"].as_str()).map(str::to_string),
        sha256: asset
            .and_then(|a| a["digest"].as_str())
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(str::to_lowercase),
        tag,
    })
}

pub fn spawn(shared: Arc<Mutex<Shared>>) {
    let Some(repo) = repo() else { return };
    std::thread::Builder::new()
        .name("isla-update".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            loop {
                if let Some(r) = latest(&repo) {
                    if version_tuple(&r.tag) > version_tuple(VERSION) {
                        shared.lock().unwrap().update = Some(r);
                    }
                }
                std::thread::sleep(Duration::from_secs(12 * 3600));
            }
        })
        .expect("spawn update");
}

fn sha256_hex(data: &[u8]) -> Option<String> {
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_SHA256_ALGORITHM, None, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0))
            .ok()
            .ok()?;
        let mut out = [0u8; 32];
        let st = BCryptHash(alg, None, data, &mut out);
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        st.ok().ok()?;
        Some(out.iter().map(|b| format!("{b:02x}")).collect())
    }
}

fn sibling(ext: &str) -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.with_extension(ext))
}

/// Removes the previous exe left behind by an update (retries while the
/// old process is still exiting).
pub fn cleanup_old() {
    let Some(old) = sibling("exe.old") else { return };
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..20 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Swaps the running exe for `data` and starts the new one with
/// `--updated` (it waits for this instance to exit). Rolls back on failure.
pub fn apply(data: &[u8]) -> Result<(), String> {
    if data.len() < 200_000 || !data.starts_with(b"MZ") {
        return Err("File unduhan tidak valid".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = exe.with_extension("exe.new");
    let old = exe.with_extension("exe.old");
    let denied = |_| "Folder Isla tidak bisa ditulisi".to_string();
    std::fs::write(&new, data).map_err(denied)?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(denied)?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // roll back
        return Err(e.to_string());
    }
    std::process::Command::new(&exe)
        .arg("--updated")
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Downloads, verifies and installs `rel` on a background thread. On
/// success the new exe is started and `WM_UPDATE_READY` is posted to `hwnd`
/// (a raw handle value) so this instance can close.
pub fn install(rel: Release, hwnd: isize, state: Arc<Mutex<Option<InstallState>>>) {
    *state.lock().unwrap() = Some(InstallState::Downloading);
    std::thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let url = rel.asset.as_deref().ok_or("Rilis ini tidak berisi isla.exe")?;
            let data = net::get_bytes(url).ok_or("Gagal mengunduh pembaruan")?;
            if let Some(want) = &rel.sha256 {
                if sha256_hex(&data).as_deref() != Some(want.as_str()) {
                    return Err("Checksum unduhan tidak cocok".into());
                }
            }
            apply(&data)
        })();
        match result {
            Ok(()) => unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as _)), WM_UPDATE_READY, WPARAM(0), LPARAM(0));
            },
            Err(e) => *state.lock().unwrap() = Some(InstallState::Failed(e)),
        }
    });
}
