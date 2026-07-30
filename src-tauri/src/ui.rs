//! Tauri UI glue: window positioning, click-through, and overlay lifecycle.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::Manager;

static FRONTEND_READY_EPOCH: AtomicU64 = AtomicU64::new(0);
static FRONTEND_RELOAD_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECORDING_CYCLE_ACTIVE: AtomicBool = AtomicBool::new(false);
static LAST_OVERLAY_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);
static PILL_STATE: AtomicU8 = AtomicU8::new(PillState::Idle as u8);
static PILL_ERROR_TEXT: OnceLock<Mutex<String>> = OnceLock::new();

#[derive(serde::Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PillState {
    Idle = 0,
    Recording = 1,
    Transcribing = 2,
    Error = 3,
}

impl PillState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Recording,
            2 => Self::Transcribing,
            3 => Self::Error,
            _ => Self::Idle,
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PillSnapshot {
    state: PillState,
    error_text: String,
}

fn error_text() -> &'static Mutex<String> {
    PILL_ERROR_TEXT.get_or_init(|| Mutex::new(String::new()))
}

fn pill_hide_allowed(cycle_active: bool, pill_state: PillState) -> bool {
    !cycle_active || matches!(pill_state, PillState::Error)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn touch_overlay_activity() {
    LAST_OVERLAY_ACTIVITY_MS.store(now_ms(), Ordering::SeqCst);
}

#[cfg(target_os = "windows")]
fn settings_webview_data_dir() -> Option<std::path::PathBuf> {
    let dir = match crate::paths::user_data_dir() {
        Ok(dir) => dir.join("webview-settings"),
        Err(e) => {
            tracing::warn!("settings webview data dir unavailable: {:#}", e);
            return None;
        }
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("settings webview data dir create failed {:?}: {}", dir, e);
        return None;
    }

    Some(dir)
}

pub fn open_settings_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("settings") {
        if let Err(e) = window.eval("void 0") {
            tracing::warn!(
                "settings window exists but webview is not usable; recreating it: {}",
                e
            );
            let _ = window.close();
        } else {
            window.show()?;
            window.set_focus()?;
            tracing::info!("settings window shown");
            return Ok(());
        }
    }

    tracing::info!("creating settings window on demand");
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("/settings".into()),
    )
    .title("flov - Settings")
    .inner_size(1240.0, 880.0)
    .min_inner_size(1080.0, 720.0)
    .decorations(false)
    .shadow(false)
    .resizable(true)
    .visible(true)
    .skip_taskbar(false)
    .focused(true)
    .center();

    // WebView2 requires webviews with different environment settings
    // (notably additional browser args) to use different data directories.
    // The main pill disables native occlusion with custom Chromium flags, so
    // Settings must not share its default profile or it can fail with
    // 0x8007139F while leaving behind a blank window shell.
    #[cfg(target_os = "windows")]
    let builder = if let Some(data_dir) = settings_webview_data_dir() {
        tracing::info!("settings webview data dir: {:?}", data_dir);
        builder.data_directory(data_dir)
    } else {
        builder
    };

    // On Windows the old hidden+transparent settings webview sometimes failed
    // at startup with WebView2 0x8007139F and left tray Open Settings as a
    // silent no-op. The settings surface is full-bleed opaque anyway, so keep
    // transparent windows reserved for the tiny recording overlay.
    #[cfg(not(target_os = "windows"))]
    let builder = builder.transparent(true);

    let builder = builder.icon(crate::tray::load_themed_icon_for_window())?;
    let window = builder.build()?;

    #[cfg(target_os = "windows")]
    disable_native_window_rounding(&window);
    window.set_focus()?;
    tracing::info!("settings window created and focused");
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn position_at_cursor_monitor(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_err() {
            return;
        }
    }

    let hmon: HMONITOR = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info) };
    if !ok.as_bool() {
        return;
    }

    let mon_x = info.rcMonitor.left;
    let mon_y = info.rcMonitor.top;
    let mon_w = info.rcMonitor.right - info.rcMonitor.left;
    let mon_h = info.rcMonitor.bottom - info.rcMonitor.top;

    // tauri.conf width/height are LOGICAL; monitor rect is PHYSICAL.
    let scale = window.scale_factor().unwrap_or(1.0);
    let pill_w = (800.0 * scale) as i32;
    let pill_h = (200.0 * scale) as i32;
    let margin_bottom = (24.0 * scale) as i32;
    let x = mon_x + (mon_w - pill_w) / 2;
    let y = mon_y + mon_h - pill_h - margin_bottom;

    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Disable Win11's native window-corner rounding so the OS doesn't paint a
/// rounded mask on top of our CSS border-radius (which produced ugly Win11
/// corner pixels showing through the frameless transparent window).
#[cfg(target_os = "windows")]
pub fn disable_native_window_rounding(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };

    let raw = match window.hwnd() {
        Ok(h) => h.0,
        Err(_) => return,
    };
    let hwnd = HWND(raw);
    let pref = DWMWCP_DONOTROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const _,
            std::mem::size_of_val(&pref) as u32,
        );
    }
}

#[cfg(target_os = "windows")]
pub fn force_click_through(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };

    let raw = match window.hwnd() {
        Ok(h) => h.0,
        Err(_) => return,
    };
    let hwnd = HWND(raw);

    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new = ex | (WS_EX_LAYERED.0 as isize) | (WS_EX_TRANSPARENT.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
    }
}

#[cfg(target_os = "windows")]
pub fn set_window_alpha(window: &tauri::WebviewWindow, alpha: u8) {
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA,
        WS_EX_LAYERED,
    };

    let raw = match window.hwnd() {
        Ok(h) => h.0,
        Err(_) => return,
    };
    let hwnd = HWND(raw);

    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_LAYERED.0 as isize));
        if let Err(e) = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) {
            tracing::warn!("SetLayeredWindowAttributes(alpha={}) failed: {}", alpha, e);
        }
    }
}

/// Force DWM to recomposite our layered+transparent+frameless pill
/// window after we call `show()`.
///
/// The bug this exists to fix: on long sessions the user calls the
/// hotkey, the backend logs `recording start` and the OS reports the
/// HWND visible, but the pill never visually appears. Backend keeps
/// working — recordings get transcribed and pasted — only the visual
/// is stuck.
///
/// The mechanism is documented in WebView2 issues
/// [#3674](https://github.com/MicrosoftEdge/WebView2Feedback/issues/3674)
/// and [#1130](https://github.com/MicrosoftEdge/WebView2Feedback/issues/1130):
/// for a `WS_EX_LAYERED` window, the standard WM_PAINT path doesn't
/// touch the composited surface — DWM keeps showing whatever was on
/// it last. Without a tickle, the surface can stay blank indefinitely,
/// and on long sessions (or after sleep/wake) this is what users see.
///
/// The cheapest reliable tickle is `RedrawWindow(INVALIDATE | UPDATENOW
/// | ERASE | FRAME)` plus a no-op `SetWindowPos` with
/// `SWP_FRAMECHANGED`, which together force the compositor to
/// re-evaluate the layered content. No flicker because we don't move,
/// resize, or restack.
#[cfg(target_os = "windows")]
pub fn force_repaint(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };

    let raw = match window.hwnd() {
        Ok(h) => h.0,
        Err(_) => return,
    };
    let hwnd = HWND(raw);

    unsafe {
        let _ = RedrawWindow(
            Some(hwnd),
            None,
            None,
            RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN | RDW_ERASE | RDW_FRAME,
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

/// Position the pill at the bottom-center of the screen the cursor is
/// currently on. macOS coordinate space matches what Tauri's
/// LogicalPosition expects (top-left origin, points) — no Y flip
/// needed because CGEventGetLocation and CGDisplayBounds both use
/// global display coordinates anchored to the upper-left corner.
#[cfg(target_os = "macos")]
pub fn position_at_cursor_monitor(window: &tauri::WebviewWindow) {
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let src = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(s) => s,
        Err(_) => return,
    };
    let cursor = match CGEvent::new(src) {
        Ok(e) => e.location(),
        Err(_) => return,
    };

    let displays = match CGDisplay::active_displays() {
        Ok(d) => d,
        Err(_) => return,
    };

    let containing = displays
        .into_iter()
        .map(CGDisplay::new)
        .find(|d| {
            let b = d.bounds();
            cursor.x >= b.origin.x
                && cursor.x < b.origin.x + b.size.width
                && cursor.y >= b.origin.y
                && cursor.y < b.origin.y + b.size.height
        })
        .unwrap_or_else(|| CGDisplay::new(CGDisplay::main().id));
    let mon = containing.bounds();

    // Pill dimensions in *logical* units (= macOS points). Tauri's
    // window config (tauri.conf.json) lists 800×200 in the same units,
    // so no scale-factor multiplication is needed here — unlike the
    // Windows path where rcMonitor is in physical pixels.
    let pill_w: f64 = 800.0;
    let pill_h: f64 = 200.0;
    let margin_bottom: f64 = 24.0;
    let x = mon.origin.x + (mon.size.width - pill_w) / 2.0;
    let y = mon.origin.y + mon.size.height - pill_h - margin_bottom;
    let _ = window.set_position(tauri::LogicalPosition::new(x, y));
}

#[tauri::command]
pub fn repaint_window(window: tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        set_window_alpha(&window, 255);
        force_repaint(&window);
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

#[tauri::command]
pub fn pill_frontend_ready(window: tauri::WebviewWindow) -> PillSnapshot {
    let epoch = FRONTEND_READY_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    FRONTEND_RELOAD_IN_PROGRESS.store(false, Ordering::SeqCst);
    if !recording_cycle_active() {
        set_overlay_active(false);
    }
    let snapshot = pill_snapshot();
    if !matches!(snapshot.state, PillState::Idle) {
        if let Err(e) = window.show() {
            tracing::warn!("pill frontend ready: window.show() failed: {}", e);
        }
    }
    #[cfg(target_os = "windows")]
    {
        force_click_through(&window);
    }
    tracing::info!("pill frontend ready, epoch={}", epoch);
    snapshot
}

pub fn frontend_ready_epoch() -> u64 {
    FRONTEND_READY_EPOCH.load(Ordering::SeqCst)
}

pub fn mark_frontend_reload_started() -> u64 {
    FRONTEND_RELOAD_IN_PROGRESS.store(true, Ordering::SeqCst);
    frontend_ready_epoch()
}

pub fn mark_frontend_reload_finished() {
    FRONTEND_RELOAD_IN_PROGRESS.store(false, Ordering::SeqCst);
}

pub fn frontend_reload_in_progress() -> bool {
    FRONTEND_RELOAD_IN_PROGRESS.load(Ordering::SeqCst)
}

pub fn set_recording_cycle_active(active: bool) {
    RECORDING_CYCLE_ACTIVE.store(active, Ordering::SeqCst);
    touch_overlay_activity();
    if active {
        OVERLAY_ACTIVE.store(true, Ordering::SeqCst);
    }
}

pub fn recording_cycle_active() -> bool {
    RECORDING_CYCLE_ACTIVE.load(Ordering::SeqCst)
}

pub fn set_overlay_active(active: bool) {
    OVERLAY_ACTIVE.store(active, Ordering::SeqCst);
    touch_overlay_activity();
}

pub fn overlay_active() -> bool {
    OVERLAY_ACTIVE.load(Ordering::SeqCst) || recording_cycle_active()
}

pub fn overlay_quiet_for(duration: Duration) -> bool {
    let last = LAST_OVERLAY_ACTIVITY_MS.load(Ordering::SeqCst);
    last != 0 && now_ms().saturating_sub(last) >= duration.as_millis() as u64
}

pub fn wait_for_frontend_ready_after(previous_epoch: u64, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if frontend_ready_epoch() > previous_epoch {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

pub fn wait_for_frontend_reload_if_needed(timeout: Duration) -> bool {
    if !frontend_reload_in_progress() {
        return true;
    }

    let previous_epoch = frontend_ready_epoch();
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !frontend_reload_in_progress() || frontend_ready_epoch() > previous_epoch {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    !frontend_reload_in_progress()
}

pub fn set_pill_state(state: PillState) {
    PILL_STATE.store(state as u8, Ordering::SeqCst);
    if !matches!(state, PillState::Error) {
        error_text().lock().unwrap().clear();
    }
    if !matches!(state, PillState::Idle) {
        set_overlay_active(true);
    } else {
        set_overlay_active(false);
    }
}

pub fn set_pill_error(message: &str) {
    PILL_STATE.store(PillState::Error as u8, Ordering::SeqCst);
    *error_text().lock().unwrap() = message.to_string();
    set_overlay_active(true);
}

pub fn pill_snapshot() -> PillSnapshot {
    PillSnapshot {
        state: PillState::from_u8(PILL_STATE.load(Ordering::SeqCst)),
        error_text: error_text().lock().unwrap().clone(),
    }
}

/// Frontend calls this after the pill's exit transition finishes.
///
/// Do not call `window.hide()` here. On Windows that turns into an HWND
/// `SW_HIDE`, and the embedded WebView2 can be treated as hidden/background:
/// timers are throttled, renderer state can suspend, and the next `show()`
/// may come back with a blank stale surface. The visual hide is the Svelte
/// `{#if}` unmount; the OS window remains transparent and click-through.
#[tauri::command]
pub fn hide_window(window: tauri::WebviewWindow) {
    let cycle_active = recording_cycle_active();
    let pill_state = PillState::from_u8(PILL_STATE.load(Ordering::SeqCst));
    // An error can be raised before recording starts at all (for example,
    // when no model is installed). Its frontend timer may expire while the
    // hotkey is still held. Hiding that error is safe; blocking it forever
    // leaves the whole Wayland surface mapped after key release. A stale
    // idle timer from an earlier cycle is still rejected once the new cycle
    // has switched the pill to Recording/Transcribing.
    let can_hide = pill_hide_allowed(cycle_active, pill_state);

    if can_hide {
        set_pill_state(PillState::Idle);
    } else {
        tracing::info!("logical pill hide received while a recording cycle is active");
    }
    set_overlay_active(false);
    #[cfg(target_os = "windows")]
    {
        force_click_through(&window);
        if !recording_cycle_active() {
            set_window_alpha(&window, 0);
        }
        force_repaint(&window);
    }
    #[cfg(target_os = "linux")]
    if can_hide {
        if let Err(e) = window.hide() {
            tracing::warn!("Linux pill window.hide() failed: {}", e);
        }
    }
    #[cfg(target_os = "macos")]
    let _ = window;
}

#[cfg(test)]
mod tests {
    use super::{pill_hide_allowed, PillState};

    #[test]
    fn active_error_can_finish_its_hide_timer() {
        assert!(pill_hide_allowed(true, PillState::Error));
    }

    #[test]
    fn stale_hide_timer_cannot_hide_a_new_recording() {
        assert!(!pill_hide_allowed(true, PillState::Recording));
        assert!(!pill_hide_allowed(true, PillState::Transcribing));
    }
}
