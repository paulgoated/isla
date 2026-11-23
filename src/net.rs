//! Minimal HTTPS GET over WinHTTP (no extra dependencies). WinRT's
//! HttpClient goes through WinINet, which fails the TLS handshake on some
//! setups with 0x80072F8F, so everything network-bound uses this.

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

/// GET `https://host/path?query` as text; the body on HTTP 200.
pub fn get(url: &str) -> Option<String> {
    String::from_utf8(get_bytes(url)?).ok()
}

/// GET returning raw bytes (redirects are followed, e.g. GitHub release
/// assets). None unless the final status is 200.
pub fn get_bytes(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    unsafe {
        // The agent string doubles as the User-Agent header (GitHub requires one).
        let session = WinHttpOpen(w!("Isla/0.1"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return None;
        }
        let _ = WinHttpSetTimeouts(session, 8000, 8000, 15000, 15000);
        let conn = WinHttpConnect(session, &HSTRING::from(host), INTERNET_DEFAULT_HTTPS_PORT, 0);
        let req = if conn.is_null() {
            std::ptr::null_mut()
        } else {
            WinHttpOpenRequest(
                conn,
                w!("GET"),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        };
        let mut body = Vec::new();
        let mut ok = !req.is_null()
            && WinHttpSendRequest(req, None, None, 0, 0, 0).is_ok()
            && WinHttpReceiveResponse(req, std::ptr::null_mut()).is_ok();
        if ok {
            let mut status = 0u32;
            let mut len = 4u32;
            let _ = WinHttpQueryHeaders(
                req,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut _),
                &mut len,
                std::ptr::null_mut(),
            );
            ok = status == 200;
        }
        if ok {
            loop {
                let mut avail = 0u32;
                if WinHttpQueryDataAvailable(req, &mut avail).is_err() || avail == 0 {
                    break;
                }
                let mut buf = vec![0u8; avail as usize];
                let mut read = 0u32;
                if WinHttpReadData(req, buf.as_mut_ptr() as *mut _, avail, &mut read).is_err() || read == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..read as usize]);
            }
        }
        for h in [req, conn, session] {
            if !h.is_null() {
                let _ = WinHttpCloseHandle(h);
            }
        }
        if !ok || body.is_empty() {
            return None;
        }
        Some(body)
    }
}

/// Percent-encodes a query parameter value.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
