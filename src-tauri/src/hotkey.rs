use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

/// Active mode: 0 = idle, 1 = recording (the hotkey is held).
pub const MODE_IDLE: u8 = 0;
pub const MODE_TRANSCRIBE: u8 = 1;

pub struct HotkeyState {
    pub active_mode: Arc<AtomicU8>,
    /// Used by the recording_loop to track whether it currently has the
    /// recorder open. The hook does NOT consult this any more — see the
    /// `trigger_held` field for that — because checking it from the
    /// hook caused a stuck state when the user tapped the hotkey during
    /// the previous transcription (the new press set `is_recording=true`
    /// but recording_loop was still in the previous iteration's
    /// transcribe step, so when it eventually looped back and saw
    /// `mode=IDLE` the flag stayed true and every subsequent hotkey
    /// press was ignored). The state-watchdog now relies on it as a
    /// last resort but the trigger_held flag means it should rarely
    /// fire.
    pub is_recording: Arc<AtomicBool>,
}

impl HotkeyState {
    pub fn new() -> Self {
        Self {
            active_mode: Arc::new(AtomicU8::new(MODE_IDLE)),
            is_recording: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for HotkeyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Parsed hotkey definition. The combo's last token is the trigger key
/// (whose KEYDOWN starts recording / KEYUP stops it). All earlier tokens
/// must be held at trigger time.
#[derive(Debug, Clone, Default)]
pub struct HotkeyDef {
    /// Virtual-key codes that must be held (use the `_LEFT`/`_RIGHT`-agnostic
    /// VK constants, e.g. VK_CONTROL, so GetAsyncKeyState matches either).
    pub modifier_vks: Vec<u16>,
    /// Virtual-key codes that fire the trigger (any of them counts).
    pub trigger_vks: Vec<u16>,
    /// Original combo string, kept so the UI / config can read it back.
    pub combo: String,
}

impl HotkeyDef {
    /// Parse "Ctrl+Win" / "Ctrl+Alt+Space" / "Ctrl+Shift+K" etc.
    pub fn parse(combo: &str) -> Result<Self, String> {
        let parts: Vec<&str> = combo
            .split('+')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err("empty hotkey combo".into());
        }
        let trigger_token = *parts.last().unwrap();
        let modifier_tokens = &parts[..parts.len() - 1];

        let mut modifier_vks = Vec::new();
        for m in modifier_tokens {
            modifier_vks.extend(
                modifier_vk_codes(m)
                    .ok_or_else(|| format!("unknown modifier '{}' in '{}'", m, combo))?,
            );
        }

        let trigger_vks = trigger_vk_codes(trigger_token)
            .ok_or_else(|| format!("unknown key '{}' in '{}'", trigger_token, combo))?;

        Ok(Self {
            modifier_vks,
            trigger_vks,
            combo: combo.to_string(),
        })
    }
}

#[allow(dead_code)] // Linux build doesn't use these but the parser needs the table
fn modifier_vk_codes(token: &str) -> Option<Vec<u16>> {
    // Generic tokens use the "either left or right" composite VK
    // (VK_CONTROL = 0x11 etc.) — GetAsyncKeyState handles both sides.
    // L/R-specific tokens map to their side-specific VK so a binding like
    // "RCtrl" can mean "right ctrl only".
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(vec![0x11]),
        "lctrl" => Some(vec![0xA2]),
        "rctrl" => Some(vec![0xA3]),
        "alt" | "menu" => Some(vec![0x12]),
        "lalt" => Some(vec![0xA4]),
        "ralt" => Some(vec![0xA5]),
        "shift" => Some(vec![0x10]),
        "lshift" => Some(vec![0xA0]),
        "rshift" => Some(vec![0xA1]),
        "win" | "meta" | "super" | "cmd" => Some(vec![0x5B, 0x5C]),
        "lwin" => Some(vec![0x5B]),
        "rwin" => Some(vec![0x5C]),
        _ => None,
    }
}

#[allow(dead_code)]
fn trigger_vk_codes(token: &str) -> Option<Vec<u16>> {
    // Trigger keys can be any modifier OR a regular key — we normalise here.
    // The low-level keyboard hook reports L/R-specific VKs in
    // KBDLLHOOKSTRUCT.vkCode, so for the *generic* names we expand to both
    // sides; for the side-specific names we match exactly one VK.
    if let Some(mods) = modifier_vk_codes(token) {
        return Some(match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => vec![0xA2, 0xA3],
            "lctrl" => vec![0xA2],
            "rctrl" => vec![0xA3],
            "alt" | "menu" => vec![0xA4, 0xA5],
            "lalt" => vec![0xA4],
            "ralt" => vec![0xA5],
            "shift" => vec![0xA0, 0xA1],
            "lshift" => vec![0xA0],
            "rshift" => vec![0xA1],
            _ => mods, // win/lwin/rwin already correct
        });
    }
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "space" => Some(vec![0x20]),
        "enter" | "return" => Some(vec![0x0D]),
        "tab" => Some(vec![0x09]),
        "esc" | "escape" => Some(vec![0x1B]),
        "backspace" => Some(vec![0x08]),
        "delete" | "del" => Some(vec![0x2E]),
        s if s.len() == 1 => {
            let c = s.chars().next().unwrap().to_ascii_uppercase();
            if c.is_ascii_alphanumeric() {
                Some(vec![c as u16])
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Opaque guard — drop stops the hook/grab
pub struct HookGuard {
    #[cfg(target_os = "windows")]
    _handle: std::thread::JoinHandle<()>,
    #[cfg(target_os = "linux")]
    _handle: std::thread::JoinHandle<()>,
    // macOS keeps the tap alive inside the dedicated CFRunLoop thread.
    // The thread leaks intentionally for the lifetime of the process;
    // recording_loop has no clean shutdown path either.
    #[cfg(target_os = "macos")]
    _phantom: (),
}

// ─── Windows ────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::sync::{OnceLock, RwLock};
    use windows::Win32::Foundation::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// Set once on `install_hook`. `OnceLock` instead of `static mut` to
    /// avoid the rust-2024 UB warnings around shared references to
    /// mutable statics.
    static HOOK_STATE: OnceLock<HotkeyState> = OnceLock::new();
    /// Suppresses Windows' KEYDOWN auto-repeat: once a trigger key is
    /// down we ignore further KEYDOWN events for it until we see the
    /// matching KEYUP. Lives only in the hook, separate from
    /// `is_recording` so the recording_loop's progress can't desync the
    /// hook's idea of "is the key physically held right now".
    static TRIGGER_HELD: AtomicBool = AtomicBool::new(false);
    /// Active hotkey definition. `RwLock` (not `Mutex`) so the hook
    /// callback can `try_read` without ever blocking — the UI command
    /// that swaps the hotkey holds the write lock for microseconds, but
    /// in a low-level keyboard hook even *microseconds* of waiting can
    /// trip Windows' `LowLevelHooksTimeout` and get the whole hook
    /// silently uninstalled. `try_read` falls through to the cached
    /// `CallNextHookEx` instead of waiting.
    static HOOK_DEF: RwLock<Option<HotkeyDef>> = RwLock::new(None);

    /// `KBDLLHOOKSTRUCT.flags` bit set when the event was synthesised by
    /// `SendInput` rather than typed by a human. We must filter these:
    /// every paste fires Ctrl-down/V-down/V-up/Ctrl-up via SendInput,
    /// and if those traversed our trigger logic we'd both (a) waste
    /// hook callback time on every paste and (b) risk re-arming the
    /// recorder mid-paste when somebody binds Ctrl as the trigger.
    const LLKHF_INJECTED: u32 = 0x10;

    pub fn set_hotkey_def(def: HotkeyDef) {
        if let Ok(mut g) = HOOK_DEF.write() {
            *g = Some(def);
        }
    }

    fn modifier_held(vk: u16) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // Hot path — keep it short. Anything that can wait or allocate
        // belongs OUTSIDE this function, otherwise Windows will quietly
        // disable the hook on the first slow callback.
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let kb = *(lparam.0 as *const KBDLLHOOKSTRUCT);

        // Skip events we generated ourselves (paste's synthetic Ctrl+V).
        if (kb.flags.0 & LLKHF_INJECTED) != 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // Don't block waiting on the writer — fall through cleanly if
        // the UI is currently swapping the hotkey definition.
        let def_guard = match HOOK_DEF.try_read() {
            Ok(g) => g,
            Err(_) => return CallNextHookEx(None, code, wparam, lparam),
        };
        let Some(def) = def_guard.as_ref() else {
            return CallNextHookEx(None, code, wparam, lparam);
        };
        let Some(state) = HOOK_STATE.get() else {
            return CallNextHookEx(None, code, wparam, lparam);
        };

        let vk = kb.vkCode as u16;
        let evt = wparam.0 as u32;
        let is_trigger = def.trigger_vks.contains(&vk);
        if is_trigger {
            match evt {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    // `TRIGGER_HELD` collapses Windows' KEYDOWN auto-repeat
                    // (held key = ~30 KEYDOWNs/s) into a single arming
                    // event. We deliberately do NOT consult
                    // `is_recording` here: that flag belongs to the
                    // recording_loop, and using it as a hook gate caused
                    // a wedge when a press during the previous
                    // transcribe left it stuck true forever.
                    if !TRIGGER_HELD.load(Ordering::SeqCst) {
                        let all_held = def.modifier_vks.iter().all(|&m| modifier_held(m));
                        if all_held {
                            TRIGGER_HELD.store(true, Ordering::SeqCst);
                            state.active_mode.store(MODE_TRANSCRIBE, Ordering::SeqCst);
                            // Swallow so e.g. Win doesn't open Start menu,
                            // Space doesn't insert a space, etc.
                            return LRESULT(1);
                        }
                    } else {
                        // Auto-repeat for an already-held trigger — still
                        // swallow so the keystroke doesn't reach the
                        // foreground app (otherwise repeating Space would
                        // type spaces while the user is dictating).
                        return LRESULT(1);
                    }
                }
                WM_KEYUP | WM_SYSKEYUP if TRIGGER_HELD.load(Ordering::SeqCst) => {
                    TRIGGER_HELD.store(false, Ordering::SeqCst);
                    state.active_mode.store(MODE_IDLE, Ordering::SeqCst);
                }
                _ => {}
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    /// Install the hook on a dedicated thread that does nothing but pump
    /// messages.
    ///
    /// Why a dedicated thread: Windows enforces `LowLevelHooksTimeout`
    /// (~300ms by default) and silently uninstalls any LL hook whose
    /// owning thread is slow to dispatch — and "the owning thread" means
    /// the one that called `SetWindowsHookExW`, not the one running the
    /// callback. If the hook lived on the main thread, any spike on it
    /// (Tauri command handler, blocked .lock(), GC-like pause from
    /// transcribe activity on other cores) could let Windows decide the
    /// hook is unresponsive and quietly nuke it. The user then sees the
    /// hotkey "just stop working" with no error.
    ///
    /// On its own thread with a tight `GetMessage` loop, the hook
    /// thread is always responsive, regardless of what the rest of the
    /// app is doing. This is the standard fix for LL hooks on Windows.
    pub fn install_hook(state: HotkeyState) -> anyhow::Result<HookGuard> {
        use std::sync::mpsc;

        let _ = HOOK_STATE.set(state);

        let (tx, rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let handle = std::thread::Builder::new()
            .name("flov-keyhook".into())
            .spawn(move || unsafe {
                let mut hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)
                {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let _ = tx.send(Ok(()));
                tracing::info!("keyboard hook installed on dedicated thread");

                // Self-healing watchdog: re-install the hook every 30s.
                // Empirically Windows quietly disables LL hooks under
                // certain conditions (reportedly Sleep/Wake, certain
                // anti-cheat/security drivers, long idles, the timeout
                // heuristic firing once for any reason). The cost of a
                // periodic reinstall is microseconds, and it guarantees
                // that the hook is always alive instead of relying on
                // the user to notice and restart the app.
                //
                // SetTimer with NULL hWnd posts WM_TIMER to this
                // thread's message queue every `REINSTALL_MS` ms.
                const REINSTALL_MS: u32 = 30_000;
                let timer_id = SetTimer(None, 0, REINSTALL_MS, None);
                if timer_id == 0 {
                    tracing::warn!(
                        "SetTimer for hook watchdog failed — running without periodic reinstall"
                    );
                }

                let mut msg = MSG::default();
                loop {
                    let r = GetMessageW(&mut msg, None, 0, 0);
                    if r.0 <= 0 {
                        break;
                    }

                    if msg.message == WM_TIMER {
                        let _ = UnhookWindowsHookEx(hook);
                        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
                            Ok(h) => {
                                hook = h;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "hook reinstall failed: {} — retrying next tick",
                                    e
                                );
                                // Fall back to a sentinel handle; next
                                // WM_TIMER will retry.
                                hook = windows::Win32::UI::WindowsAndMessaging::HHOOK::default();
                            }
                        }
                        continue;
                    }

                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                if timer_id != 0 {
                    let _ = KillTimer(None, timer_id);
                }
                let _ = UnhookWindowsHookEx(hook);
            })?;

        match rx.recv() {
            Ok(Ok(())) => Ok(HookGuard { _handle: handle }),
            Ok(Err(e)) => anyhow::bail!("SetWindowsHookExW failed: {}", e),
            Err(e) => anyhow::bail!("hook thread vanished: {}", e),
        }
    }
}

// ─── macOS ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos_impl {
    //! Global push-to-talk hook via a CoreGraphics event tap.
    //!
    //! The tap lives in a dedicated thread that runs its own CFRunLoop.
    //! When a configured trigger fires (with all required modifiers held)
    //! we flip `active_mode` to `MODE_TRANSCRIBE`. Releasing the trigger
    //! flips it back. The recording loop in `lib.rs` polls those flags
    //! exactly like on Windows / Linux.
    //!
    //! Suppression: the `core_graphics` 0.24 high-level wrapper can't
    //! return NULL from the tap callback (which is what suppresses an
    //! event in CoreGraphics). For combos that have a built-in macOS
    //! action (e.g. Cmd+Space → Spotlight) we'd need a raw FFI call to
    //! suppress. The default Cmd+Alt has no system action, so pass-
    //! through is acceptable for v1; revisit if users complain.
    //!
    //! Permission: the first time we install the tap the system prompts
    //! the user for Accessibility access (Settings → Privacy & Security
    //! → Accessibility). Without it `CGEventTapCreate` returns NULL.

    use super::*;
    use std::sync::atomic::AtomicPtr;
    use std::sync::{Once, OnceLock, RwLock};

    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
    use core_foundation::string::CFString;
    use core_graphics::event::{
        CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, EventField,
    };

    // Apple's AX API for checking + prompting Accessibility permission.
    // We can't go through the high-level `accessibility-sys` crate because
    // it pulls AppKit + adds an extra build step, so we link the symbols
    // straight from ApplicationServices.
    //
    // (Plain `//` comment, not `///`: rustdoc warns on doc comments
    // attached to extern blocks because it can't render them.)
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation::dictionary::CFDictionaryRef,
        ) -> bool;
    }

    // Raw FFI for re-enabling a tap. The high-level
    // `CGEventTap::enable(&self)` needs a wrapper instance, but we can
    // only get one at tap-construction time on the worker thread — the
    // callback is a `fn`, not a closure capturing the tap. Stashing the
    // raw CFMachPort and calling `CGEventTapEnable` directly is the
    // cleanest way to re-arm after `TapDisabledBy{Timeout,UserInput}`.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapEnable(tap: core_foundation::mach_port::CFMachPortRef, enable: bool);
    }

    /// Returns true if the process is already trusted for Accessibility.
    /// `prompt=false` so we never trigger macOS's broken auto-prompt
    /// (see `install_hook` for the rationale).
    fn accessibility_check(prompt: bool) -> bool {
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = if prompt {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        let dict = CFDictionary::from_CFType_pairs(&[(key, value)]);
        unsafe { AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef()) }
    }

    /// Walk up from the running executable to the `.app` bundle root
    /// (`Foo.app`), or return the executable path itself if we're a
    /// loose binary (e.g. `cargo run`).
    ///
    /// macOS's TCC Accessibility list resolves entries by bundle path
    /// for `.app`s — pointing the user at the bundle and not at
    /// `Contents/MacOS/flov_app` is the difference between the toggle
    /// actually taking effect and it silently no-op'ing.
    fn flov_app_bundle_path() -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap_or_default();
        // `<bundle>.app/Contents/MacOS/<exe>` → climb three parents to
        // reach `<bundle>.app`.
        let bundle = exe
            .ancestors()
            .find(|p| p.extension().map(|e| e == "app").unwrap_or(false));
        bundle.map(|p| p.to_path_buf()).unwrap_or(exe)
    }

    /// Pop the Accessibility pane of System Settings via the standard
    /// URL scheme. Same panel macOS itself opens from the (broken)
    /// auto-prompt's button, just without the misleading entry.
    fn open_accessibility_settings() {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }

    /// Carbon virtual key codes referenced from
    /// `<HIToolbox/Events.h>`. The `core_graphics::event::KeyCode`
    /// constants cover modifiers + a few special keys but stop short of
    /// the ANSI letter/digit table, so we keep the full list here.
    mod vk {
        pub const COMMAND: u16 = 0x37;
        pub const RIGHT_COMMAND: u16 = 0x36;
        pub const SHIFT: u16 = 0x38;
        pub const RIGHT_SHIFT: u16 = 0x3C;
        pub const OPTION: u16 = 0x3A;
        pub const RIGHT_OPTION: u16 = 0x3D;
        pub const CONTROL: u16 = 0x3B;
        pub const RIGHT_CONTROL: u16 = 0x3E;

        pub const RETURN: u16 = 0x24;
        pub const TAB: u16 = 0x30;
        pub const SPACE: u16 = 0x31;
        pub const DELETE: u16 = 0x33;
        pub const FORWARD_DELETE: u16 = 0x75;
        pub const ESCAPE: u16 = 0x35;
    }

    /// Parsed combo in macOS terms — produced from `HotkeyDef.combo`.
    #[derive(Debug, Clone, Default)]
    struct MacCombo {
        required_flags: CGEventFlags,
        trigger_keycodes: Vec<u16>,
        /// True iff the trigger token is itself a modifier (e.g. binding
        /// the right Option key as a single-key push-to-talk). When set,
        /// we react on FlagsChanged instead of KeyDown / KeyUp.
        trigger_is_modifier: bool,
        /// Which flag bit toggles when the trigger fires. Used to decide
        /// press vs release in FlagsChanged.
        trigger_flag: CGEventFlags,
    }

    fn token_modifier_flag(tok: &str) -> Option<CGEventFlags> {
        Some(match tok.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "lctrl" | "rctrl" => CGEventFlags::CGEventFlagControl,
            "alt" | "menu" | "option" | "lalt" | "ralt" => CGEventFlags::CGEventFlagAlternate,
            "shift" | "lshift" | "rshift" => CGEventFlags::CGEventFlagShift,
            "win" | "meta" | "super" | "cmd" | "command" | "lwin" | "rwin" => {
                CGEventFlags::CGEventFlagCommand
            }
            _ => return None,
        })
    }

    fn token_modifier_keycodes(tok: &str) -> Option<Vec<u16>> {
        Some(match tok.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => vec![vk::CONTROL, vk::RIGHT_CONTROL],
            "lctrl" => vec![vk::CONTROL],
            "rctrl" => vec![vk::RIGHT_CONTROL],
            "alt" | "menu" | "option" => vec![vk::OPTION, vk::RIGHT_OPTION],
            "lalt" => vec![vk::OPTION],
            "ralt" => vec![vk::RIGHT_OPTION],
            "shift" => vec![vk::SHIFT, vk::RIGHT_SHIFT],
            "lshift" => vec![vk::SHIFT],
            "rshift" => vec![vk::RIGHT_SHIFT],
            "win" | "meta" | "super" | "cmd" | "command" => {
                vec![vk::COMMAND, vk::RIGHT_COMMAND]
            }
            "lwin" => vec![vk::COMMAND],
            "rwin" => vec![vk::RIGHT_COMMAND],
            _ => return None,
        })
    }

    /// ANSI letter / digit → Carbon virtual key code. Lifted from
    /// `Events.h`. Layout-independent — these are physical key
    /// positions, not the character produced by the user's layout, which
    /// matches how the Windows hook works (`VK_A` is also the physical
    /// A key position, regardless of dvorak / etc).
    fn ansi_keycode(c: char) -> Option<u16> {
        Some(match c.to_ascii_lowercase() {
            'a' => 0x00,
            'b' => 0x0B,
            'c' => 0x08,
            'd' => 0x02,
            'e' => 0x0E,
            'f' => 0x03,
            'g' => 0x05,
            'h' => 0x04,
            'i' => 0x22,
            'j' => 0x26,
            'k' => 0x28,
            'l' => 0x25,
            'm' => 0x2E,
            'n' => 0x2D,
            'o' => 0x1F,
            'p' => 0x23,
            'q' => 0x0C,
            'r' => 0x0F,
            's' => 0x01,
            't' => 0x11,
            'u' => 0x20,
            'v' => 0x09,
            'w' => 0x0D,
            'x' => 0x07,
            'y' => 0x10,
            'z' => 0x06,
            '0' => 0x1D,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '5' => 0x17,
            '6' => 0x16,
            '7' => 0x1A,
            '8' => 0x1C,
            '9' => 0x19,
            _ => return None,
        })
    }

    fn token_trigger(tok: &str) -> Option<(Vec<u16>, Option<CGEventFlags>)> {
        if let Some(codes) = token_modifier_keycodes(tok) {
            let flag = token_modifier_flag(tok).unwrap_or(CGEventFlags::CGEventFlagNull);
            return Some((codes, Some(flag)));
        }
        let lower = tok.to_ascii_lowercase();
        let code = match lower.as_str() {
            "space" => vk::SPACE,
            "enter" | "return" => vk::RETURN,
            "tab" => vk::TAB,
            "esc" | "escape" => vk::ESCAPE,
            "backspace" => vk::DELETE,
            "delete" | "del" => vk::FORWARD_DELETE,
            s if s.len() == 1 => {
                let c = s.chars().next().unwrap();
                ansi_keycode(c)?
            }
            _ => return None,
        };
        Some((vec![code], None))
    }

    impl MacCombo {
        fn from_combo(combo: &str) -> Option<Self> {
            let parts: Vec<&str> = combo
                .split('+')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            let trigger_token = *parts.last().unwrap();
            let modifier_tokens = &parts[..parts.len() - 1];

            let mut required_flags = CGEventFlags::CGEventFlagNull;
            for m in modifier_tokens {
                required_flags |= token_modifier_flag(m)?;
            }
            let (trigger_keycodes, trigger_flag) = token_trigger(trigger_token)?;
            let trigger_is_modifier = trigger_flag.is_some();
            Some(MacCombo {
                required_flags,
                trigger_keycodes,
                trigger_is_modifier,
                trigger_flag: trigger_flag.unwrap_or(CGEventFlags::CGEventFlagNull),
            })
        }
    }

    static HOOK_STATE: OnceLock<HotkeyState> = OnceLock::new();
    /// Active hotkey definition (Mac-specific encoding). `RwLock` so the
    /// tap callback can `try_read` without blocking — same pattern as
    /// the Windows hook (see comment on `HOOK_DEF` there). CGEventTap's
    /// timeout is forgiving compared to LL hooks, but the cost is the
    /// same one-liner.
    static MAC_COMBO: RwLock<Option<MacCombo>> = RwLock::new(None);
    /// Collapses press/release tracking inside the tap callback,
    /// independent of `state.is_recording` (which belongs to the
    /// recording_loop; see the Windows-side `TRIGGER_HELD` comment for
    /// the wedge this prevents).
    static TRIGGER_HELD: AtomicBool = AtomicBool::new(false);
    /// Raw `CFMachPortRef` to the tap, stashed once we've successfully
    /// created it. Used by the callback to re-arm via `CGEventTapEnable`
    /// when macOS hits us with `TapDisabledByTimeout`. `AtomicPtr` so
    /// we don't need a lock in the hot path.
    static TAP_PORT: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
    static INSTALLED: Once = Once::new();

    pub fn set_hotkey_def(def: HotkeyDef) {
        let mac = MacCombo::from_combo(&def.combo);
        if mac.is_none() {
            tracing::warn!("hotkey '{}' has no macOS mapping", def.combo);
        }
        if let Ok(mut g) = MAC_COMBO.write() {
            *g = mac;
        }
    }

    pub fn install_hook(state: HotkeyState) -> anyhow::Result<HookGuard> {
        let _ = HOOK_STATE.set(state);

        // We deliberately do NOT pass `prompt=true` here. macOS's
        // auto-prompt has a long-standing bug (still present on Sonoma
        // 14.x / Sequoia 15.x) where, for an unsigned .app bundle, the
        // resulting TCC entry points at the executable file
        // (.../Contents/MacOS/flov_app) instead of the bundle, so even
        // after the user flips the toggle on, AXIsProcessTrusted keeps
        // returning false and the tap stays dead.
        //
        // The reliable path is: tell the user what's missing, open
        // System Settings on the right pane, and let them drag the
        // .app in manually via the "+" button — TCC then registers the
        // bundle path/id correctly and toggling the entry sticks.
        let trusted = accessibility_check(false);
        if !trusted {
            tracing::warn!(
                "Accessibility permission not granted. Opening System Settings — \
                 add this .app manually via '+' (do NOT trust the auto-prompt's \
                 entry, it points at the wrong path on unsigned builds): {}",
                flov_app_bundle_path().display()
            );
            open_accessibility_settings();
        }

        // The tap is installed exactly once — re-binding the combo just
        // updates MAC_COMBO, which the live callback re-reads on every
        // event. We still spawn the thread even without permission so the
        // log explains *why* nothing happens; CGEventTapCreate returns
        // NULL inside and we log the same hint.
        INSTALLED.call_once(|| {
            std::thread::Builder::new()
                .name("flov-hotkey-tap".into())
                .spawn(tap_thread)
                .expect("spawn hotkey tap thread");
        });

        Ok(HookGuard { _phantom: () })
    }

    fn tap_thread() {
        let tap = match CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![
                CGEventType::KeyDown,
                CGEventType::KeyUp,
                CGEventType::FlagsChanged,
            ],
            tap_callback,
        ) {
            Ok(t) => t,
            Err(_) => {
                tracing::error!(
                    "CGEventTapCreate failed — flov needs Accessibility \
                     permission. Open System Settings → Privacy & Security \
                     → Accessibility and enable flov."
                );
                return;
            }
        };

        // Stash the raw mach-port so the callback can re-enable the tap
        // after macOS suspends it (TapDisabledByTimeout fires when the
        // callback is too slow, TapDisabledByUserInput on every Cmd+Tab
        // or password-prompt-like UI). Both deactivate the tap until
        // we explicitly re-enable it.
        TAP_PORT.store(
            tap.mach_port.as_concrete_TypeRef() as *mut _,
            Ordering::SeqCst,
        );

        unsafe {
            let source = match tap.mach_port.create_runloop_source(0) {
                Ok(s) => s,
                Err(_) => {
                    tracing::error!("create_runloop_source failed");
                    return;
                }
            };
            CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
        }
        tap.enable();
        tracing::info!("macOS hotkey tap installed");
        CFRunLoop::run_current();
    }

    fn tap_callback(
        _proxy: core_graphics::event::CGEventTapProxy,
        etype: CGEventType,
        event: &core_graphics::event::CGEvent,
    ) -> Option<core_graphics::event::CGEvent> {
        // Self-healing: the OS hands these two pseudo-events to a tap
        // when it deactivates it. Re-enable so the hotkey keeps working
        // through long sessions, Cmd+Tabs, security UIs, etc. We swallow
        // them either way (returning `None` passes the original event
        // through, which is fine — they're synthetic and have no payload
        // an app could react to).
        match etype {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                let port =
                    TAP_PORT.load(Ordering::SeqCst) as core_foundation::mach_port::CFMachPortRef;
                if !port.is_null() {
                    unsafe { CGEventTapEnable(port, true) };
                    tracing::warn!("CGEventTap re-enabled after {:?}", etype);
                }
                return None;
            }
            _ => {}
        }

        let state = match HOOK_STATE.get() {
            Some(s) => s,
            None => return None,
        };
        // `try_read`: if the UI is mid-swap of the combo, fall through
        // to pass-through rather than wait. CGEventTap is more tolerant
        // than Windows' LL hook but the principle is the same — never
        // block the hot path.
        let combo = {
            let guard = match MAC_COMBO.try_read() {
                Ok(g) => g,
                Err(_) => return None,
            };
            match guard.as_ref() {
                Some(c) => c.clone(),
                None => return None,
            }
        };
        let vk = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
        let flags = event.get_flags();

        match (etype, combo.trigger_is_modifier) {
            // Non-modifier trigger (e.g. Cmd+Alt+Space).
            (CGEventType::KeyDown, false) => {
                if combo.trigger_keycodes.contains(&vk) && flags.contains(combo.required_flags) {
                    // `TRIGGER_HELD` collapses macOS's KEYDOWN auto-repeat
                    // into a single arm event — same as the Windows hook.
                    // We deliberately do NOT consult `state.is_recording`
                    // here: recording_loop manages that flag and using it
                    // as a gate caused a wedge on Windows (see Gotchas
                    // section of CLAUDE.md). Same trap on macOS.
                    if !TRIGGER_HELD.load(Ordering::SeqCst) {
                        TRIGGER_HELD.store(true, Ordering::SeqCst);
                        state.active_mode.store(MODE_TRANSCRIBE, Ordering::SeqCst);
                    }
                }
            }
            (CGEventType::KeyUp, false) => {
                if combo.trigger_keycodes.contains(&vk) && TRIGGER_HELD.load(Ordering::SeqCst) {
                    TRIGGER_HELD.store(false, Ordering::SeqCst);
                    state.active_mode.store(MODE_IDLE, Ordering::SeqCst);
                }
            }

            // Modifier trigger (e.g. Cmd+Alt, where the trigger token
            // "Alt"/Option is itself a modifier — the press is detected
            // via FlagsChanged + the bit transitioning from clear to set).
            (CGEventType::FlagsChanged, true) => {
                if !combo.trigger_keycodes.contains(&vk) {
                    return None;
                }
                let pressed = flags.contains(combo.trigger_flag);
                // required_flags excludes the trigger flag itself —
                // we strip it before checking because at press time
                // it's about to become set.
                let other_required = combo.required_flags & !combo.trigger_flag;
                let held = TRIGGER_HELD.load(Ordering::SeqCst);
                if pressed && !held && flags.contains(other_required) {
                    TRIGGER_HELD.store(true, Ordering::SeqCst);
                    state.active_mode.store(MODE_TRANSCRIBE, Ordering::SeqCst);
                } else if !pressed && held {
                    TRIGGER_HELD.store(false, Ordering::SeqCst);
                    state.active_mode.store(MODE_IDLE, Ordering::SeqCst);
                }
            }
            _ => {}
        }
        None
    }
}

// ─── Linux ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use evdev::{Device, InputEventKind, Key};
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU16;
    use std::sync::{Mutex, OnceLock, RwLock};

    #[derive(Debug, Clone)]
    struct LinuxCombo {
        /// Each inner vector contains equivalent left/right alternatives.
        required: Vec<Vec<Key>>,
        triggers: Vec<Key>,
        label: String,
    }

    impl LinuxCombo {
        fn from_combo(combo: &str) -> Option<Self> {
            let parts: Vec<&str> = combo
                .split('+')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect();
            let (trigger, modifiers) = parts.split_last()?;
            let required = modifiers
                .iter()
                .map(|token| modifier_keys(token))
                .collect::<Option<Vec<_>>>()?;
            let triggers = trigger_keys(trigger)?;
            Some(Self {
                required,
                triggers,
                label: combo.to_string(),
            })
        }
    }

    #[derive(Default)]
    struct PressedKeys {
        counts: HashMap<Key, usize>,
    }

    impl PressedKeys {
        fn press(&mut self, key: Key) {
            *self.counts.entry(key).or_default() += 1;
        }

        fn release(&mut self, key: Key) {
            if let Some(count) = self.counts.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.counts.remove(&key);
                }
            }
        }

        fn is_held(&self, key: Key) -> bool {
            self.counts.get(&key).copied().unwrap_or(0) != 0
        }
    }

    static HOOK_STATE: OnceLock<HotkeyState> = OnceLock::new();
    static LINUX_COMBO: RwLock<Option<LinuxCombo>> = RwLock::new(None);
    static TRIGGER_HELD: AtomicBool = AtomicBool::new(false);
    static ACTIVE_TRIGGER: AtomicU16 = AtomicU16::new(0);

    fn modifier_keys(token: &str) -> Option<Vec<Key>> {
        Some(match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => vec![Key::KEY_LEFTCTRL, Key::KEY_RIGHTCTRL],
            "lctrl" => vec![Key::KEY_LEFTCTRL],
            "rctrl" => vec![Key::KEY_RIGHTCTRL],
            "alt" | "menu" | "option" => vec![Key::KEY_LEFTALT, Key::KEY_RIGHTALT],
            "lalt" => vec![Key::KEY_LEFTALT],
            "ralt" => vec![Key::KEY_RIGHTALT],
            "shift" => vec![Key::KEY_LEFTSHIFT, Key::KEY_RIGHTSHIFT],
            "lshift" => vec![Key::KEY_LEFTSHIFT],
            "rshift" => vec![Key::KEY_RIGHTSHIFT],
            "win" | "meta" | "super" | "cmd" | "command" => {
                vec![Key::KEY_LEFTMETA, Key::KEY_RIGHTMETA]
            }
            "lwin" => vec![Key::KEY_LEFTMETA],
            "rwin" => vec![Key::KEY_RIGHTMETA],
            _ => return None,
        })
    }

    fn trigger_keys(token: &str) -> Option<Vec<Key>> {
        if let Some(keys) = modifier_keys(token) {
            return Some(keys);
        }
        let lower = token.to_ascii_lowercase();
        let key = match lower.as_str() {
            "space" => Key::KEY_SPACE,
            "enter" | "return" => Key::KEY_ENTER,
            "tab" => Key::KEY_TAB,
            "esc" | "escape" => Key::KEY_ESC,
            "backspace" => Key::KEY_BACKSPACE,
            "delete" | "del" => Key::KEY_DELETE,
            "a" => Key::KEY_A,
            "b" => Key::KEY_B,
            "c" => Key::KEY_C,
            "d" => Key::KEY_D,
            "e" => Key::KEY_E,
            "f" => Key::KEY_F,
            "g" => Key::KEY_G,
            "h" => Key::KEY_H,
            "i" => Key::KEY_I,
            "j" => Key::KEY_J,
            "k" => Key::KEY_K,
            "l" => Key::KEY_L,
            "m" => Key::KEY_M,
            "n" => Key::KEY_N,
            "o" => Key::KEY_O,
            "p" => Key::KEY_P,
            "q" => Key::KEY_Q,
            "r" => Key::KEY_R,
            "s" => Key::KEY_S,
            "t" => Key::KEY_T,
            "u" => Key::KEY_U,
            "v" => Key::KEY_V,
            "w" => Key::KEY_W,
            "x" => Key::KEY_X,
            "y" => Key::KEY_Y,
            "z" => Key::KEY_Z,
            "0" => Key::KEY_0,
            "1" => Key::KEY_1,
            "2" => Key::KEY_2,
            "3" => Key::KEY_3,
            "4" => Key::KEY_4,
            "5" => Key::KEY_5,
            "6" => Key::KEY_6,
            "7" => Key::KEY_7,
            "8" => Key::KEY_8,
            "9" => Key::KEY_9,
            _ => return None,
        };
        Some(vec![key])
    }

    fn find_keyboards(skip: &HashSet<PathBuf>) -> Vec<(PathBuf, Device)> {
        let mut keyboards = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                if skip.contains(&path) {
                    continue;
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with("event") {
                        continue;
                    }
                }
                if let Ok(dev) = Device::open(&path) {
                    if dev
                        .supported_keys()
                        .is_some_and(|keys| keys.contains(Key::KEY_A))
                    {
                        tracing::info!(
                            "Found keyboard: {} ({})",
                            dev.name().unwrap_or("?"),
                            path.display()
                        );
                        keyboards.push((path, dev));
                    }
                }
            }
        }
        keyboards
    }

    pub fn set_hotkey_def(def: HotkeyDef) {
        let combo = LinuxCombo::from_combo(&def.combo);
        if combo.is_none() {
            tracing::warn!("hotkey '{}' has no Linux evdev mapping", def.combo);
        }
        if let Ok(mut guard) = LINUX_COMBO.write() {
            *guard = combo;
        }
        if TRIGGER_HELD.swap(false, Ordering::SeqCst) {
            ACTIVE_TRIGGER.store(0, Ordering::SeqCst);
            if let Some(state) = HOOK_STATE.get() {
                state.active_mode.store(MODE_IDLE, Ordering::SeqCst);
            }
        }
    }

    pub fn install_hook(state: HotkeyState) -> anyhow::Result<HookGuard> {
        let empty = HashSet::new();
        let initial = find_keyboards(&empty);
        if initial.is_empty() {
            anyhow::bail!(
                "no readable keyboard devices in /dev/input; use Hyprland press/release binds \
                 or grant the user access through the 'input' group/udev"
            );
        }
        let _ = HOOK_STATE.set(state);

        let handle = std::thread::Builder::new()
            .name("flov-evdev-manager".into())
            .spawn(move || {
                let pressed = Arc::new(Mutex::new(PressedKeys::default()));
                let active_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));

                for (path, device) in initial {
                    spawn_keyboard_worker(path, device, pressed.clone(), active_paths.clone());
                }

                // Pick up hot-plugged keyboards and devices recreated after
                // suspend. Workers remove their path after an evdev error.
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let skip = active_paths.lock().unwrap().clone();
                    for (path, device) in find_keyboards(&skip) {
                        spawn_keyboard_worker(path, device, pressed.clone(), active_paths.clone());
                    }
                }
            })?;

        Ok(HookGuard { _handle: handle })
    }

    fn spawn_keyboard_worker(
        path: PathBuf,
        mut device: Device,
        pressed: Arc<Mutex<PressedKeys>>,
        active_paths: Arc<Mutex<HashSet<PathBuf>>>,
    ) {
        if !active_paths.lock().unwrap().insert(path.clone()) {
            return;
        }
        let name = device.name().unwrap_or("keyboard").to_string();
        let thread_path = path.clone();
        let worker_pressed = pressed.clone();
        let worker_paths = active_paths.clone();
        let spawn_result = std::thread::Builder::new()
            .name("flov-evdev-keyboard".into())
            .spawn(move || {
                let mut local_pressed = HashSet::<Key>::new();
                loop {
                    match device.fetch_events() {
                        Ok(events) => {
                            for event in events {
                                if let InputEventKind::Key(key) = event.kind() {
                                    handle_key_event(
                                        key,
                                        event.value(),
                                        &mut local_pressed,
                                        &worker_pressed,
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "evdev keyboard '{}' ({}) disconnected: {}",
                                name,
                                thread_path.display(),
                                e
                            );
                            break;
                        }
                    }
                }

                {
                    let mut shared = worker_pressed.lock().unwrap();
                    for key in &local_pressed {
                        shared.release(*key);
                    }
                }
                let active = ACTIVE_TRIGGER.load(Ordering::SeqCst);
                if local_pressed.iter().any(|key| key.code() == active)
                    && TRIGGER_HELD.swap(false, Ordering::SeqCst)
                {
                    ACTIVE_TRIGGER.store(0, Ordering::SeqCst);
                    if let Some(state) = HOOK_STATE.get() {
                        state.active_mode.store(MODE_IDLE, Ordering::SeqCst);
                    }
                }
                worker_paths.lock().unwrap().remove(&thread_path);
            });
        if let Err(e) = spawn_result {
            active_paths.lock().unwrap().remove(&path);
            tracing::error!("failed to spawn evdev worker for {}: {}", path.display(), e);
        }
    }

    fn handle_key_event(
        key: Key,
        value: i32,
        local_pressed: &mut HashSet<Key>,
        pressed: &Mutex<PressedKeys>,
    ) {
        if value == 1 && local_pressed.insert(key) {
            pressed.lock().unwrap().press(key);
        } else if value == 0 && local_pressed.remove(&key) {
            pressed.lock().unwrap().release(key);
        }

        if value == 0
            && ACTIVE_TRIGGER.load(Ordering::SeqCst) == key.code()
            && TRIGGER_HELD.swap(false, Ordering::SeqCst)
        {
            ACTIVE_TRIGGER.store(0, Ordering::SeqCst);
            if let Some(state) = HOOK_STATE.get() {
                state.active_mode.store(MODE_IDLE, Ordering::SeqCst);
            }
            return;
        }
        if value != 1 || TRIGGER_HELD.load(Ordering::SeqCst) {
            return;
        }

        let combo_guard = match LINUX_COMBO.read() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let Some(combo) = combo_guard.as_ref() else {
            return;
        };
        if !combo.triggers.contains(&key) {
            return;
        }
        let shared = pressed.lock().unwrap();
        let modifiers_held = combo
            .required
            .iter()
            .all(|alternatives| alternatives.iter().any(|key| shared.is_held(*key)));
        drop(shared);
        if modifiers_held {
            TRIGGER_HELD.store(true, Ordering::SeqCst);
            ACTIVE_TRIGGER.store(key.code(), Ordering::SeqCst);
            if let Some(state) = HOOK_STATE.get() {
                tracing::info!("Hotkey: {} (transcribe)", combo.label);
                state.active_mode.store(MODE_TRANSCRIBE, Ordering::SeqCst);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_configurable_ctrl_super_combo() {
            let combo = LinuxCombo::from_combo("Ctrl+Win").unwrap();

            assert_eq!(
                combo.required,
                vec![vec![Key::KEY_LEFTCTRL, Key::KEY_RIGHTCTRL]]
            );
            assert_eq!(combo.triggers, vec![Key::KEY_LEFTMETA, Key::KEY_RIGHTMETA]);
        }

        #[test]
        fn maps_right_ctrl_as_a_single_trigger() {
            let combo = LinuxCombo::from_combo("RCtrl").unwrap();

            assert!(combo.required.is_empty());
            assert_eq!(combo.triggers, vec![Key::KEY_RIGHTCTRL]);
        }
    }
}

// ─── Public API ─────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub use windows_impl::{install_hook, set_hotkey_def};

#[cfg(target_os = "macos")]
pub use macos_impl::{install_hook, set_hotkey_def};

#[cfg(target_os = "linux")]
pub use linux_impl::{install_hook, set_hotkey_def};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_ctrl_win_combo() {
        let def = HotkeyDef::parse("Ctrl+Win").unwrap();

        assert_eq!(def.modifier_vks, vec![0x11]);
        assert_eq!(def.trigger_vks, vec![0x5B, 0x5C]);
        assert_eq!(def.combo, "Ctrl+Win");
    }

    #[test]
    fn parses_regular_key_trigger() {
        let def = HotkeyDef::parse(" ctrl + alt + space ").unwrap();

        assert_eq!(def.modifier_vks, vec![0x11, 0x12]);
        assert_eq!(def.trigger_vks, vec![0x20]);
    }

    #[test]
    fn generic_modifier_trigger_expands_to_left_and_right_keys() {
        let def = HotkeyDef::parse("Ctrl").unwrap();

        assert!(def.modifier_vks.is_empty());
        assert_eq!(def.trigger_vks, vec![0xA2, 0xA3]);
    }

    #[test]
    fn side_specific_modifier_trigger_matches_single_key() {
        let def = HotkeyDef::parse("RCtrl").unwrap();

        assert!(def.modifier_vks.is_empty());
        assert_eq!(def.trigger_vks, vec![0xA3]);
    }

    #[test]
    fn rejects_unknown_tokens() {
        assert!(HotkeyDef::parse("Ctrl+Nope").is_err());
        assert!(HotkeyDef::parse("Nope+Space").is_err());
        assert!(HotkeyDef::parse("   ").is_err());
    }
}
