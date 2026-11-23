#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Isla: a Dynamic Island style notch overlay for Windows.
//! Hover to expand; scroll for volume; drop files on it; right-click for the menu.

mod anim;
mod clip;
mod config;
mod draw;
mod lyrics;
mod net;
mod notify;
mod sensors;
mod settings;
mod shelf;
mod system;
mod text;
mod tray;
mod ui;
mod update;
mod views;
mod weather;
mod worker;

use std::cell::RefCell;
use std::sync::mpsc;
use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::System::SystemServices::MK_LBUTTON;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    static APP: RefCell<Option<ui::App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app unless it is already borrowed (re-entrant message
/// dispatch, e.g. while a popup menu or drag runs its modal loop).
pub(crate) fn with_app<R>(f: impl FnOnce(&mut ui::App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

const MENU_SETTINGS: usize = 1;
const MENU_AUTOSTART: usize = 2;
const MENU_EXIT: usize = 3;
const MENU_UPDATE: usize = 4;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Dev aid: export the app icon (assets/isla.ico + isla.png) and exit.
    if let Some(i) = args.iter().position(|a| a == "--write-icon") {
        let dir = args.get(i + 1).cloned().unwrap_or_else(|| "assets".into());
        return tray::write_icon_files(&dir).map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()));
    }
    // Dev aid: run the update swap with a local exe instead of a download.
    if let Some(i) = args.iter().position(|a| a == "--apply-update") {
        let data = args.get(i + 1).and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        return update::apply(&data).map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e));
    }
    system::migrate_legacy();
    let snapshot = args
        .iter()
        .position(|a| a == "--snapshot")
        .map(|i| args.get(i + 1).cloned().unwrap_or_else(|| "snapshots".into()));
    let updated = args.iter().any(|a| a == "--updated");
    update::cleanup_old();
    unsafe {
        // Single instance (snapshot runs alongside the live notch). Right
        // after an update the old instance may still be closing: wait for it.
        let mut tries = if updated { 50 } else { 1 };
        let _mutex = loop {
            let m = CreateMutexW(None, true, w!("Local\\IslaSingleInstance"))?;
            if GetLastError() != ERROR_ALREADY_EXISTS || snapshot.is_some() {
                break m;
            }
            let _ = CloseHandle(m);
            tries -= 1;
            if tries == 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // STA + OLE for drag & drop and shell thumbnails.
        OleInitialize(None)?;

        let hinst = GetModuleHandleW(None)?;
        let class = w!("IslaWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("Isla"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;

        // Config first: the worker threads read it from their first poll.
        let cfg_load = config::load();
        let (tx, rx) = mpsc::channel();
        let shared = worker::spawn(rx);
        APP.with(|a| *a.borrow_mut() = Some(ui::App::new(hwnd, shared, tx, cfg_load)));
        if let Some(dir) = snapshot {
            // Give the worker time to gather media, sensors, weather and notifications.
            std::thread::sleep(std::time::Duration::from_secs(6));
            with_app(|app| {
                app.relayout_hidden();
                app.snapshot(&dir);
            });
            return Ok(());
        }
        with_app(|app| {
            app.relayout();
            if updated {
                app.show_updated();
            }
        });
        shelf::register(hwnd);
        tray::add(hwnd);
        clip::listen(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, 16, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    let x = (lp.0 & 0xffff) as u16 as i16 as i32;
    let y = ((lp.0 >> 16) & 0xffff) as u16 as i16 as i32;
    (x, y)
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                with_app(|a| a.tick());
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE => {
                let (x, y) = lparam_point(lp);
                let lbutton = wp.0 & MK_LBUTTON.0 as usize != 0;
                // Start the shell drag outside the app borrow: it runs a
                // modal loop that keeps dispatching our messages.
                if let Some(Some(paths)) = with_app(|a| a.mouse_move(x, y, lbutton)) {
                    shelf::drag_out(hwnd, &paths);
                    with_app(|a| a.after_drag_out());
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_point(lp);
                with_app(|a| a.mouse_down(x, y));
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_point(lp);
                with_app(|a| a.mouse_up(x, y));
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xffff) as u16 as i16;
                let (x, y) = lparam_point(lp); // screen coordinates
                with_app(|a| a.wheel(x, y, delta));
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let (x, y) = lparam_point(lp);
                if !with_app(|a| a.right_click(x, y)).unwrap_or(false) {
                    show_menu(hwnd);
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let hand = with_app(|a| a.hovering_button()).unwrap_or(false);
                let cur = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }).ok();
                SetCursor(cur);
                LRESULT(1)
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
                with_app(|a| a.relayout());
                LRESULT(0)
            }
            WM_CLIPBOARDUPDATE => {
                with_app(|a| a.on_clipboard());
                LRESULT(0)
            }
            tray::WM_TRAY => {
                match (lp.0 & 0xffff) as u32 {
                    WM_LBUTTONUP => {
                        with_app(|a| a.open_settings());
                    }
                    WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            update::WM_UPDATE_READY => {
                // The new exe is in place and waiting for this one to exit.
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                tray::remove(hwnd);
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m == tray::taskbar_created() => {
                tray::add(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let autostart = system::autostart_enabled();
    let check = if autostart { MF_CHECKED } else { MF_UNCHECKED };
    if let Some(Some(tag)) = with_app(|a| a.update_tag()) {
        let label = HSTRING::from(format!("Perbarui ke {tag}"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_UPDATE, PCWSTR(label.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    }
    let _ = AppendMenuW(menu, MF_STRING, MENU_SETTINGS, w!("Pengaturan…"));
    let _ = AppendMenuW(menu, MF_STRING | check, MENU_AUTOSTART, w!("Jalankan saat Windows mulai"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("Keluar"));

    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_TOPALIGN | TPM_LEFTALIGN,
        p.x,
        p.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        MENU_SETTINGS => {
            with_app(|a| a.open_settings());
        }
        MENU_AUTOSTART => system::set_autostart(!autostart),
        MENU_UPDATE => {
            with_app(|a| a.install_update());
        }
        MENU_EXIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}
