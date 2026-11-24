//! The playing app's own window, brought to the front from its media card (One UI's Now Bar
//! opens the player). The click was on Pebbly, so Windows lets the foreground be handed on.

use windows::core::{BOOL, GUID, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, PROPERTYKEY};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow,
    ShowWindow, GWL_EXSTYLE, GW_OWNER, SW_RESTORE, SW_SHOWNORMAL, WS_EX_TOOLWINDOW,
};

use crate::activities::media::{pick_window, show_target};

/// PKEY_AppUserModel_ID: the app id a window told the shell (Store app frames, Firefox).
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3), pid: 5 };

/// `hwnd` comes up, restored if minimised. False when it is gone.
pub fn bring_to_front(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// The GSMTC app `app_id`'s window comes up: the one showing `title` (a browser's playing
/// tab), else its topmost ([`pick_window`]). With none on screen the app is asked to show
/// itself ([`show_target`]). False when nothing could be done.
pub fn focus_app(app_id: &str, title: &str) -> bool {
    let windows = windows_on_screen();
    let list: Vec<(&str, &str, Option<&str>, &str)> = windows.iter().map(|w| (w.exe.as_str(), w.path.as_str(), w.aumid.as_deref(), w.title.as_str())).collect();
    if let Some(i) = pick_window(&list, app_id, title) {
        return bring_to_front(windows[i].hwnd);
    }
    let Some(target) = show_target(app_id) else { return false };
    let shown = unsafe { windows::Win32::UI::Shell::ShellExecuteW(None, windows::core::w!("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL) };
    // ShellExecute's success is a value above 32.
    shown.0 as usize > 32
}

/// A window one can switch to, as Alt+Tab lists them.
struct AppWindow {
    hwnd: HWND,
    exe: String,
    path: String,
    aumid: Option<String>,
    title: String,
}

/// The windows Alt+Tab would show (visible, not a tool window, not owned, not cloaked: a
/// Store app suspended on another desktop is), top of the z-order first.
fn windows_on_screen() -> Vec<AppWindow> {
    unsafe extern "system" fn each(hwnd: HWND, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<AppWindow>) };
        let shown = unsafe { IsWindowVisible(hwnd) }.as_bool();
        let tool = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW.0 != 0;
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
        let mut cloaked = 0u32;
        let _ = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as _, 4) };
        if shown && !tool && !owned && cloaked == 0 {
            let mut buf = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
            if len > 0 {
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                let path = super::fullscreen::image_of_pid(pid).unwrap_or_default();
                let exe = path.rsplit('\\').next().unwrap_or_default().to_string();
                out.push(AppWindow { hwnd, exe, path, aumid: window_aumid(hwnd), title: String::from_utf16_lossy(&buf[..len]) });
            }
        }
        true.into()
    }
    let mut out = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window the card would bring up for `PEBBLY_APP` (a GSMTC id; "Brave" without it) and
    /// what plays, `PEBBLY_TITLE` (read-only: nothing comes to the front).
    /// `cargo t --lib live_app_window -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_app_window() {
        let app = std::env::var("PEBBLY_APP").unwrap_or_else(|_| "Brave".into());
        let title = std::env::var("PEBBLY_TITLE").unwrap_or_default();
        let windows = windows_on_screen();
        let list: Vec<(&str, &str, Option<&str>, &str)> = windows.iter().map(|w| (w.exe.as_str(), w.path.as_str(), w.aumid.as_deref(), w.title.as_str())).collect();
        println!("{} windows on screen", list.len());
        let picked = pick_window(&list, &app, &title);
        println!("{app}: {:?}", picked.map(|i| (&windows[i].exe, &windows[i].aumid, &windows[i].title)));
        println!("with none: {:?}", show_target(&app));
    }
}

/// The app id set on the window itself, if any.
fn window_aumid(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let mut value = store.GetValue(&PKEY_APP_USER_MODEL_ID).ok()?;
        let text = PropVariantToStringAlloc(&value).ok();
        let _ = PropVariantClear(&mut value);
        let text = text?;
        let s = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        s.filter(|s| !s.is_empty())
    }
}
