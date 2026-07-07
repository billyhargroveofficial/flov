//! Tauri UI glue: window positioning, click-through, and overlay lifecycle.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::Manager;

static FRONTEND_READY_EPOCH: AtomicU64 = AtomicU64::new(0);
static FRONTEND_RELOAD_STARTED_MS: AtomicU64 = AtomicU64::new(0);
static OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECORDING_CYCLE_ACTIVE: AtomicBool = AtomicBool::new(false);
static LAST_OVERLAY_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);
static PILL_STATE: AtomicU8 = AtomicU8::new(PillState::Idle as u8);
static PILL_EVENT_SEQ: AtomicU64 = AtomicU64::new(0);
static PILL_ERROR_TEXT: OnceLock<Mutex<String>> = OnceLock::new();

pub const FRONTEND_RELOAD_STALE_AFTER: Duration = Duration::from_secs(15);

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
    seq: u64,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PillEvent {
    state: PillState,
    seq: u64,
}

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PillErrorEvent {
    message: String,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadGateInput {
    pub is_recording: bool,
    pub hotkey_idle: bool,
    pub reload_in_progress: bool,
    pub overlay_active: bool,
    pub overlay_quiet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadSkipReason {
    Recording,
    HotkeyActive,
    ReloadInProgress,
    OverlayActive,
    OverlayNotQuiet,
}

impl ReloadSkipReason {
    pub fn log_message(self) -> &'static str {
        match self {
            Self::Recording => "recording in progress",
            Self::HotkeyActive => "hotkey mode active",
            Self::ReloadInProgress => "previous reload still pending",
            Self::OverlayActive => "pill logically active",
            Self::OverlayNotQuiet => "pill was active recently",
        }
    }
}

fn error_text() -> &'static Mutex<String> {
    PILL_ERROR_TEXT.get_or_init(|| Mutex::new(String::new()))
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

fn next_pill_seq() -> u64 {
    PILL_EVENT_SEQ.fetch_add(1, Ordering::SeqCst) + 1
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
    FRONTEND_RELOAD_STARTED_MS.store(0, Ordering::SeqCst);
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

pub fn reload_gate_decision(input: ReloadGateInput) -> std::result::Result<(), ReloadSkipReason> {
    if input.is_recording {
        return Err(ReloadSkipReason::Recording);
    }
    if !input.hotkey_idle {
        return Err(ReloadSkipReason::HotkeyActive);
    }
    if input.reload_in_progress {
        return Err(ReloadSkipReason::ReloadInProgress);
    }
    if input.overlay_active {
        return Err(ReloadSkipReason::OverlayActive);
    }
    if !input.overlay_quiet {
        return Err(ReloadSkipReason::OverlayNotQuiet);
    }
    Ok(())
}

pub fn activity_reload_gate_decision(
    input: ReloadGateInput,
) -> std::result::Result<(), ReloadSkipReason> {
    reload_gate_decision(ReloadGateInput {
        reload_in_progress: false,
        ..input
    })
}

pub fn try_mark_frontend_reload_started(stale_after: Duration) -> Option<u64> {
    recover_stale_frontend_reload(stale_after);
    let now = now_ms().max(1);
    FRONTEND_RELOAD_STARTED_MS
        .compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst)
        .ok()
        .map(|_| frontend_ready_epoch())
}

pub fn mark_frontend_reload_finished() {
    FRONTEND_RELOAD_STARTED_MS.store(0, Ordering::SeqCst);
}

pub fn frontend_reload_in_progress() -> bool {
    recover_stale_frontend_reload(FRONTEND_RELOAD_STALE_AFTER);
    FRONTEND_RELOAD_STARTED_MS.load(Ordering::SeqCst) != 0
}

pub fn recover_stale_frontend_reload(stale_after: Duration) -> bool {
    let started = FRONTEND_RELOAD_STARTED_MS.load(Ordering::SeqCst);
    if !reload_started_is_stale(started, now_ms(), stale_after) {
        return false;
    }
    FRONTEND_RELOAD_STARTED_MS
        .compare_exchange(started, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

fn reload_started_is_stale(started_ms: u64, now_ms: u64, stale_after: Duration) -> bool {
    started_ms != 0 && now_ms.saturating_sub(started_ms) >= stale_after.as_millis() as u64
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

pub fn set_pill_state(state: PillState) -> u64 {
    PILL_STATE.store(state as u8, Ordering::SeqCst);
    if !matches!(state, PillState::Error) {
        error_text().lock().unwrap().clear();
    }
    if !matches!(state, PillState::Idle) {
        set_overlay_active(true);
    } else {
        set_overlay_active(false);
    }
    next_pill_seq()
}

pub fn set_pill_error(message: &str) -> u64 {
    PILL_STATE.store(PillState::Error as u8, Ordering::SeqCst);
    *error_text().lock().unwrap() = message.to_string();
    set_overlay_active(true);
    next_pill_seq()
}

pub fn pill_snapshot() -> PillSnapshot {
    PillSnapshot {
        state: PillState::from_u8(PILL_STATE.load(Ordering::SeqCst)),
        error_text: error_text().lock().unwrap().clone(),
        seq: PILL_EVENT_SEQ.load(Ordering::SeqCst),
    }
}

pub fn pill_event(state: PillState, seq: u64) -> PillEvent {
    PillEvent { state, seq }
}

pub fn pill_error_event(message: &str, seq: u64) -> PillErrorEvent {
    PillErrorEvent {
        message: message.to_string(),
        seq,
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
    if recording_cycle_active() {
        tracing::info!("logical pill hide received while a recording cycle is active");
    } else {
        set_pill_state(PillState::Idle);
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
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_gate_allows_idle_quiet_state() {
        let input = ReloadGateInput {
            is_recording: false,
            hotkey_idle: true,
            reload_in_progress: false,
            overlay_active: false,
            overlay_quiet: true,
        };

        assert_eq!(reload_gate_decision(input), Ok(()));
    }

    #[test]
    fn reload_gate_rejects_activity_before_quiet_time() {
        let mut input = ReloadGateInput {
            is_recording: false,
            hotkey_idle: true,
            reload_in_progress: false,
            overlay_active: false,
            overlay_quiet: false,
        };
        assert_eq!(
            reload_gate_decision(input),
            Err(ReloadSkipReason::OverlayNotQuiet)
        );

        input.is_recording = true;
        assert_eq!(
            reload_gate_decision(input),
            Err(ReloadSkipReason::Recording)
        );
    }

    #[test]
    fn reload_started_stale_after_timeout() {
        assert!(!reload_started_is_stale(0, 100, Duration::from_millis(10)));
        assert!(!reload_started_is_stale(
            100,
            109,
            Duration::from_millis(10)
        ));
        assert!(reload_started_is_stale(100, 110, Duration::from_millis(10)));
    }
}
