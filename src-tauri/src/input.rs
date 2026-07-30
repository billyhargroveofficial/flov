// ─── Windows ────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use windows::Win32::Foundation::*;
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    pub fn type_text(text: &str) {
        if set_clipboard(text) {
            paste();
        }
    }

    pub fn get_clipboard() -> Option<String> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }

            // CF_UNICODETEXT = 13
            let handle = GetClipboardData(13);
            let result = if let Ok(handle) = handle {
                let ptr = GlobalLock(HGLOBAL(handle.0));
                if !ptr.is_null() {
                    let wide_ptr = ptr as *const u16;
                    let mut len = 0;
                    while *wide_ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(wide_ptr, len);
                    let text = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(HGLOBAL(handle.0));
                    Some(text)
                } else {
                    None
                }
            } else {
                None
            };

            let _ = CloseClipboard();
            result
        }
    }

    fn set_clipboard(text: &str) -> bool {
        unsafe {
            if OpenClipboard(None).is_err() {
                return false;
            }

            let _ = EmptyClipboard();

            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let size = wide.len() * 2;

            let hmem = GlobalAlloc(GMEM_MOVEABLE, size);
            if hmem.is_err() {
                let _ = CloseClipboard();
                return false;
            }
            let hmem = hmem.unwrap();

            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                let _ = GlobalFree(Some(hmem));
                let _ = CloseClipboard();
                return false;
            }

            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            let _ = GlobalUnlock(hmem);

            // CF_UNICODETEXT = 13
            let result = SetClipboardData(13, Some(HANDLE(hmem.0)));
            let _ = CloseClipboard();

            result.is_ok()
        }
    }

    fn paste() {
        let inputs = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_CONTROL,
                        wScan: 0,
                        dwFlags: KEYEVENTF_EXTENDEDKEY,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_V,
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(0),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_V,
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_CONTROL,
                        wScan: 0,
                        dwFlags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ];

        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

// ─── macOS ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos_impl {
    //! Paste = `arboard` clipboard set + synthetic Cmd+V via CGEventPost.
    //! Needs Accessibility permission (same TCC bucket as the hotkey
    //! event tap).

    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    /// kVK_ANSI_V from `<HIToolbox/Events.h>`.
    const KVK_ANSI_V: u16 = 0x09;

    pub fn get_clipboard() -> Option<String> {
        // `arboard::Clipboard::new` allocates an NSPasteboard handle each
        // call — fine for the once-per-transcription frequency.
        match arboard::Clipboard::new() {
            Ok(mut cb) => cb.get_text().ok(),
            Err(e) => {
                tracing::warn!("clipboard open failed: {}", e);
                None
            }
        }
    }

    pub fn type_text(text: &str) {
        if let Err(e) = set_clipboard_text(text) {
            tracing::error!("clipboard set failed: {:#}", e);
            return;
        }
        if let Err(e) = post_cmd_v() {
            tracing::error!(
                "Cmd+V post failed (likely missing Accessibility \
                 permission): {:#}",
                e
            );
        }
    }

    fn set_clipboard_text(text: &str) -> anyhow::Result<()> {
        let mut cb =
            arboard::Clipboard::new().map_err(|e| anyhow::anyhow!("Clipboard::new: {}", e))?;
        cb.set_text(text.to_string())
            .map_err(|e| anyhow::anyhow!("set_text: {}", e))
    }

    fn post_cmd_v() -> anyhow::Result<()> {
        // HIDSystemState = same level as a real keypress; combined-session
        // states drop modifier flags before delivery, which breaks Cmd+V.
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
        let down = CGEvent::new_keyboard_event(src.clone(), KVK_ANSI_V, true)
            .map_err(|_| anyhow::anyhow!("CGEvent::new_keyboard_event(down) failed"))?;
        down.set_flags(CGEventFlags::CGEventFlagCommand);
        down.post(CGEventTapLocation::HID);

        let up = CGEvent::new_keyboard_event(src, KVK_ANSI_V, false)
            .map_err(|_| anyhow::anyhow!("CGEvent::new_keyboard_event(up) failed"))?;
        up.set_flags(CGEventFlags::CGEventFlagCommand);
        up.post(CGEventTapLocation::HID);
        Ok(())
    }
}

// ─── Linux ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub fn get_clipboard() -> Option<String> {
        let output = Command::new("wl-paste").arg("--no-newline").output().ok()?;
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            None
        }
    }

    pub fn type_text(text: &str) {
        // Feed stdin rather than an argv value: transcripts beginning with
        // '-' must never be interpreted as wl-copy command-line options.
        if let Err(e) = set_clipboard(text) {
            tracing::error!("wl-copy failed: {:#} (install wl-clipboard)", e);
            return;
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
        match Command::new("wtype")
            .arg("-M")
            .arg("ctrl")
            .arg("-k")
            .arg("v")
            .arg("-m")
            .arg("ctrl")
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::error!("wtype exited with: {}", status),
            Err(e) => tracing::error!("wtype failed: {} (install wtype)", e),
        }
    }

    fn set_clipboard(text: &str) -> anyhow::Result<()> {
        let mut child = Command::new("wl-copy")
            .arg("--type")
            .arg("text/plain;charset=utf-8")
            .stdin(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("wl-copy stdin is unavailable"))?
            .write_all(text.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            anyhow::bail!("wl-copy exited with {status}");
        }
        Ok(())
    }
}

// ─── Public API ─────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub use windows_impl::{get_clipboard, type_text};

#[cfg(target_os = "macos")]
pub use macos_impl::{get_clipboard, type_text};

#[cfg(target_os = "linux")]
pub use linux_impl::{get_clipboard, type_text};
