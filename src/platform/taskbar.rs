//! The taskbar we live in. The pill is a child window of `Shell_TrayWnd` (TrafficMonitor-style):
//! a separate topmost popup gets covered whenever the taskbar itself has focus, a child never does.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use windows::core::{w, BSTR, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement, SetWinEventHook, TreeScope_Children,
    TreeScope_Descendants, HWINEVENTHOOK, UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId,
};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETSTATE, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::platform::log::log;
use crate::platform::windows::post_taskbar_items;
use crate::platform::os::{self, Generation};
use crate::render::layout::{RectPx, TaskbarAlign, TaskbarInfo};

/// The main screen's taskbar.
pub fn find_taskbar() -> Option<HWND> {
    unsafe { FindWindowW(w!("Shell_TrayWnd"), None).ok() }
}

/// Every taskbar with its screen's device name (`\\.\DISPLAY2`) and rect, the main one first:
/// the others (Shell_SecondaryTrayWnd) exist while Windows shows the taskbar on all displays.
pub fn taskbars() -> Vec<(HWND, String, RectPx)> {
    let mut bars: Vec<HWND> = find_taskbar().into_iter().collect();
    let mut prev = None;
    unsafe {
        while let Ok(bar) = FindWindowExW(None, prev, w!("Shell_SecondaryTrayWnd"), None) {
            prev = Some(bar);
            bars.push(bar);
        }
    }
    let nowhere = RectPx { x: 0, y: 0, w: 0, h: 0 };
    bars.into_iter().map(|bar| (bar, screen_of(bar).unwrap_or_default(), monitor_rect(bar).unwrap_or(nowhere))).collect()
}

/// The taskbar the pill lives in: the one on `screen` (a device name, as [`taskbars`] gives),
/// else the main screen's.
pub fn find_taskbar_for(screen: &str) -> Option<HWND> {
    let bars = taskbars();
    let screens: Vec<String> = bars.iter().map(|(_, s, _)| s.clone()).collect();
    bars.get(pick_bar(screen, &screens)).map(|(bar, _, _)| *bar)
}

/// Where a fullscreen app on screen `home` sends the pill: the first other one (of the
/// taskbars' screens, `fullscreen` saying which are) that isn't fullscreen too.
pub fn pick_away(home: usize, fullscreen: &[bool]) -> Option<usize> {
    (0..fullscreen.len()).find(|&i| i != home && !fullscreen[i])
}

static PILL_SCREEN: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The screen the pill lives on now (its setting's, or where a fullscreen app sent it): the
/// items thread reads that taskbar.
pub fn set_pill_screen(screen: &str) {
    if let Ok(mut s) = PILL_SCREEN.lock() {
        screen.clone_into(&mut s);
    }
}

fn pill_screen() -> String {
    PILL_SCREEN.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Which of `screens` (the main one first) takes the pill for the setting `screen`.
pub fn pick_bar(screen: &str, screens: &[String]) -> usize {
    if screen.is_empty() {
        return 0;
    }
    screens.iter().position(|s| s == screen).unwrap_or(0)
}

fn monitor_info(hwnd: HWND) -> Option<MONITORINFOEXW> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool().then_some(info)
    }
}

/// The device name of the screen `hwnd` is on.
fn screen_of(hwnd: HWND) -> Option<String> {
    let info = monitor_info(hwnd)?;
    let n = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
    Some(String::from_utf16_lossy(&info.szDevice[..n]))
}

fn monitor_rect(hwnd: HWND) -> Option<RectPx> {
    let r = monitor_info(hwnd)?.monitorInfo.rcMonitor;
    Some(RectPx { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top })
}

/// A taskbar sits at the bottom when its middle is in the lower half of its screen (an
/// auto-hidden one is mostly below it).
pub fn at_bottom(bar: RectPx, screen: RectPx) -> bool {
    bar.y + bar.h / 2 > screen.y + screen.h / 2
}

/// The tray from the first of its elements to the last, those with a size on the bar's row.
pub fn tray_span(on: RectPx, elements: &[RectPx]) -> Option<(i32, i32)> {
    let on_row = |r: &&RectPx| r.w > 0 && r.h > 0 && r.bottom() > on.y && r.y < on.bottom();
    let row: Vec<&RectPx> = elements.iter().filter(on_row).collect();
    let left = row.iter().map(|r| r.x).min()?;
    let right = row.iter().map(|r| r.right()).max()?;
    Some((left, right))
}

pub fn window_rect(hwnd: HWND) -> Result<RectPx> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r)? };
    Ok(RectPx { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top })
}

fn explorer_dword(name: windows::core::PCWSTR) -> Option<u32> {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value)
}

/// Taskbar rect plus the settings that decide where free space is ([`alignment`]). Another
/// screen's taskbar has no Widgets button, and the shell says the edge only of the main one.
pub fn taskbar_info(bar: HWND) -> Result<TaskbarInfo> {
    let (align, widgets) = alignment(os::generation(), explorer_dword(w!("TaskbarAl")), explorer_dword(w!("TaskbarDa")));
    let rect = window_rect(bar)?;
    if find_taskbar() == Some(bar) {
        return Ok(TaskbarInfo { rect, align, widgets, bottom: edge().is_none_or(|e| e == ABE_BOTTOM) });
    }
    let bottom = monitor_rect(bar).is_none_or(|screen| at_bottom(rect, screen));
    Ok(TaskbarInfo { rect, align, widgets: false, bottom })
}

/// Which screen edge the taskbar is on (`None`: the shell didn't say; taken as the bottom,
/// so a failed ask doesn't hide the pill until the next settings change).
fn edge() -> Option<u32> {
    let mut data = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
    (unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut data) } != 0).then_some(data.uEdge)
}

/// The taskbar hides itself until the mouse comes (then a maximised window covers the monitor).
pub fn auto_hides() -> bool {
    let mut data = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
    unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) as u32 & ABS_AUTOHIDE != 0 }
}

const ABE_BOTTOM: u32 = 3;
const ABS_AUTOHIDE: u32 = 1;

/// Makes `child` a child of the taskbar and places it at `rect` (see [`place`]).
pub fn embed(child: HWND, bar: HWND, origin: RectPx, rect: RectPx) -> Result<()> {
    unsafe {
        // Windows keeps WS_CLIPSIBLINGS here; composition content only shows with it. Because a
        // clip-siblings window is cut out wherever a sibling lies above it, the composition window
        // must be the top-most of our children (see `PillWindow::set_rects`).
        let visible = GetWindowLongPtrW(child, GWL_STYLE) as u32 & WS_VISIBLE.0;
        SetWindowLongPtrW(child, GWL_STYLE, (WS_CHILD.0 | visible) as isize);
        // A top-level window has no previous parent, so SetParent may "fail" with a stale last
        // error even though it worked: check the outcome instead. Right after a previous instance
        // was killed (or Explorer restarted) the taskbar refuses for a moment, so retry briefly.
        let mut attached = false;
        for attempt in 0..30 {
            let _ = SetParent(child, Some(bar));
            if GetParent(child).ok() == Some(bar) {
                attached = true;
                if attempt > 0 {
                    crate::platform::log::log(&format!("embedded into the taskbar after {attempt} retries"));
                }
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !attached {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "SetParent into the taskbar failed"));
        }
    }
    place(child, origin, rect)
}

/// Moves an embedded child to `rect` and keeps it above its siblings. `rect` is in screen
/// coordinates of the taskbar at `origin`, the rect it was worked out from, not of where the
/// taskbar is now: an auto-hiding one slides, and its children slide along with it.
pub fn place(child: HWND, origin: RectPx, rect: RectPx) -> Result<()> {
    unsafe {
        SetWindowPos(
            child,
            Some(HWND_TOP),
            rect.x - origin.x,
            rect.y - origin.y,
            rect.w,
            rect.h,
            SWP_NOACTIVATE,
        )
        .inspect_err(|e| crate::platform::log::log(&format!("place {rect:?}: {e:?}")))
    }
}

/// The x spans of the taskbar's own items (Start, Search, the app buttons, the tray...), in
/// screen pixels, for the main thread (told with `post_taskbar_items`).
pub type ItemsMailbox = Arc<Mutex<Vec<(i32, i32)>>>;

/// After a window shows or hides the buttons are read soon, and again once the taskbar's own
/// animation has settled.
const LOOK_SOON_MS: [u64; 2] = [150, 700];
/// And every so often regardless: a renamed or pinned button, a missed event.
const LOOK_EVERY_MS: u64 = 2000;

thread_local! {
    static NUDGED: Cell<bool> = const { Cell::new(false) };
}

extern "system" fn on_window(_: HWINEVENTHOOK, _: u32, hwnd: HWND, object: i32, child: i32, _: u32, _: u32) {
    if object != OBJID_WINDOW.0 || child != CHILDID_SELF as i32 {
        return;
    }
    // Only top-level windows that may have a button: no children, no tool windows.
    unsafe {
        if GetAncestor(hwnd, GA_ROOT) != hwnd || GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return;
        }
    }
    NUDGED.with(|n| n.set(true));
}

/// Starts the thread that reads where the taskbar's items are and posts them whenever they
/// change. Windows 11's taskbar is XAML: only UI Automation knows its buttons' rects (the old
/// MSTaskListWClass window is still there, at a stale size). Windows 10's is windows, but its
/// app buttons too are known only to UI Automation.
pub fn watch_items(mailbox: ItemsMailbox) {
    let spawned = std::thread::Builder::new().name("taskbar-items".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut reader = match ItemReader::new() {
            Ok(r) => r,
            Err(e) => return log(&format!("taskbar items: no UI Automation ({e})")),
        };
        // An app opening or closing shows or hides (or, for packaged apps, uncloaks) its window.
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let _show = SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_HIDE, None, Some(on_window), 0, 0, flags);
        let _cloak = SetWinEventHook(EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED, None, Some(on_window), 0, 0, flags);
        let mut last: Option<Vec<(i32, i32)>> = None;
        let mut soon: Vec<u64> = Vec::new();
        let mut next = GetTickCount64();
        loop {
            let now = GetTickCount64();
            if now >= next || soon.iter().any(|&t| t <= now) {
                soon.retain(|&t| t > now);
                next = now + LOOK_EVERY_MS;
                let items = reader.read();
                if last.as_ref() != Some(&items) {
                    if last.is_none() {
                        log(&format!("taskbar items ({:?}): {items:?}", reader.generation));
                    }
                    if let Ok(mut slot) = mailbox.lock() {
                        slot.clone_from(&items);
                    }
                    post_taskbar_items();
                    last = Some(items);
                }
            }
            let due = soon.iter().copied().chain([next]).min().unwrap_or(next);
            let wait = due.saturating_sub(GetTickCount64()).min(u32::MAX as u64) as u32;
            MsgWaitForMultipleObjects(None, false, wait, QS_ALLINPUT);
            // The hooks' callbacks run in here.
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            if NUDGED.with(|n| n.replace(false)) && soon.is_empty() {
                let now = GetTickCount64();
                soon = LOOK_SOON_MS.iter().map(|ms| now + ms).collect();
            }
        }
    });
    if let Err(e) = spawned {
        log(&format!("taskbar items: no thread ({e})"));
    }
}

/// `parent`'s visible child windows, but ours (the pill is the taskbar's child too).
fn children(parent: HWND) -> Vec<BarWindow> {
    let mut found = Vec::new();
    let mut prev = None;
    unsafe {
        while let Ok(hwnd) = FindWindowExW(Some(parent), prev, None, None) {
            prev = Some(hwnd);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if !IsWindowVisible(hwnd).as_bool() || pid == GetCurrentProcessId() {
                continue;
            }
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut class).max(0) as usize;
            if let Ok(rect) = window_rect(hwnd) {
                found.push(BarWindow { class: String::from_utf16_lossy(&class[..n]), rect });
            }
        }
    }
    found
}

struct ItemReader {
    uia: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    generation: Generation,
    /// The taskbar and its XAML frame (Start, Search, the buttons), found once per taskbar.
    frame: Option<(HWND, IUIAutomationElement)>,
}

impl ItemReader {
    fn new() -> Result<Self> {
        unsafe {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let cache = uia.CreateCacheRequest()?;
            cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
            Ok(ItemReader { uia, cache, generation: os::generation(), frame: None })
        }
    }

    /// The items' spans on the pill's taskbar now: the frame's children (one cross-process call,
    /// about 6 ms) and the tray's window (or, on another screen's, its XAML elements), or Windows
    /// 10's windows and buttons. Empty when there is no taskbar.
    fn read(&mut self) -> Vec<(i32, i32)> {
        let Some(bar) = find_taskbar_for(&pill_screen()) else {
            self.frame = None;
            return Vec::new();
        };
        let Ok(on) = window_rect(bar) else { return Vec::new() };
        if self.generation == Generation::Windows10 {
            return self.read_classic(bar, on);
        }
        let mut spans = Vec::new();
        if let Some(frame) = self.frame(bar) {
            match self.children(&frame, on) {
                Ok(v) => spans = v,
                // Explorer restarted under the same handle, or the frame was rebuilt.
                Err(_) => self.frame = None,
            }
        }
        match unsafe { FindWindowExW(Some(bar), None, w!("TrayNotifyWnd"), None) } {
            Ok(tray) => {
                if let Ok(r) = window_rect(tray) {
                    spans.push((r.x, r.right()));
                }
            }
            // Another screen's taskbar: its clock and buttons are only XAML.
            Err(_) => spans.extend(tray_span(on, &self.tray_elements(bar))),
        }
        spans
    }

    /// The rects of the elements Windows 11's XAML tray is made of (the clock, the
    /// notifications button, Show desktop...: AutomationId SystemTrayIcon).
    fn tray_elements(&self, bar: HWND) -> Vec<RectPx> {
        let read = || -> Result<Vec<RectPx>> {
            unsafe {
                let id = VARIANT::from(BSTR::from("SystemTrayIcon"));
                let condition = self.uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &id)?;
                let all = self.uia.ElementFromHandle(bar)?.FindAllBuildCache(TreeScope_Descendants, &condition, &self.cache)?;
                let mut rects = Vec::new();
                for i in 0..all.Length()? {
                    let r = all.GetElement(i)?.CachedBoundingRectangle()?;
                    rects.push(RectPx { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
                }
                Ok(rects)
            }
        };
        read().unwrap_or_default()
    }

    fn frame(&mut self, bar: HWND) -> Option<IUIAutomationElement> {
        if self.frame.as_ref().is_none_or(|(b, _)| *b != bar) {
            self.frame = unsafe {
                let id = VARIANT::from(BSTR::from("TaskbarFrame"));
                let found = self.uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &id).and_then(|c| {
                    self.uia.ElementFromHandle(bar)?.FindFirst(TreeScope_Descendants, &c)
                });
                found.ok().map(|f| (bar, f))
            };
        }
        self.frame.as_ref().map(|(_, f)| f.clone())
    }

    /// Windows 10: the taskbar's and its rebar's windows, and the app buttons in MSTaskListWClass
    /// (a window as wide as their whole band) from UI Automation. Another screen's taskbar has a
    /// WorkerW where the main one has its rebar.
    fn read_classic(&self, bar: HWND, on: RectPx) -> Vec<(i32, i32)> {
        let top = children(bar);
        let rebar = unsafe { FindWindowExW(Some(bar), None, w!("ReBarWindow32"), None).or_else(|_| FindWindowExW(Some(bar), None, w!("WorkerW"), None)) }.ok();
        let band = rebar.map(children).unwrap_or_default();
        let list = rebar.and_then(|r| unsafe {
            let sw = FindWindowExW(Some(r), None, w!("MSTaskSwWClass"), None).ok()?;
            FindWindowExW(Some(sw), None, w!("MSTaskListWClass"), None).ok()
        });
        let buttons = list.map(|l| self.buttons(l)).unwrap_or_default();
        classic_items(on, &top, &band, &buttons)
    }

    /// The app buttons' rects (empty when UI Automation can't say: the pill then keeps its home).
    fn buttons(&self, list: HWND) -> Vec<RectPx> {
        let read = || -> Result<Vec<RectPx>> {
            unsafe {
                let all = self.uia.ElementFromHandle(list)?.FindAllBuildCache(TreeScope_Children, &self.uia.CreateTrueCondition()?, &self.cache)?;
                let mut rects = Vec::new();
                for i in 0..all.Length()? {
                    let r = all.GetElement(i)?.CachedBoundingRectangle()?;
                    rects.push(RectPx { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
                }
                Ok(rects)
            }
        };
        read().unwrap_or_default()
    }

    /// The frame's children with a size, on the taskbar's row.
    fn children(&self, frame: &IUIAutomationElement, on: RectPx) -> Result<Vec<(i32, i32)>> {
        unsafe {
            let all = frame.FindAllBuildCache(TreeScope_Children, &self.uia.CreateTrueCondition()?, &self.cache)?;
            let mut spans = Vec::new();
            for i in 0..all.Length()? {
                let r = all.GetElement(i)?.CachedBoundingRectangle()?;
                if r.right > r.left && r.bottom > on.y && r.top < on.bottom() {
                    spans.push((r.left, r.right));
                }
            }
            Ok(spans)
        }
    }
}

/// Icons centred (`TaskbarAl`) and Widgets on (`TaskbarDa`), with Windows 11's defaults for a
/// missing value. Windows 10's buttons always start at the left and it has no Widgets button.
pub fn alignment(generation: Generation, al: Option<u32>, da: Option<u32>) -> (TaskbarAlign, bool) {
    match generation {
        Generation::Windows10 => (TaskbarAlign::Left, false),
        Generation::Windows11 => (if al == Some(0) { TaskbarAlign::Left } else { TaskbarAlign::Center }, da.unwrap_or(1) != 0),
    }
}

/// One of the classic taskbar's windows (visible, not ours).
#[derive(Clone, Debug)]
pub struct BarWindow {
    pub class: String,
    pub rect: RectPx,
}

/// What takes room on Windows 10's taskbar `on` (spans in screen pixels): the taskbar's own
/// children (`top`: Start, Search, Task View, the tray...) but the rebar, the rebar's (`band`:
/// toolbars) but the app buttons' band, and the app buttons themselves (`buttons`).
pub fn classic_items(on: RectPx, top: &[BarWindow], band: &[BarWindow], buttons: &[RectPx]) -> Vec<(i32, i32)> {
    // A window as wide as half the bar hosts others (the rebar is already left out by name).
    let item = |r: &RectPx| r.w > 0 && r.h > 0 && r.w <= on.w / 2 && r.bottom() > on.y && r.y < on.bottom();
    let windows = top.iter().filter(|w| w.class != "ReBarWindow32").chain(band.iter().filter(|w| w.class != "MSTaskSwWClass"));
    windows.map(|w| w.rect).chain(buttons.iter().copied()).filter(item).map(|r| (r.x, r.right())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_10_buttons_are_always_on_the_left() {
        assert_eq!(alignment(Generation::Windows10, None, None), (TaskbarAlign::Left, false));
        assert_eq!(alignment(Generation::Windows10, Some(1), Some(1)), (TaskbarAlign::Left, false), "a leftover value");
    }

    #[test]
    fn windows_11_reads_its_settings() {
        assert_eq!(alignment(Generation::Windows11, None, None), (TaskbarAlign::Center, true));
        assert_eq!(alignment(Generation::Windows11, Some(0), Some(0)), (TaskbarAlign::Left, false));
        assert_eq!(alignment(Generation::Windows11, Some(1), Some(1)), (TaskbarAlign::Center, true));
    }

    fn window(class: &str, x: i32, right: i32) -> BarWindow {
        BarWindow { class: class.into(), rect: RectPx { x, y: 1040, w: right - x, h: 40 } }
    }

    /// Windows 10 at 100 %: Start, the search box, Task View, the app buttons' band up to the
    /// tray, and the buttons UI Automation finds in it.
    fn win10_bar() -> (RectPx, Vec<BarWindow>, Vec<BarWindow>) {
        let on = RectPx { x: 0, y: 1040, w: 1920, h: 40 };
        let top = vec![
            window("Start", 0, 48),
            window("TrayDummySearchControl", 48, 392),
            window("TrayButton", 392, 440),
            window("ReBarWindow32", 440, 1660),
            window("TrayNotifyWnd", 1660, 1920),
        ];
        (on, top, vec![window("MSTaskSwWClass", 442, 1660)])
    }

    #[test]
    fn classic_taskbar_items_skip_the_band_and_take_the_buttons() {
        let (on, top, band) = win10_bar();
        let buttons: Vec<RectPx> = (0..3).map(|i| RectPx { x: 442 + 48 * i, y: 1040, w: 48, h: 40 }).collect();
        let items = classic_items(on, &top, &band, &buttons);
        assert_eq!(items, vec![(0, 48), (48, 392), (392, 440), (1660, 1920), (442, 490), (490, 538), (538, 586)]);
    }

    /// A toolbar beside the app buttons (Links, Desktop) takes its room.
    #[test]
    fn toolbars_in_the_rebar_count() {
        let (on, top, mut band) = win10_bar();
        band[0].rect.w = 1500 - 442;
        band.push(window("ToolbarWindow32", 1500, 1660));
        assert!(classic_items(on, &top, &band, &[]).contains(&(1500, 1660)));
    }

    /// Nothing that only hosts others (as wide as most of the bar), nothing empty, nothing off the
    /// taskbar's row (a button scrolled to another page).
    #[test]
    fn hosts_empty_and_off_row_windows_dont_count() {
        let (on, mut top, band) = win10_bar();
        top.push(window("Windows.UI.Composition.DesktopWindowContentBridge", 0, 1920));
        top.push(window("Empty", 900, 900));
        let off = RectPx { x: 700, y: 1080, w: 48, h: 40 };
        let items = classic_items(on, &top, &band, &[off]);
        assert_eq!(items, vec![(0, 48), (48, 392), (392, 440), (1660, 1920)]);
    }

    /// Issue #7: the pill on another screen's taskbar; one that isn't there, the main one's.
    #[test]
    fn the_pill_takes_the_chosen_screens_taskbar() {
        let screens = [r"\\.\DISPLAY1".to_string(), r"\\.\DISPLAY2".to_string()];
        assert_eq!(pick_bar("", &screens), 0, "the main screen");
        assert_eq!(pick_bar(r"\\.\DISPLAY2", &screens), 1);
        assert_eq!(pick_bar(r"\\.\DISPLAY1", &screens), 0);
        assert_eq!(pick_bar(r"\\.\DISPLAY3", &screens), 0, "not plugged in: the main one");
    }

    /// The shell says where only the main taskbar is; another screen's is at the bottom when it
    /// sits in the screen's lower half, auto-hidden (mostly below the screen) too.
    #[test]
    fn a_taskbar_is_at_the_bottom_by_where_it_sits() {
        let screen = RectPx { x: 1920, y: 0, w: 2560, h: 1440 };
        assert!(at_bottom(RectPx { x: 1920, y: 1392, w: 2560, h: 48 }, screen));
        assert!(at_bottom(RectPx { x: 1920, y: 1438, w: 2560, h: 48 }, screen), "auto-hidden");
        assert!(!at_bottom(RectPx { x: 1920, y: 0, w: 2560, h: 48 }, screen));
    }

    /// Another screen's taskbar has no TrayNotifyWnd: its clock and buttons are XAML elements
    /// (SystemTrayIcon), and the tray runs from the first to the last on the bar's row.
    #[test]
    fn the_tray_spans_its_elements_on_the_row() {
        let on = RectPx { x: 1920, y: 1030, w: 1920, h: 50 };
        let el = |x: i32, right: i32| RectPx { x, y: 1030, w: right - x, h: 50 };
        assert_eq!(tray_span(on, &[el(3790, 3820), el(3703, 3785), el(3825, 3840)]), Some((3703, 3840)));
        let off_row = RectPx { x: 3500, y: 0, w: 30, h: 30 };
        assert_eq!(tray_span(on, &[el(3703, 3785), off_row, el(3600, 3600)]), Some((3703, 3785)), "off the row or empty");
        assert_eq!(tray_span(on, &[]), None);
    }

    /// This PC's taskbars (read-only): the main one first and the pick falling back to it; the
    /// XAML tray read the way another screen's is spans what TrayNotifyWnd does on the main one
    /// (Windows 11). `cargo t --lib live_taskbars -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_taskbars() {
        let bars = taskbars();
        println!("taskbars: {:?}", bars.iter().map(|(_, s, r)| (s, r)).collect::<Vec<_>>());
        let main = find_taskbar().expect("a taskbar");
        assert_eq!(bars[0].0, main);
        assert_eq!(find_taskbar_for(""), Some(main));
        assert_eq!(find_taskbar_for(r"\\.\DISPLAY99"), Some(main), "not plugged in");
        assert_eq!(find_taskbar_for(&bars[0].1), Some(main));
        let on = window_rect(main).unwrap();
        assert!(at_bottom(on, monitor_rect(main).unwrap()));
        if os::generation() == Generation::Windows11 {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
            let reader = ItemReader::new().unwrap();
            let xaml = tray_span(on, &reader.tray_elements(main)).expect("tray elements");
            let tray = unsafe { FindWindowExW(Some(main), None, w!("TrayNotifyWnd"), None) }.map(|t| window_rect(t).unwrap()).unwrap();
            println!("xaml tray {xaml:?}, TrayNotifyWnd ({}, {})", tray.x, tray.right());
            assert!((xaml.0 - tray.x).abs() <= 4 && (xaml.1 - tray.right()).abs() <= 4);
        }
    }

    /// Windows 10's taskbar on another screen: Start, then the buttons' band in a WorkerW (no
    /// rebar, no tray).
    #[test]
    fn a_classic_taskbar_on_another_screen() {
        let on = RectPx { x: 1920, y: 1040, w: 1920, h: 40 };
        let top = vec![window("Start", 1920, 1968), window("WorkerW", 1968, 3840)];
        let band = vec![window("MSTaskSwWClass", 1968, 3840)];
        let buttons = [RectPx { x: 1968, y: 1040, w: 48, h: 40 }];
        assert_eq!(classic_items(RectPx { x: 1920, ..on }, &top, &band, &buttons), vec![(1920, 1968), (1968, 2016)]);
    }

    /// A fullscreen app on the pill's screen sends it to the first other screen with a taskbar
    /// that isn't fullscreen too; none: it stays (and hides).
    #[test]
    fn fullscreen_sends_the_pill_to_another_screen() {
        assert_eq!(pick_away(0, &[true, false]), Some(1));
        assert_eq!(pick_away(1, &[false, true]), Some(0), "from a second screen to the main one");
        assert_eq!(pick_away(0, &[true, true, false]), Some(2), "past a fullscreen one");
        assert_eq!(pick_away(0, &[true, true]), None);
        assert_eq!(pick_away(0, &[true]), None, "one screen");
    }
}
