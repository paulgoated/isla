//! File shelf: drop files on the notch to park them, drag them back out
//! to copy/move them, click to open. Also shell icon / thumbnail helpers.

use crate::with_app;
use std::path::Path;
use tiny_skia::{IntSize, Pixmap, PixmapPaint, Transform};
use windows::core::{implement, Ref, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, POINTL, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub struct Item {
    pub path: String,
    pub name: String,
    pub icon: Option<Pixmap>,
}

#[derive(Default)]
pub struct Shelf {
    pub items: Vec<Item>,
}

const MAX_ITEMS: usize = 9;

fn store() -> std::path::PathBuf {
    crate::config::dir().join("shelf.txt")
}

impl Shelf {
    pub fn load() -> Shelf {
        let mut s = Shelf::default();
        if let Ok(text) = std::fs::read_to_string(store()) {
            let paths: Vec<String> = text.lines().map(str::to_string).collect();
            s.add(paths);
        }
        s
    }

    fn save(&self) {
        let text: Vec<&str> = self.items.iter().map(|i| i.path.as_str()).collect();
        let _ = std::fs::create_dir_all(crate::config::dir());
        let _ = std::fs::write(store(), text.join("\n"));
    }

    pub fn add(&mut self, paths: Vec<String>) {
        for p in paths {
            if p.trim().is_empty() || !Path::new(&p).exists() || self.items.iter().any(|i| i.path == p) {
                continue;
            }
            let name = Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone());
            let icon = shell_image(&p, 96, false);
            self.items.push(Item { path: p, name, icon });
        }
        if self.items.len() > MAX_ITEMS {
            let extra = self.items.len() - MAX_ITEMS;
            self.items.drain(..extra);
        }
        self.save();
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            self.items.remove(i);
            self.save();
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.save();
    }

    /// Drops entries whose file was moved or deleted.
    pub fn prune(&mut self) {
        let before = self.items.len();
        self.items.retain(|i| Path::new(&i.path).exists());
        if self.items.len() != before {
            self.save();
        }
    }
}

pub fn open(path: &str, args: &str) {
    unsafe {
        let args = HSTRING::from(args);
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &HSTRING::from(path),
            if args.is_empty() { PCWSTR::null() } else { PCWSTR(args.as_ptr()) },
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// ---------------------------------------------------------------------
// shell images
// ---------------------------------------------------------------------

/// Icon (or thumbnail when `icon_only` is false) for a path, as a square
/// premultiplied RGBA pixmap of `px` pixels.
pub fn shell_image(path: &str, px: i32, icon_only: bool) -> Option<Pixmap> {
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()?;
        let mut flags = SIIGBF_RESIZETOFIT;
        if icon_only {
            flags |= SIIGBF_ICONONLY;
        }
        let hbmp = factory.GetImage(SIZE { cx: px, cy: px }, flags).ok()?;
        let img = hbitmap_to_pixmap(hbmp);
        let _ = DeleteObject(hbmp.into());
        let img = img?;
        // Center non-square thumbnails in a square canvas.
        let mut sq = Pixmap::new(px as u32, px as u32)?;
        let x = (px - img.width() as i32) / 2;
        let y = (px - img.height() as i32) / 2;
        sq.draw_pixmap(x, y, img.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        Some(sq)
    }
}

unsafe fn hbitmap_to_pixmap(hbmp: HBITMAP) -> Option<Pixmap> {
    let mut bm = BITMAP::default();
    if GetObjectW(hbmp.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let mut bi = BITMAPINFO {
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
    let mut data = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(dc, hbmp, 0, h as u32, Some(data.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 {
        return None;
    }
    let has_alpha = data.chunks_exact(4).any(|p| p[3] != 0);
    // Some sources hand out straight alpha; premultiply only if a color
    // channel exceeds alpha (impossible for premultiplied data).
    let straight = has_alpha && data.chunks_exact(4).any(|p| p[0].max(p[1]).max(p[2]) > p[3]);
    for p in data.chunks_exact_mut(4) {
        p.swap(0, 2);
        if !has_alpha {
            p[3] = 255;
        } else if straight {
            let a = p[3] as u16;
            for c in &mut p[..3] {
                *c = ((*c as u16 * a + 127) / 255) as u8;
            }
        }
    }
    Pixmap::from_vec(data, IntSize::from_wh(w as u32, h as u32)?)
}

// ---------------------------------------------------------------------
// drag & drop
// ---------------------------------------------------------------------

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn has_files(obj: &IDataObject) -> bool {
    unsafe { obj.QueryGetData(&hdrop_format()).is_ok() }
}

fn files_of(obj: &IDataObject) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let Ok(mut medium) = obj.GetData(&hdrop_format()) else { return out };
        let hdrop = HDROP(medium.u.hGlobal.0);
        let n = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..n {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            out.push(String::from_utf16_lossy(&buf[..len]));
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

#[implement(IDropTarget)]
struct DropTarget;

fn accept(effect: *mut DROPEFFECT, ok: bool) {
    if !effect.is_null() {
        unsafe { *effect = if ok { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        obj: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let files = obj.as_ref().is_some_and(has_files);
        let ok = with_app(|a| a.file_drag_enter(files)).unwrap_or(false);
        accept(effect, ok);
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, _pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let ok = with_app(|a| a.file_drag_active()).unwrap_or(false);
        accept(effect, ok);
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        with_app(|a| a.file_drag_leave());
        Ok(())
    }

    fn Drop(
        &self,
        obj: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let files = obj.as_ref().map(files_of).unwrap_or_default();
        let ok = with_app(|a| a.file_drop(files)).unwrap_or(false);
        accept(effect, ok);
        Ok(())
    }
}

pub fn register(hwnd: HWND) {
    unsafe {
        let target: IDropTarget = DropTarget.into();
        let _ = RegisterDragDrop(hwnd, &target);
    }
}

/// Starts a shell drag of the given files (copy / move / link). Blocks
/// until the drop finishes; must not run while the app is borrowed.
pub fn drag_out(hwnd: HWND, paths: &[String]) {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None).is_ok() {
                pidls.push(pidl);
            }
        }
        if !pidls.is_empty() {
            let raw: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const _).collect();
            if let Ok(arr) = SHCreateShellItemArrayFromIDLists(&raw) {
                if let Ok(obj) = arr.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) {
                    let _ = SHDoDragDrop(
                        Some(hwnd),
                        &obj,
                        None,
                        DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
                    );
                }
            }
        }
        for p in pidls {
            CoTaskMemFree(Some(p as *const _));
        }
    }
}
