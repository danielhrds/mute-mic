// Render this application's controls without desktop input automation or audio changes.
#[allow(dead_code)]
#[path = "../src/audio.rs"]
mod audio;
#[allow(dead_code)]
#[path = "../src/input.rs"]
mod input;
#[allow(dead_code)]
#[path = "../src/storage.rs"]
mod storage;
#[allow(dead_code)]
#[path = "../src/ui.rs"]
mod ui;
#[allow(dead_code)]
#[path = "../src/volume.rs"]
mod volume;

use anyhow::{Context, Result, bail};
use std::{fs::File, io::Write, mem::size_of, path::PathBuf};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow},
        System::{Com::*, LibraryLoader::GetModuleHandleW},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

#[derive(Default)]
struct RenderSurface {
    window: HWND,
    dc: HDC,
    memory: HDC,
    bitmap: HBITMAP,
}
impl Drop for RenderSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            }
            if !self.memory.is_invalid() {
                let _ = DeleteDC(self.memory);
            }
            if !self.dc.is_invalid() {
                ReleaseDC(Some(self.window), self.dc);
            }
        }
    }
}

fn render(window: HWND, path: &std::path::Path) -> Result<()> {
    unsafe {
        let mut rectangle = RECT::default();
        GetWindowRect(window, &mut rectangle)?;
        let width = rectangle.right - rectangle.left;
        let height = rectangle.bottom - rectangle.top;
        if width <= 0 || height <= 0 || width > 5000 || height > 5000 {
            bail!("Unexpected render dimensions.");
        }
        let mut surface = RenderSurface {
            window,
            ..RenderSurface::default()
        };
        surface.dc = GetDC(Some(window));
        if surface.dc.is_invalid() {
            bail!("Could not acquire the window device context.");
        }
        surface.memory = CreateCompatibleDC(Some(surface.dc));
        if surface.memory.is_invalid() {
            bail!("Could not create the render device context.");
        }
        surface.bitmap = CreateCompatibleBitmap(surface.dc, width, height);
        if surface.bitmap.is_invalid() {
            bail!("Could not create the render bitmap.");
        }
        let RenderSurface {
            dc, memory, bitmap, ..
        } = surface;
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let printed = PrintWindow(window, memory, PRINT_WINDOW_FLAGS(2)).as_bool();
        SelectObject(memory, previous);
        let mut pixels = vec![0; width as usize * height as usize * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        drop(surface);
        if !printed || rows != height {
            bail!("Native control rendering failed.");
        }
        let mut file = File::create(path)?;
        file.write_all(b"BM")?;
        file.write_all(&(54 + pixels.len() as u32).to_le_bytes())?;
        file.write_all(&0u32.to_le_bytes())?;
        file.write_all(&54u32.to_le_bytes())?;
        let header = std::slice::from_raw_parts(
            (&info.bmiHeader as *const BITMAPINFOHEADER).cast::<u8>(),
            40,
        );
        file.write_all(header)?;
        file.write_all(&pixels)?;
    }
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let output = args
        .get(1)
        .context("Usage: ui-check.exe <output.bmp> <profile-directory> [--read-only]")?;
    let directory = PathBuf::from(args.get(2).context("A profile directory is required.")?);
    let read_only = args
        .get(3)
        .is_some_and(|argument| argument == "--read-only");
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = ComApartment;
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("MuteMic.Rust.Settings"),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread().into());
        }
        let (settings, warning) = storage::Settings::load(&directory);
        let store = storage::VolumeStore::new(&directory);
        let snapshot = settings
            .device_id
            .as_deref()
            .map(|id| audio::Endpoint::open(id).and_then(|endpoint| endpoint.snapshot(&store)))
            .transpose()?
            .unwrap_or_else(|| audio::Snapshot {
                name: "Choose your headset in Settings".into(),
                ..Default::default()
            });
        {
            let mut ui = ui::Ui::new(instance, HWND::default())?;
            ui.open(&settings, audio::devices()?, false)?;
            let selected_key = |ui: &ui::Ui| {
                ui.selected(ui::KEY)
                    .map(|index| input::supported_keys()[index].0)
            };
            if selected_key(&ui) != Some(settings.hotkey)
                || !windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(
                    ui.control(ui::SAVE),
                )
                .as_bool()
            {
                bail!("Normal settings must enable saving and show the saved or default shortcut.");
            }
            if settings.device_id.is_none() && ui.selected_device().is_some() {
                bail!("First-run settings must require explicit microphone selection.");
            }
            if settings.device_id.is_some() {
                let draft = ui.draft()?;
                if draft.device_id != settings.device_id
                    || draft.hotkey != settings.hotkey
                    || draft.input_mode != settings.input_mode
                {
                    bail!("The native controls did not retain their saved values.");
                }
            }
            ui.begin_recording()?;
            ui.end_recording(Some(0x78));
            if selected_key(&ui) != Some(0x78)
                || !windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(
                    ui.control(ui::SAVE),
                )
                .as_bool()
            {
                bail!("The captured key was not selected.");
            }
            ui.open(&settings, audio::devices()?, true)?;
            for id in [ui::SAVE, ui::KEY, ui::RECORD, ui::DEVICE, ui::MODE] {
                if windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(ui.control(id))
                    .as_bool()
                {
                    bail!("Profiling controls must be visibly read-only.");
                }
            }
            ui.open(&settings, audio::devices()?, read_only)?;
            if windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(ui.control(ui::SAVE))
                .as_bool()
                == read_only
            {
                bail!("Normal settings did not recover their editable state after profiling.");
            }
            verify_raw_registration(ui.window)?;
            ui.set_status(&snapshot, warning.as_deref(), read_only);
            let _ = UpdateWindow(ui.window);
            render(ui.window, &PathBuf::from(output))?;
        }
    }
    Ok(())
}

fn verify_raw_registration(window: HWND) -> Result<()> {
    use windows::Win32::UI::Input::*;
    unsafe {
        let size = size_of::<RAWINPUTDEVICE>() as u32;
        RegisterRawInputDevices(
            &[RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
                hwndTarget: window,
            }],
            size,
        )?;
        let result = (|| -> Result<()> {
            let mut count = 0;
            if GetRegisteredRawInputDevices(None, &mut count, size) == u32::MAX {
                bail!("Could not query raw input registration.");
            }
            let mut devices = vec![RAWINPUTDEVICE::default(); count as usize];
            if GetRegisteredRawInputDevices(Some(devices.as_mut_ptr()), &mut count, size)
                == u32::MAX
            {
                bail!("Could not read raw input registration.");
            }
            if !devices.iter().any(|device| {
                device.usUsagePage == 1 && device.usUsage == 6 && device.hwndTarget == window
            }) {
                bail!("The keyboard receiver was not registered.");
            }
            Ok(())
        })();
        let remove = RegisterRawInputDevices(
            &[RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            }],
            size,
        );
        result?;
        remove?;
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
