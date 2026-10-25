use crate::{
    audio::{Device, Snapshot, wide},
    input,
    storage::{InputMode, MuteMode, Settings},
};
use anyhow::{Context, Result};
use std::{cell::RefCell, collections::HashMap};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        UI::{
            Controls::*,
            HiDpi::*,
            Input::KeyboardAndMouse::{EnableWindow, SetFocus},
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

pub const DEVICE: u16 = 100;
pub const REFRESH: u16 = 101;
pub const KEY: u16 = 102;
pub const RECORD: u16 = 103;
pub const MODE: u16 = 104;
pub const MINIMUM: u16 = 105;
pub const SOUNDS: u16 = 106;
pub const STARTUP: u16 = 107;
pub const SAVE: u16 = 108;
pub const CANCEL: u16 = 109;
pub const WINDOWS_SETTINGS: u16 = 110;
pub const TOGGLE: u16 = 200;
pub const SETTINGS: u16 = 201;
pub const EXIT: u16 = 202;
const STATUS: u16 = 111;
const DEVICE_NAME: u16 = 112;
const MESSAGE: u16 = 113;
const TITLE: u16 = 114;

pub struct Ui {
    pub window: HWND,
    pub recorder: Option<HWND>,
    instance: HINSTANCE,
    controls: Vec<(u16, HWND)>,
    fonts: Vec<HFONT>,
    pub devices: Vec<Device>,
    keys: &'static [(u16, String)],
    text_cache: RefCell<HashMap<u16, String>>,
}

impl Ui {
    pub fn new(instance: HINSTANCE, parent: HWND) -> Result<Self> {
        unsafe {
            let dpi = GetDpiForSystem() as i32;
            let scale = |value: i32| value * dpi / 96;
            let window = CreateWindowExW(
                WS_EX_CONTROLPARENT | WS_EX_APPWINDOW,
                w!("MuteMic.Rust.Settings"),
                w!("MuteMic Settings"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                scale(610),
                scale(710),
                Some(parent),
                None,
                Some(instance),
                None,
            )?;
            let font = CreateFontW(
                -scale(16),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            let title_font = CreateFontW(
                -scale(25),
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Segoe UI"),
            );
            let mut ui = Self {
                window,
                recorder: None,
                instance,
                controls: vec![],
                fonts: vec![font, title_font],
                devices: vec![],
                keys: input::supported_keys(),
                text_cache: RefCell::new(HashMap::new()),
            };
            if font.is_invalid() || title_font.is_invalid() {
                anyhow::bail!("Could not create the settings fonts.");
            }
            let title = ui.add(
                TITLE,
                "STATIC",
                "MuteMic",
                (24, 18, 540, 34),
                WINDOW_STYLE(0),
            )?;
            SendMessageW(
                title,
                WM_SETFONT,
                Some(WPARAM(title_font.0 as usize)),
                Some(LPARAM(1)),
            );
            ui.add(
                0,
                "STATIC",
                "One key to toggle your selected headset microphone.",
                (24, 57, 540, 26),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                STATUS,
                "STATIC",
                "Microphone status",
                (24, 96, 540, 24),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                DEVICE_NAME,
                "STATIC",
                "",
                (24, 124, 540, 35),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                0,
                "STATIC",
                "Microphone",
                (24, 174, 540, 22),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                DEVICE,
                "COMBOBOX",
                "",
                (24, 199, 435, 260),
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0),
            )?;
            ui.add(
                REFRESH,
                "BUTTON",
                "Refresh",
                (469, 198, 100, 29),
                WS_TABSTOP,
            )?;
            ui.add(
                0,
                "STATIC",
                "Only this microphone is controlled, including after reconnecting.",
                (24, 235, 540, 22),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                0,
                "STATIC",
                "Shortcut key",
                (24, 273, 540, 22),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                KEY,
                "COMBOBOX",
                "",
                (24, 298, 390, 260),
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0),
            )?;
            ui.add(
                RECORD,
                "BUTTON",
                "Record key...",
                (424, 297, 145, 29),
                WS_TABSTOP,
            )?;
            ui.add(
                0,
                "STATIC",
                "The shortcut key also reaches other applications.",
                (24, 333, 540, 22),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                0,
                "STATIC",
                "Input mode",
                (24, 369, 540, 22),
                WINDOW_STYLE(0),
            )?;
            ui.add(
                MODE,
                "COMBOBOX",
                "",
                (24, 394, 545, 120),
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_TABSTOP.0),
            )?;
            ui.add(
                MINIMUM,
                "BUTTON",
                "Use minimum volume (compatibility mode)",
                (24, 433, 545, 26),
                WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0),
            )?;
            ui.add(
                SOUNDS,
                "BUTTON",
                "Play a sound when toggling",
                (24, 464, 545, 26),
                WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0),
            )?;
            ui.add(
                STARTUP,
                "BUTTON",
                "Start with Windows",
                (24, 495, 545, 26),
                WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0),
            )?;
            ui.add(MESSAGE, "STATIC", "", (24, 533, 545, 62), WINDOW_STYLE(0))?;
            ui.add(
                WINDOWS_SETTINGS,
                "BUTTON",
                "Windows microphone settings",
                (24, 605, 255, 32),
                WS_TABSTOP,
            )?;
            ui.add(CANCEL, "BUTTON", "Cancel", (330, 605, 95, 32), WS_TABSTOP)?;
            ui.add(
                SAVE,
                "BUTTON",
                "Save and close",
                (435, 605, 134, 32),
                WINDOW_STYLE(BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0),
            )?;
            for (_, label) in ui.keys {
                ui.combo_add(KEY, label);
            }
            ui.combo_add(MODE, "Background keyboard input (recommended)");
            ui.combo_add(MODE, "Compatibility mode (key state polling)");
            Ok(ui)
        }
    }

    fn add(
        &mut self,
        id: u16,
        class: &str,
        text: &str,
        (x, y, width, height): (i32, i32, i32, i32),
        style: WINDOW_STYLE,
    ) -> Result<HWND> {
        unsafe {
            let dpi = GetDpiForWindow(self.window) as i32;
            let scale = |value| value * dpi / 96;
            let handle = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide(class).as_ptr()),
                PCWSTR(wide(text).as_ptr()),
                WS_CHILD | WS_VISIBLE | style,
                scale(x),
                scale(y),
                scale(width),
                scale(height),
                Some(self.window),
                Some(HMENU(id as usize as *mut _)),
                Some(self.instance),
                None,
            )?;
            SendMessageW(
                handle,
                WM_SETFONT,
                Some(WPARAM(self.fonts[0].0 as usize)),
                Some(LPARAM(1)),
            );
            self.controls.push((id, handle));
            Ok(handle)
        }
    }

    pub fn control(&self, id: u16) -> HWND {
        self.controls
            .iter()
            .find(|entry| entry.0 == id)
            .expect("known control ID")
            .1
    }
    fn combo_add(&self, id: u16, text: &str) {
        unsafe {
            SendMessageW(
                self.control(id),
                CB_ADDSTRING,
                None,
                Some(LPARAM(wide(text).as_ptr() as isize)),
            );
        }
    }
    pub fn selected(&self, id: u16) -> Option<usize> {
        let value = unsafe { SendMessageW(self.control(id), CB_GETCURSEL, None, None) }.0;
        (value >= 0).then_some(value as usize)
    }
    fn select(&self, id: u16, index: Option<usize>) {
        unsafe {
            SendMessageW(
                self.control(id),
                CB_SETCURSEL,
                Some(WPARAM(index.unwrap_or(usize::MAX))),
                None,
            );
        }
    }
    fn checked(&self, id: u16) -> bool {
        unsafe {
            SendMessageW(self.control(id), BM_GETCHECK, None, None).0 == BST_CHECKED.0 as isize
        }
    }
    fn check(&self, id: u16, value: bool) {
        unsafe {
            SendMessageW(
                self.control(id),
                BM_SETCHECK,
                Some(WPARAM(if value {
                    BST_CHECKED.0 as usize
                } else {
                    BST_UNCHECKED.0 as usize
                })),
                None,
            );
        }
    }
    fn text(&self, id: u16, text: &str) {
        if self
            .text_cache
            .borrow()
            .get(&id)
            .is_some_and(|current| current == text)
        {
            return;
        }
        if unsafe { SetWindowTextW(self.control(id), PCWSTR(wide(text).as_ptr())) }.is_ok() {
            self.text_cache.borrow_mut().insert(id, text.into());
        }
    }

    pub fn open(
        &mut self,
        settings: &Settings,
        devices: Vec<Device>,
        read_only: bool,
    ) -> Result<()> {
        self.devices.clear();
        self.refresh_devices(settings, devices);
        self.select(
            KEY,
            self.keys
                .iter()
                .position(|entry| entry.0 == settings.hotkey),
        );
        self.select(
            MODE,
            Some(if settings.input_mode == InputMode::RawInput {
                0
            } else {
                1
            }),
        );
        self.check(MINIMUM, settings.mute_mode == MuteMode::VolumeZero);
        self.check(SOUNDS, settings.play_sounds);
        self.check(STARTUP, settings.start_with_windows);
        self.text(
            TITLE,
            if read_only {
                "MuteMic - read-only profiling"
            } else {
                "MuteMic"
            },
        );
        self.text(
            SAVE,
            if read_only {
                "Read-only mode"
            } else {
                "Save and close"
            },
        );
        let title = if read_only {
            "MuteMic Profiling (read-only)"
        } else {
            "MuteMic Settings"
        };
        let caption = format!("{title} - {}", env!("CARGO_PKG_VERSION"));
        unsafe {
            SetWindowTextW(self.window, PCWSTR(wide(&caption).as_ptr()))?;
            for id in [
                DEVICE,
                REFRESH,
                KEY,
                RECORD,
                MODE,
                MINIMUM,
                SOUNDS,
                STARTUP,
                WINDOWS_SETTINGS,
                SAVE,
            ] {
                let _ = EnableWindow(self.control(id), !read_only);
            }
            let _ = ShowWindow(self.window, SW_RESTORE);
            let _ = SetForegroundWindow(self.window);
        }
        Ok(())
    }

    pub fn refresh_devices(&mut self, settings: &Settings, mut devices: Vec<Device>) {
        let id = self
            .selected_device()
            .map(|device| device.id.clone())
            .or_else(|| settings.device_id.clone());
        if let Some(id) = &id
            && !devices.iter().any(|device| &device.id == id)
        {
            devices.insert(
                0,
                Device {
                    id: id.clone(),
                    name: settings
                        .device_name
                        .clone()
                        .unwrap_or_else(|| "Selected microphone".into()),
                    available: false,
                },
            );
        }
        unsafe {
            SendMessageW(self.control(DEVICE), CB_RESETCONTENT, None, None);
        }
        for device in &devices {
            self.combo_add(
                DEVICE,
                &if device.available {
                    device.name.clone()
                } else {
                    format!("{} (unavailable)", device.name)
                },
            );
        }
        self.devices = devices;
        self.select(
            DEVICE,
            id.and_then(|id| self.devices.iter().position(|device| device.id == id)),
        );
    }

    pub fn selected_device(&self) -> Option<&Device> {
        self.selected(DEVICE)
            .and_then(|index| self.devices.get(index))
    }
    pub fn draft(&self) -> Result<Settings> {
        let device = self
            .selected_device()
            .context("Choose a microphone before saving.")?;
        let key = self
            .selected(KEY)
            .and_then(|index| self.keys.get(index))
            .context("Choose a shortcut key.")?;
        Ok(Settings {
            device_id: Some(device.id.clone()),
            device_name: Some(device.name.clone()),
            hotkey: key.0,
            input_mode: if self.selected(MODE) == Some(1) {
                InputMode::Compatibility
            } else {
                InputMode::RawInput
            },
            play_sounds: self.checked(SOUNDS),
            mute_mode: if self.checked(MINIMUM) {
                MuteMode::VolumeZero
            } else {
                MuteMode::Endpoint
            },
            start_with_windows: self.checked(STARTUP),
        })
    }

    pub fn set_status(&self, snapshot: &Snapshot, warning: Option<&str>, read_only: bool) {
        let state = if !snapshot.available {
            "Microphone unavailable"
        } else if snapshot.warning.is_some() {
            "Mute needs attention"
        } else if snapshot.volume_minimized {
            "Microphone volume minimized"
        } else if snapshot.muted {
            "Windows reports muted"
        } else {
            "Microphone active"
        };
        self.text(STATUS, state);
        self.text(DEVICE_NAME, &snapshot.name);
        let hint = if read_only {
            "Profiling mode: saving and shortcuts are disabled. Open MuteMic.exe without --read-only to use the app normally."
        } else if self.checked(MINIMUM) {
            "This mode reduces input to the device's minimum volume. Very loud sounds may still pass. Your exact original volume is restored when toggled."
        } else {
            "Windows and available hardware mute controls are checked. Volume stays unchanged. Disable exclusive microphone access for predictable behavior."
        };
        self.text(
            MESSAGE,
            warning.or(snapshot.warning.as_deref()).unwrap_or(hint),
        );
    }

    pub fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.window).as_bool() }
    }
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.window, SW_HIDE);
        }
    }
    pub fn begin_recording(&mut self) -> Result<()> {
        if self.recorder.is_some() {
            return Ok(());
        }
        unsafe {
            let dpi = GetDpiForWindow(self.window) as i32;
            let scale = |value: i32| value * dpi / 96;
            let window = CreateWindowExW(
                WS_EX_DLGMODALFRAME,
                w!("MuteMic.Rust.Settings"),
                w!("Choose shortcut key"),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                scale(430),
                scale(170),
                Some(self.window),
                None,
                Some(self.instance),
                None,
            )?;
            let label = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(
                    "Press one supported key.\nRelease Ctrl, Alt and Shift first.\nPress Escape to cancel."
                ),
                WS_CHILD | WS_VISIBLE,
                scale(24),
                scale(24),
                scale(380),
                scale(95),
                Some(window),
                None,
                Some(self.instance),
                None,
            );
            let label = match label {
                Ok(label) => label,
                Err(error) => {
                    let _ = DestroyWindow(window);
                    return Err(error.into());
                }
            };
            SendMessageW(
                label,
                WM_SETFONT,
                Some(WPARAM(self.fonts[0].0 as usize)),
                Some(LPARAM(1)),
            );
            self.recorder = Some(window);
            let _ = EnableWindow(self.window, false);
            let _ = ShowWindow(window, SW_SHOW);
            let _ = SetFocus(Some(window));
        }
        Ok(())
    }
    pub fn end_recording(&mut self, key: Option<u16>) {
        if let Some(key) = key {
            self.select(KEY, self.keys.iter().position(|entry| entry.0 == key));
        }
        if let Some(window) = self.recorder.take() {
            unsafe {
                let _ = DestroyWindow(window);
            }
        }
        unsafe {
            let _ = EnableWindow(self.window, true);
            let _ = SetForegroundWindow(self.window);
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        unsafe {
            if let Some(window) = self.recorder.take() {
                let _ = DestroyWindow(window);
            }
            let _ = DestroyWindow(self.window);
            for font in &self.fonts {
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
        }
    }
}

pub fn create_icons() -> Result<[HICON; 4]> {
    let mut icons = Vec::new();
    for (index, (red, green, blue)) in [
        (21, 150, 100),
        (214, 55, 70),
        (185, 117, 22),
        (105, 116, 134),
    ]
    .into_iter()
    .enumerate()
    {
        let mut pixels = [0u8; 32 * 32 * 4];
        let mut mask = [255u8; 128];
        for y in 0..32i32 {
            for x in 0..32i32 {
                if (x - 16).pow(2) + (y - 16).pow(2) > 15 * 15 {
                    continue;
                }
                mask[y as usize * 4 + x as usize / 8] &= !(0x80 >> (x % 8));
                let microphone = ((13..=19).contains(&x) && (6..=18).contains(&y))
                    || ((x == 10 || x == 22) && (13..=20).contains(&y))
                    || ((10..=22).contains(&x) && y == 21)
                    || (x == 16 && (21..=26).contains(&y))
                    || ((12..=20).contains(&x) && y == 26);
                let slash = index == 1 && (x - y).abs() <= 1 && (6..=26).contains(&x);
                let color = if microphone || slash {
                    [255, 255, 255, 255]
                } else {
                    [blue, green, red, 255]
                };
                let offset = ((31 - y) * 32 + x) as usize * 4;
                pixels[offset..offset + 4].copy_from_slice(&color);
            }
        }
        match unsafe { CreateIcon(None, 32, 32, 1, 32, mask.as_ptr(), pixels.as_ptr()) } {
            Ok(icon) => icons.push(icon),
            Err(error) => {
                for icon in icons {
                    unsafe {
                        let _ = DestroyIcon(icon);
                    }
                }
                return Err(error.into());
            }
        }
    }
    icons
        .try_into()
        .map_err(|_| anyhow::anyhow!("Could not create tray icons."))
}
