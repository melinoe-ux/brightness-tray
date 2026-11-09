//! Brightness Tray — an ultra-light tray app that controls external monitor brightness via DDC/CI.
//! Left click: slider popup. Right click: menu. Never shows in the taskbar or Alt+Tab.

#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitor, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_READ_DONE: u32 = WM_APP + 2;
const ID_AUTOSTART: usize = 1;
const ID_REFRESH: usize = 2;
const ID_EXIT: usize = 3;
const TIMER_REENUM: usize = 1;
const TIMER_TRAY_RETRY: usize = 2;
const VCP_BRIGHTNESS: u8 = 0x10;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_NAME: &str = "BrightnessTray";
const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";

// Layout (96-DPI units)
const WIDTH: i32 = 320;
const PAD: i32 = 16;
const ROW: i32 = 56;
const NAME_H: i32 = 20;
const BAR_Y: i32 = 38;
const KNOB_R: i32 = 10;
const PCT_W: i32 = 56;

static DPI: AtomicU32 = AtomicU32::new(96);
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

fn s(v: i32) -> i32 {
    v * DPI.load(Ordering::Relaxed) as i32 / 96
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Pending,
    Ok,
    Unsupported,
}

struct Mon {
    handle: usize,
    name: Vec<u16>,
    pct: u32,
    max: u32,
    state: State,
}

#[derive(Clone, Copy)]
struct Theme {
    bg: u32,
    text: u32,
    track: u32,
    accent: u32,
    knob: u32,
}

enum Cmd {
    Set(usize, u32),
    Read(Vec<usize>),
    Destroy(Vec<usize>),
}

struct App {
    mons: Vec<Mon>,
    tx: Sender<Cmd>,
    results: Arc<Mutex<Vec<(usize, u32, u32)>>>,
    drag: Option<usize>,
    last_hide: u32,
    font: HFONT,
    font_bold: HFONT,
    icon: HICON,
    light_taskbar: bool,
    theme: Theme,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(a.borrow_mut().as_mut().expect("app")))
}

// ---------------------------------------------------------------- DDC/CI worker

/// DDC/CI calls are slow (~50ms), so they run on a separate thread to keep the UI responsive.
/// Requests queued while dragging are coalesced; only the latest value is sent.
fn worker(rx: Receiver<Cmd>, results: Arc<Mutex<Vec<(usize, u32, u32)>>>, hwnd: usize) {
    while let Ok(first) = rx.recv() {
        let mut sets: Vec<(usize, u32)> = Vec::new();
        let mut reads: Vec<usize> = Vec::new();
        let mut destroys: Vec<usize> = Vec::new();
        let mut take = |c: Cmd| match c {
            Cmd::Set(h, v) => {
                if let Some(e) = sets.iter_mut().find(|e| e.0 == h) {
                    e.1 = v;
                } else {
                    sets.push((h, v));
                }
            }
            Cmd::Read(hs) => reads = hs,
            Cmd::Destroy(hs) => destroys.extend(hs),
        };
        take(first);
        while let Ok(c) = rx.try_recv() {
            take(c);
        }
        unsafe {
            for (h, v) in sets {
                if SetVCPFeature(h as HANDLE, VCP_BRIGHTNESS, v) == 0 {
                    SetVCPFeature(h as HANDLE, VCP_BRIGHTNESS, v);
                }
            }
            if !reads.is_empty() {
                let mut out = Vec::with_capacity(reads.len());
                for h in reads {
                    let (mut cur, mut max) = (0u32, 0u32);
                    let mut ok = false;
                    for _ in 0..3 {
                        if GetVCPFeatureAndVCPFeatureReply(
                            h as HANDLE,
                            VCP_BRIGHTNESS,
                            null_mut(),
                            &mut cur,
                            &mut max,
                        ) != 0
                            && max > 0
                        {
                            ok = true;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(40));
                    }
                    out.push((h, cur, if ok { max } else { 0 }));
                }
                results.lock().unwrap().extend(out);
                PostMessageW(hwnd as HWND, WM_READ_DONE, 0, 0);
            }
            for h in destroys {
                DestroyPhysicalMonitor(h as HANDLE);
            }
        }
    }
}

// ---------------------------------------------------------------- Monitors

unsafe extern "system" fn enum_proc(hm: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
    (*(data as *mut Vec<HMONITOR>)).push(hm);
    1
}

fn enumerate() -> Vec<Mon> {
    let mut out = Vec::new();
    unsafe {
        let mut hms: Vec<HMONITOR> = Vec::new();
        EnumDisplayMonitors(null_mut(), null(), Some(enum_proc), &mut hms as *mut _ as LPARAM);
        for hm in hms {
            let mut n = 0u32;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(hm, &mut n) == 0 || n == 0 {
                continue;
            }
            let mut arr: Vec<PHYSICAL_MONITOR> = (0..n).map(|_| zeroed()).collect();
            if GetPhysicalMonitorsFromHMONITOR(hm, n, arr.as_mut_ptr()) == 0 {
                continue;
            }
            for pm in arr {
                let desc = { pm.szPhysicalMonitorDescription }; // packed struct: copy out
                let len = desc.iter().position(|&c| c == 0).unwrap_or(desc.len());
                let d = String::from_utf16_lossy(&desc[..len]);
                let name = if d.trim().is_empty() || d.contains("Generic") {
                    format!("Monitor {}", out.len() + 1)
                } else {
                    d.trim().to_string()
                };
                out.push(Mon {
                    handle: pm.hPhysicalMonitor as usize,
                    name: name.encode_utf16().collect(),
                    pct: 50,
                    max: 100,
                    state: State::Pending,
                });
            }
        }
    }
    out
}

fn request_read() {
    app(|a| {
        let hs: Vec<usize> = a.mons.iter().map(|m| m.handle).collect();
        if !hs.is_empty() {
            let _ = a.tx.send(Cmd::Read(hs));
        }
    });
}

fn reenumerate(hwnd: HWND) {
    let new = enumerate();
    app(|a| {
        let old = std::mem::replace(&mut a.mons, new);
        a.drag = None;
        let _ = a.tx.send(Cmd::Destroy(old.into_iter().map(|m| m.handle).collect()));
    });
    request_read();
    resize_if_visible(hwnd);
}

fn rows(a: &App) -> Vec<usize> {
    (0..a.mons.len()).filter(|&i| a.mons[i].state != State::Unsupported).collect()
}

fn popup_size() -> (i32, i32) {
    let n = app(|a| rows(a).len()) as i32;
    let h = if n == 0 { s(64) } else { s(PAD) * 2 + (n - 1) * s(ROW) + s(BAR_Y + KNOB_R) };
    (s(WIDTH), h)
}

// ---------------------------------------------------------------- Theme & icon

fn reg_dword(key: &str, val: &str) -> Option<u32> {
    let (k, v) = (w(key), w(val));
    let mut data = 0u32;
    let mut sz = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut data as *mut _ as *mut c_void,
            &mut sz,
        )
    };
    (r == 0).then_some(data)
}

fn make_theme() -> Theme {
    if reg_dword(THEME_KEY, "AppsUseLightTheme") == Some(1) {
        Theme { bg: 0xF3F3F3, text: 0x1A1A1A, track: 0xC4C4C4, accent: 0xB85F00, knob: 0xFFFFFF }
    } else {
        Theme { bg: 0x202020, text: 0xFFFFFF, track: 0x5A5A5A, accent: 0xFFC24C, knob: 0x454545 }
    }
}

/// Draws the sun tray icon at runtime (no embedded resource).
fn make_icon(dark_glyph: bool) -> HICON {
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        bmi.bmiHeader.biHeight = -size;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        let mut bits: *mut c_void = null_mut();
        let sdc = GetDC(null_mut());
        let color = CreateDIBSection(sdc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        ReleaseDC(null_mut(), sdc);
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        let sz = size as f32;
        let c = sz / 2.0;
        let rgb: u32 = if dark_glyph { 0 } else { 255 };
        let step = std::f32::consts::PI / 4.0;
        for y in 0..size {
            for x in 0..size {
                let mut cov = 0u32;
                for j in 0..4 {
                    for i in 0..4 {
                        let dx = x as f32 + (i as f32 + 0.5) / 4.0 - c;
                        let dy = y as f32 + (j as f32 + 0.5) / 4.0 - c;
                        let d = (dx * dx + dy * dy).sqrt();
                        let inside = if d <= 0.24 * sz {
                            true
                        } else if d >= 0.35 * sz && d <= 0.49 * sz {
                            let a = dy.atan2(dx);
                            let da = a - (a / step).round() * step;
                            d * da.sin().abs() <= 0.065 * sz
                        } else {
                            false
                        };
                        cov += inside as u32;
                    }
                }
                let al = cov * 255 / 16;
                let ch = rgb * al / 255;
                px[(y * size + x) as usize] = (al << 24) | (ch << 16) | (ch << 8) | ch;
            }
        }
        let mask_bytes = vec![0u8; (((size + 15) / 16) * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, mask_bytes.as_ptr() as *const c_void);
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask);
        DeleteObject(color);
        icon
    }
}

// ---------------------------------------------------------------- Tray

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { zeroed() };
    n.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

fn tip_text() -> Vec<u16> {
    let t = app(|a| {
        let p: Vec<String> = a
            .mons
            .iter()
            .filter(|m| m.state == State::Ok)
            .map(|m| format!("{}%", m.pct))
            .collect();
        if p.is_empty() { "Brightness".to_string() } else { format!("Brightness: {}", p.join(" · ")) }
    });
    t.encode_utf16().take(127).collect()
}

fn tray(hwnd: HWND, op: u32) -> bool {
    let mut n = nid(hwnd);
    n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = app(|a| a.icon);
    let t = tip_text();
    n.szTip[..t.len()].copy_from_slice(&t);
    unsafe { Shell_NotifyIconW(op, &n) != 0 }
}

fn add_tray(hwnd: HWND) {
    if !tray(hwnd, NIM_ADD) {
        // Explorer may not be ready yet at logon; retry.
        unsafe { SetTimer(hwnd, TIMER_TRAY_RETRY, 2000, None) };
    }
}

// ---------------------------------------------------------------- Autostart

fn autostart_enabled() -> bool {
    let (k, v) = (w(RUN_KEY), w(RUN_NAME));
    let mut sz = 0u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut sz) == 0
    }
}

fn set_autostart(on: bool) {
    let (k, v) = (w(RUN_KEY), w(RUN_NAME));
    unsafe {
        if on {
            let mut buf = [0u16; 1024];
            let len = GetModuleFileNameW(null_mut(), buf.as_mut_ptr(), buf.len() as u32) as usize;
            let path = String::from_utf16_lossy(&buf[..len]);
            let data = w(&format!("\"{}\"", path));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_SZ,
                data.as_ptr() as *const c_void,
                (data.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
}

// ---------------------------------------------------------------- Popup

fn show_popup(hwnd: HWND) {
    let (wd, ht) = popup_size();
    unsafe {
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        let hm = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(hm, &mut mi);
        let wa = mi.rcWork;
        let m = s(12);
        let x = (pt.x - wd / 2).min(wa.right - wd - m).max(wa.left + m);
        let mut y = pt.y - ht - m;
        if y < wa.top + m {
            y = pt.y + m; // taskbar at the top
        }
        let y = y.min(wa.bottom - ht - m).max(wa.top + m);
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, wd, ht, SWP_SHOWWINDOW);
        SetForegroundWindow(hwnd);
        InvalidateRect(hwnd, null(), 0);
    }
    request_read();
}

fn hide_popup(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
            let t = GetTickCount();
            app(|a| {
                a.last_hide = t;
                a.drag = None;
            });
        }
    }
}

fn resize_if_visible(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return;
        }
        let (wd, ht) = popup_size();
        let mut rc: RECT = zeroed();
        GetWindowRect(hwnd, &mut rc);
        if rc.bottom - rc.top != ht {
            SetWindowPos(hwnd, null_mut(), rc.left, rc.bottom - ht, wd, ht, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        InvalidateRect(hwnd, null(), 0);
    }
}

unsafe fn fill_round(dc: HDC, l: i32, t: i32, r: i32, b: i32, color: u32, rad: i32) {
    let br = CreateSolidBrush(color);
    let ob = SelectObject(dc, br);
    let op = SelectObject(dc, GetStockObject(NULL_PEN));
    RoundRect(dc, l, t, r + 1, b + 1, rad, rad);
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(br);
}

unsafe fn fill_circle(dc: HDC, cx: i32, cy: i32, r: i32, color: u32) {
    let br = CreateSolidBrush(color);
    let ob = SelectObject(dc, br);
    let op = SelectObject(dc, GetStockObject(NULL_PEN));
    Ellipse(dc, cx - r, cy - r, cx + r + 1, cy + r + 1);
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(br);
}

unsafe fn text(dc: HDC, s: &[u16], mut rc: RECT, fmt: u32) {
    DrawTextW(dc, s.as_ptr(), s.len() as i32, &mut rc, fmt | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = zeroed();
        GetClientRect(hwnd, &mut rc);
        let (wd, ht) = (rc.right, rc.bottom);
        let dc = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, wd, ht);
        let old = SelectObject(dc, bmp);
        app(|a| {
            let t = a.theme;
            let bg = CreateSolidBrush(t.bg);
            FillRect(dc, &rc, bg);
            DeleteObject(bg);
            SetBkMode(dc, TRANSPARENT as _);
            SetTextColor(dc, t.text);
            let r = rows(a);
            if r.is_empty() {
                SelectObject(dc, a.font);
                let msg: Vec<u16> = "No DDC/CI capable monitor found".encode_utf16().collect();
                text(dc, &msg, rc, DT_CENTER);
            }
            for (row, &mi) in r.iter().enumerate() {
                let m = &a.mons[mi];
                let y0 = s(PAD) + row as i32 * s(ROW);
                SelectObject(dc, a.font);
                text(
                    dc,
                    &m.name,
                    RECT { left: s(PAD), top: y0, right: wd - s(PAD) - s(PCT_W), bottom: y0 + s(NAME_H) },
                    DT_LEFT | DT_END_ELLIPSIS,
                );
                SelectObject(dc, a.font_bold);
                let pct: Vec<u16> = if m.state == State::Ok { format!("{}%", m.pct) } else { "…".into() }
                    .encode_utf16()
                    .collect();
                text(
                    dc,
                    &pct,
                    RECT { left: wd - s(PAD) - s(PCT_W), top: y0, right: wd - s(PAD), bottom: y0 + s(NAME_H) },
                    DT_RIGHT,
                );
                let (tl, tr) = (s(PAD) + s(KNOB_R), wd - s(PAD) - s(KNOB_R));
                let yc = y0 + s(BAR_Y);
                let fx = tl + (tr - tl) * m.pct as i32 / 100;
                fill_round(dc, tl, yc - s(2), tr, yc + s(2), t.track, s(4));
                if m.state == State::Ok {
                    fill_round(dc, tl, yc - s(2), fx, yc + s(2), t.accent, s(4));
                    fill_circle(dc, fx, yc, s(KNOB_R), t.knob);
                    let inner = if a.drag == Some(mi) { s(7) } else { s(5) };
                    fill_circle(dc, fx, yc, inner, t.accent);
                }
            }
        });
        BitBlt(hdc, 0, 0, wd, ht, dc, 0, 0, SRCCOPY);
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        EndPaint(hwnd, &ps);
    }
}

/// Maps a client y coordinate to a monitor index.
fn hit(y: i32) -> Option<usize> {
    app(|a| {
        let r = rows(a);
        if y < s(PAD) / 2 {
            return None;
        }
        let row = ((y - s(PAD) / 2) / s(ROW)) as usize;
        r.get(row).copied().filter(|&i| a.mons[i].state == State::Ok)
    })
}

fn set_pct(hwnd: HWND, mi: usize, pct: u32) {
    let changed = app(|a| {
        let m = &mut a.mons[mi];
        let pct = pct.min(100);
        if m.state != State::Ok || m.pct == pct {
            return false;
        }
        m.pct = pct;
        let _ = a.tx.send(Cmd::Set(m.handle, (pct * m.max + 50) / 100));
        true
    });
    if changed {
        unsafe { InvalidateRect(hwnd, null(), 0) };
        tray(hwnd, NIM_MODIFY);
    }
}

fn drag_to(hwnd: HWND, mi: usize, x: i32) {
    let (tl, tr) = (s(PAD) + s(KNOB_R), s(WIDTH) - s(PAD) - s(KNOB_R));
    let pct = ((x - tl) * 100 + (tr - tl) / 2) / (tr - tl);
    set_pct(hwnd, mi, pct.clamp(0, 100) as u32);
}

fn on_results(hwnd: HWND) {
    app(|a| {
        let res: Vec<_> = std::mem::take(&mut *a.results.lock().unwrap());
        for (h, cur, max) in res {
            if let Some(m) = a.mons.iter_mut().find(|m| m.handle == h) {
                if max > 0 {
                    if a.drag.is_none() {
                        m.max = max;
                        m.pct = ((cur * 100 + max / 2) / max).min(100);
                    }
                    m.state = State::Ok;
                } else if m.state == State::Pending {
                    m.state = State::Unsupported;
                }
            }
        }
    });
    resize_if_visible(hwnd);
    tray(hwnd, NIM_MODIFY);
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let auto = autostart_enabled();
        let menu = CreatePopupMenu();
        let (t1, t2, t3) = (w("Start with Windows"), w("Refresh monitors"), w("Exit"));
        AppendMenuW(menu, MF_STRING | if auto { MF_CHECKED } else { 0 }, ID_AUTOSTART, t1.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_REFRESH, t2.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, ID_EXIT, t3.as_ptr());
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd {
            ID_AUTOSTART => set_autostart(!auto),
            ID_REFRESH => reenumerate(hwnd),
            ID_EXIT => {
                let n = nid(hwnd);
                Shell_NotifyIconW(NIM_DELETE, &n);
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

fn refresh_theme(hwnd: HWND) {
    let light = reg_dword(THEME_KEY, "SystemUsesLightTheme") == Some(1);
    let theme = make_theme();
    let need_icon = app(|a| {
        a.theme = theme;
        a.light_taskbar != light
    });
    if need_icon {
        let icon = make_icon(light);
        let old = app(|a| {
            a.light_taskbar = light;
            std::mem::replace(&mut a.icon, icon)
        });
        tray(hwnd, NIM_MODIFY);
        unsafe { DestroyIcon(old) };
    }
    unsafe { InvalidateRect(hwnd, null(), 0) };
}

fn xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let tb = TASKBAR_MSG.load(Ordering::Relaxed);
    if tb != 0 && msg == tb {
        add_tray(hwnd); // Explorer restarted
        return 0;
    }
    match msg {
        WM_TRAY => {
            match (lp as u32) & 0xFFFF {
                WM_LBUTTONUP => {
                    if IsWindowVisible(hwnd) != 0 {
                        hide_popup(hwnd);
                    } else if GetTickCount().wrapping_sub(app(|a| a.last_hide)) > 250 {
                        show_popup(hwnd);
                    }
                }
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_READ_DONE => {
            on_results(hwnd);
            0
        }
        WM_ACTIVATE => {
            if (wp & 0xFFFF) as u32 == WA_INACTIVE {
                hide_popup(hwnd);
            }
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lp);
            if let Some(mi) = hit(y) {
                app(|a| a.drag = Some(mi));
                SetCapture(hwnd);
                drag_to(hwnd, mi, x);
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSEMOVE => {
            if let Some(mi) = app(|a| a.drag) {
                drag_to(hwnd, mi, xy(lp).0);
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            0
        }
        WM_CAPTURECHANGED => {
            app(|a| a.drag = None);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: xy(lp).0, y: xy(lp).1 };
            ScreenToClient(hwnd, &mut pt);
            // Monitor under the cursor; all of them if over empty space
            let list: Vec<(usize, u32)> = match hit(pt.y) {
                Some(mi) => app(|a| vec![(mi, a.mons[mi].pct)]),
                None => app(|a| {
                    rows(a).into_iter().filter(|&i| a.mons[i].state == State::Ok).map(|i| (i, a.mons[i].pct)).collect()
                }),
            };
            for (mi, p) in list {
                let np = (p as i32 + delta * 5 / 120).clamp(0, 100) as u32;
                set_pct(hwnd, mi, np);
            }
            0
        }
        WM_KEYDOWN => {
            if wp == VK_ESCAPE as usize {
                hide_popup(hwnd);
            }
            0
        }
        WM_DISPLAYCHANGE => {
            SetTimer(hwnd, TIMER_REENUM, 1500, None);
            0
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC as usize {
                SetTimer(hwnd, TIMER_REENUM, 3000, None);
            }
            1
        }
        WM_TIMER => {
            KillTimer(hwnd, wp);
            match wp {
                TIMER_REENUM => reenumerate(hwnd),
                TIMER_TRAY_RETRY => add_tray(hwnd),
                _ => {}
            }
            0
        }
        WM_SETTINGCHANGE => {
            refresh_theme(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() {
    unsafe {
        let mname = w("Local\\BrightnessTraySingleInstance");
        let _mutex = CreateMutexW(null(), 0, mname.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        DPI.store(GetDpiForSystem().max(96), Ordering::Relaxed);

        let hinst = GetModuleHandleW(null());
        let cls = w("BrightnessTray");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: cls.as_ptr(),
            ..zeroed()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            cls.as_ptr(),
            cls.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        let round: i32 = 2; // DWMWCP_ROUND (Windows 11)
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as _, &round as *const _ as *const c_void, 4);
        let tbm = w("TaskbarCreated");
        TASKBAR_MSG.store(RegisterWindowMessageW(tbm.as_ptr()), Ordering::Relaxed);

        let face = w("Segoe UI");
        let mk_font = |weight: i32| {
            CreateFontW(-s(14), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr())
        };
        let light = reg_dword(THEME_KEY, "SystemUsesLightTheme") == Some(1);
        let (tx, rx) = channel();
        let results = Arc::new(Mutex::new(Vec::new()));
        let (r2, hw) = (results.clone(), hwnd as usize);
        std::thread::spawn(move || worker(rx, r2, hw));

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                mons: enumerate(),
                tx,
                results,
                drag: None,
                last_hide: 0,
                font: mk_font(400),
                font_bold: mk_font(600),
                icon: make_icon(light),
                light_taskbar: light,
                theme: make_theme(),
            })
        });
        add_tray(hwnd);
        request_read();

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
