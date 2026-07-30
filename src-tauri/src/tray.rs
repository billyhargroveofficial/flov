//! Tauri 2 native tray with Windows-theme-aware tray icon.
//!
//! - Single source PNG (`<exe_dir>/icons/tray.png`, dark glyph).
//! - On dark Windows theme we invert RGB so the glyph becomes light, keeping
//!   the alpha channel intact.
//! - We poll the theme registry key every few seconds to react to user theme
//!   switches without an extra Win32 message hook.
//! - State is conveyed via tooltip — pill UI is the primary signal.

use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::AppHandle;

pub const TRAY_ID: &str = "flov-tray";

#[derive(Clone, Copy)]
pub enum TrayState {
    Idle,
    Recording,
    Transcribing,
}

impl TrayState {
    fn tooltip(self) -> &'static str {
        match self {
            TrayState::Idle => "flov — hold the hotkey to dictate",
            TrayState::Recording => "flov — recording…",
            TrayState::Transcribing => "flov — transcribing…",
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_uses_dark_theme() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let subkey = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value_name = to_wide("SystemUsesLightTheme");

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let mut data: u32 = 1;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let mut kind = REG_VALUE_TYPE::default();
        let res = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut kind),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if res != ERROR_SUCCESS {
            return false;
        }
        // SystemUsesLightTheme: 1 = light, 0 = dark
        data == 0
    }
}

#[cfg(not(target_os = "windows"))]
fn windows_uses_dark_theme() -> bool {
    false
}

/// Returns the same icon used in the tray, themed to current Windows light/dark.
/// Reused as the Settings window's titlebar/taskbar icon for brand consistency.
pub fn load_themed_icon_for_window() -> Image<'static> {
    load_themed_icon(windows_uses_dark_theme())
}

/// PNG bytes baked into the binary at compile time. Earlier versions
/// looked the icon up next to the exe at runtime, but the bundle/install
/// step doesn't ship icons/tray.png, so packaged builds saw a fallback
/// solid square. Embedding sidesteps that entirely.
const TRAY_PNG: &[u8] = include_bytes!("../icons/tray.png");

/// Decodes the embedded tray PNG and (on dark theme) inverts its RGB
/// channels so the dark glyph reads as light. Falls back to a flat
/// themed square only if the PNG is somehow undecodeable.
fn load_themed_icon(dark_theme: bool) -> Image<'static> {
    if let Ok(img) = image::load_from_memory(TRAY_PNG) {
        let mut rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if dark_theme {
            for px in rgba.pixels_mut() {
                px[0] = 255 - px[0];
                px[1] = 255 - px[1];
                px[2] = 255 - px[2];
                // alpha untouched
            }
        }
        return Image::new_owned(rgba.into_raw(), w, h);
    }

    const SIZE: u32 = 32;
    let fill: [u8; 4] = if dark_theme {
        [0xF5, 0xF5, 0xF7, 0xFF]
    } else {
        [0x1C, 0x1C, 0x1E, 0xFF]
    };
    let mut data = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        data.extend_from_slice(&fill);
    }
    Image::new_owned(data, SIZE, SIZE)
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    // Tray menu is intentionally minimal: settings / stats / postprocess /
    // backend all live inside the Settings window now. Tray stays for
    // open + quit only.
    let open_item = MenuItem::with_id(app, "open_settings", "Open Settings…", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &sep, &quit_item])?;

    let initial_dark = windows_uses_dark_theme();

    let builder = TrayIconBuilder::with_id(TRAY_ID)
        .icon(load_themed_icon(initial_dark))
        .tooltip(TrayState::Idle.tooltip())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(
            move |app: &AppHandle, event: MenuEvent| match event.id.as_ref() {
                "quit" => app.exit(0),
                "open_settings" => {
                    if let Err(e) = crate::ui::open_settings_window(app) {
                        tracing::error!("open settings failed: {:#}", e);
                    }
                }
                _ => {}
            },
        );

    // On macOS the OS handles tinting for template images: a B&W glyph
    // with alpha gets painted in the current menu-bar color (which
    // tracks light/dark automatically). Our tray.png is already that
    // shape, so we just flag it template and skip the Windows polling
    // thread entirely.
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);

    builder.build(app)?;

    // Theme-tracking poll only matters on Windows — that's where we
    // hand-recolor the icon based on a registry value. macOS template
    // images recolor themselves; Linux has no equivalent surface.
    #[cfg(target_os = "windows")]
    {
        let app_for_poll = app.clone();
        std::thread::spawn(move || {
            let mut current = initial_dark;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let now = windows_uses_dark_theme();
                if now != current {
                    current = now;
                    if let Some(tray) = app_for_poll.tray_by_id(TRAY_ID) {
                        let _ = tray.set_icon(Some(load_themed_icon(now)));
                    }
                }
            }
        });
    }
    // Use the variable on non-Windows to silence unused-var warnings
    // when the polling loop is compiled out.
    #[cfg(not(target_os = "windows"))]
    let _ = initial_dark;

    Ok(())
}

pub fn set_state(app: &AppHandle, state: TrayState) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(state.tooltip()));
    }
}
