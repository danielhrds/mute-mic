use crate::{
    audio::{self, Endpoint, Snapshot, wide},
    input::{self, KeyTracker},
    startup,
    storage::{InputMode, RestoreStore, Settings, VolumeStore},
    ui::{self, Ui},
    volume,
};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    mem::{size_of, size_of_val},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        Media::Audio::{PlaySoundW, SND_ASYNC, SND_FLAGS, SND_MEMORY, SND_NODEFAULT},
        System::{Com::*, LibraryLoader::GetModuleHandleW, RemoteDesktop::*, Threading::*},
        UI::{
            HiDpi::*,
            Input::{KeyboardAndMouse::*, *},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

const TRAY_MESSAGE: u32 = WM_APP + 1;
const TOGGLE_MESSAGE: u32 = WM_APP + 2;
const VOLUME_MESSAGE: u32 = WM_APP + 3;
const STATUS_TIMER: usize = 1;
const POLL_TIMER: usize = 2;
const TRAY_KEY_SELECT: u32 = NIN_SELECT | NINF_KEY;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static KEYBOARD: RefCell<Keyboard> = RefCell::new(Keyboard::default());
    static DEFERRED: RefCell<DeferredMessages> = RefCell::new(DeferredMessages::default());
}

#[derive(Default)]
struct Keyboard {
    tracker: KeyTracker,
    hotkey: u16,
    raw: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct Message {
    window: HWND,
    id: u32,
    wparam: WPARAM,
    lparam: LPARAM,
}

#[derive(Default)]
struct DeferredMessages(VecDeque<Message>);

impl DeferredMessages {
    fn push(&mut self, mut message: Message) -> bool {
        // Only copy messages whose parameters contain values, never borrowed pointers.
        let registered = message.id >= 0xc000
            && unsafe {
                message.id == RegisterWindowMessageW(w!("TaskbarCreated"))
                    || message.id == RegisterWindowMessageW(w!("MuteMic.ShowSettings.v1"))
            };
        if !matches!(
            message.id,
            WM_COMMAND
                | WM_CLOSE
                | WM_POWERBROADCAST
                | WM_WTSSESSION_CHANGE
                | TRAY_MESSAGE
                | TOGGLE_MESSAGE
                | VOLUME_MESSAGE
                | WM_TIMER
        ) && !registered
        {
            return false;
        }
        if matches!(message.id, WM_POWERBROADCAST | WM_WTSSESSION_CHANGE) {
            // Only the reset signal is needed; power-setting payloads have a borrowed lifetime.
            message.lparam = LPARAM(0);
        }
        if matches!(message.id, WM_TIMER | VOLUME_MESSAGE) && self.0.contains(&message) {
            return true;
        }
        self.0.push_back(message);
        true
    }
}

enum Effect {
    Error(String),
    Menu {
        window: HWND,
        enabled: bool,
        label: &'static str,
    },
}

impl Effect {
    fn run(self) {
        match self {
            Self::Error(message) => show_error(&message),
            Self::Menu {
                window,
                enabled,
                label,
            } => {
                if let Err(error) = tray_menu(window, enabled, label) {
                    APP.with(|slot| {
                        if let Some(app) = slot.borrow_mut().as_mut() {
                            app.report(&format!("Could not open tray menu: {error:#}"));
                        }
                    });
                }
            }
        }
    }
}

pub fn show_error(message: &str) {
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(wide(message).as_ptr()),
            w!("MuteMic"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

fn log(directory: &Path, message: &str) {
    let _ = (|| -> std::io::Result<()> {
        fs::create_dir_all(directory)?;
        let path = directory.join("app.log");
        if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 1_000_000) {
            let previous = directory.join("app.previous.log");
            let _ = fs::remove_file(&previous);
            fs::rename(&path, previous)?;
        }
        writeln!(
            OpenOptions::new().create(true).append(true).open(path)?,
            "{:?} {message}",
            std::time::SystemTime::now()
        )
    })();
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct InstanceMutex(HANDLE);
impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn run() -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _com = ComApartment;
    let args: Vec<String> = std::env::args().collect();
    let argument = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1))
    };
    let directory = match argument("--data-dir") {
        Some(path) => std::path::absolute(path)?,
        None => {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable.")?)
                .join("MuteMic")
        }
    };
    let directory = data_directory(&directory)?;
    let executable = std::env::current_exe()?;
    let (mut settings, warning) = Settings::load(&directory);
    if let Some(path) = argument("--diagnostics") {
        let store = VolumeStore::new(&directory);
        let devices = audio::devices()?.into_iter().map(|device| {
            let snapshot = Endpoint::open(&device.id).and_then(|endpoint| endpoint.snapshot(&store));
            serde_json::json!({ "Device": device, "State": snapshot.as_ref().ok(), "Error": snapshot.as_ref().err().map(|error| format!("{error:#}")) })
        }).collect::<Vec<_>>();
        fs::write(
            path,
            serde_json::to_vec_pretty(
                &serde_json::json!({ "Version": env!("CARGO_PKG_VERSION"), "Settings": settings, "Devices": devices }),
            )?,
        )?;
        return Ok(());
    }
    let hash = Sha256::digest(directory.to_string_lossy().to_uppercase().as_bytes());
    let suffix: String = hash[..8].iter().map(|byte| format!("{byte:02X}")).collect();
    let title = format!("MuteMic.Instance.{suffix}");
    let mutex = unsafe {
        CreateMutexW(
            None,
            true,
            PCWSTR(wide(&format!("Local\\{title}")).as_ptr()),
        )?
    };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(mutex);
            if !args.iter().any(|arg| arg == "--startup")
                && let Ok(window) = FindWindowW(PCWSTR::null(), PCWSTR(wide(&title).as_ptr()))
            {
                let mut process = 0;
                GetWindowThreadProcessId(window, Some(&mut process));
                let _ = AllowSetForegroundWindow(process);
                let message = RegisterWindowMessageW(w!("MuteMic.ShowSettings.v1"));
                let _ = PostMessageW(Some(window), message, WPARAM(0), LPARAM(0));
            }
        }
        return Ok(());
    }
    let _mutex = InstanceMutex(mutex);
    let read_only = args.iter().any(|arg| arg == "--read-only");
    let startup_launch = args.iter().any(|arg| arg == "--startup");
    let preference = if args.iter().any(|arg| arg == "--enable-startup") {
        Some(true)
    } else if args.iter().any(|arg| arg == "--disable-startup") {
        Some(false)
    } else {
        None
    };
    if let Some(enabled) = preference {
        if read_only {
            bail!("Read-only mode cannot change startup registration.");
        }
        if warning.is_some() {
            bail!("Settings could not be read; startup registration was not changed.");
        }
        settings.start_with_windows = enabled;
        startup::save(&executable, &directory, enabled, || {
            settings.save(&directory)
        })?;
    } else {
        settings.start_with_windows = startup::enabled(&executable, &directory)?;
    }
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        for class in [w!("MuteMic.Rust.Hidden"), w!("MuteMic.Rust.Settings")] {
            let definition = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
                ..Default::default()
            };
            if RegisterClassW(&definition) == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
        }
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("MuteMic.Rust.Hidden"),
            PCWSTR(wide(&title).as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        let mut app = App::new(
            window, instance, directory, executable, settings, warning, read_only,
        )?;
        app.configure_input();
        app.refresh();
        if SetTimer(Some(window), STATUS_TIMER, 1000, None) == 0 {
            bail!("Could not start the status timer.");
        }
        let _ = WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION);
        let show = args.iter().any(|arg| arg == "--settings")
            || (!startup_launch && (app.settings.device_id.is_none() || app.warning.is_some()));
        log(&app.directory, "Rust application started");
        APP.with(|slot| *slot.borrow_mut() = Some(app));
        if show {
            let _ = PostMessageW(
                Some(window),
                RegisterWindowMessageW(w!("MuteMic.ShowSettings.v1")),
                WPARAM(0),
                LPARAM(0),
            );
        }
        let mut message = MSG::default();
        let message_error = loop {
            let result = GetMessageW(&mut message, None, 0, 0);
            if result.0 < 0 {
                break Some(windows::core::Error::from_thread());
            }
            if result.0 == 0 {
                break None;
            }
            let settings_window = APP.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(|app| app.ui.as_ref())
                    .filter(|ui| ui.visible() && ui.recorder.is_none())
                    .map(|ui| ui.window)
            });
            if settings_window.is_some_and(|window| IsDialogMessageW(window, &message).as_bool()) {
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        };
        APP.with(|slot| {
            let app = slot.borrow_mut().take();
            drop(app);
        });
        if let Some(error) = message_error {
            return Err(error.into());
        }
        if message.wParam.0 != 0 {
            bail!("The application stopped after an unexpected message handling failure.");
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = std::panic::catch_unwind(|| {
        keyboard_message(window, message, wparam, lparam);
        let event = Message {
            window,
            id: message,
            wparam,
            lparam,
        };
        let mut dispatched = false;
        let (result, effect) = APP.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                let queued = DEFERRED.with(|queue| queue.borrow_mut().push(event));
                return (queued.then_some(LRESULT(0)), None);
            };
            dispatched = true;
            let Some(app) = slot.as_mut() else {
                return (None, None);
            };
            let result = app.message(window, message, wparam, lparam);
            (result, app.effect.take())
        });
        if dispatched {
            // Native menus, dialogs and COM can dispatch messages synchronously.
            // Release the application borrow before entering any modal loop.
            if let Some(effect) = effect {
                effect.run();
            }
            loop {
                let next = DEFERRED.with(|queue| queue.borrow_mut().0.pop_front());
                let Some(next) = next else {
                    break;
                };
                let effect = APP.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    let app = slot.as_mut()?;
                    app.message(next.window, next.id, next.wparam, next.lparam);
                    app.effect.take()
                });
                if let Some(effect) = effect {
                    effect.run();
                }
            }
        }
        result
    });
    match handled {
        Ok(Some(result)) => result,
        Ok(None) => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        Err(_) => unsafe {
            PostQuitMessage(1);
            DefWindowProcW(window, message, wparam, lparam)
        },
    }
}

fn data_directory(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path)?;
    let canonical = fs::canonicalize(path)?;
    // Keep the ordinary DOS spelling compatible with existing instance names and startup entries.
    let text = canonical
        .to_str()
        .context("The data directory is not valid Unicode.")?;
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        Ok(PathBuf::from(format!(r"\\{unc}")))
    } else {
        Ok(PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text)))
    }
}

fn keyboard_message(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) {
    KEYBOARD.with(|state| {
        let mut state = state.borrow_mut();
        if message == WM_INPUT_DEVICE_CHANGE && wparam.0 == GIDC_REMOVAL as usize {
            state.tracker.remove(lparam.0);
        }
        if message != WM_INPUT || !state.raw {
            return;
        }
        let mut buffer = [0u64; 5];
        let mut size = size_of_val(&buffer) as u32;
        let copied = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut _),
                RID_INPUT,
                Some(buffer.as_mut_ptr().cast()),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if copied > size_of_val(&buffer) as u32 {
            return;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), copied as usize) };
        if let Some(packet) = input::parse(bytes, size_of::<usize>())
            && packet.key == state.hotkey
        {
            let toggle = state.tracker.process(packet.device, packet.down);
            if !packet.down {
                let released = unsafe { GetAsyncKeyState(state.hotkey as i32) >= 0 };
                state.tracker.release_wait(released);
            }
            if toggle {
                // This is an action for the configured shortcut; no typed keys are retained.
                unsafe {
                    let _ = PostMessageW(Some(window), TOGGLE_MESSAGE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

struct App {
    window: HWND,
    instance: HINSTANCE,
    directory: PathBuf,
    executable: PathBuf,
    settings: Settings,
    store: VolumeStore,
    endpoint: Option<Endpoint>,
    preview: Option<Endpoint>,
    snapshot: Snapshot,
    ui: Option<Ui>,
    warning: Option<String>,
    last_error: Option<String>,
    read_only: bool,
    effect: Option<Effect>,
    raw_registered: bool,
    polling: bool,
    watched: bool,
    watch_retry: Instant,
    pending: Arc<AtomicBool>,
    tray: NOTIFYICONDATAW,
    icons: [HICON; 4],
    last_icon: usize,
    last_tip: String,
    taskbar_message: u32,
    settings_message: u32,
    muted_sound: Vec<u8>,
    active_sound: Vec<u8>,
}

impl App {
    fn new(
        window: HWND,
        instance: HINSTANCE,
        directory: PathBuf,
        executable: PathBuf,
        settings: Settings,
        warning: Option<String>,
        read_only: bool,
    ) -> Result<Self> {
        let icons = ui::create_icons()?;
        let mut tray = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            hIcon: icons[3],
            ..Default::default()
        };
        copy_text(&mut tray.szTip, "MuteMic");
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &tray) }.as_bool() {
            for icon in icons {
                unsafe {
                    let _ = DestroyIcon(icon);
                }
            }
            bail!("Could not add the tray icon.");
        }
        unsafe {
            tray.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &tray);
        }
        Ok(Self {
            window,
            instance,
            store: VolumeStore::new(&directory),
            directory,
            executable,
            settings,
            warning,
            read_only,
            endpoint: None,
            preview: None,
            snapshot: Snapshot::default(),
            ui: None,
            last_error: None,
            effect: None,
            raw_registered: false,
            polling: false,
            watched: false,
            watch_retry: Instant::now(),
            pending: Arc::new(AtomicBool::new(false)),
            tray,
            icons,
            last_icon: usize::MAX,
            last_tip: String::new(),
            taskbar_message: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            settings_message: unsafe { RegisterWindowMessageW(w!("MuteMic.ShowSettings.v1")) },
            muted_sound: tone(620.0, 420.0),
            active_sound: tone(420.0, 720.0),
        })
    }

    fn held(&self) -> bool {
        unsafe { GetAsyncKeyState(self.settings.hotkey as i32) < 0 }
    }
    fn recording(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.recorder.is_some())
    }
    fn reset_input(&mut self) {
        KEYBOARD.with(|state| {
            let mut state = state.borrow_mut();
            state.hotkey = self.settings.hotkey;
            state.tracker.reset(self.held());
        });
    }
    fn configure_input(&mut self) {
        KEYBOARD.with(|state| state.borrow_mut().raw = false);
        unsafe {
            let _ = KillTimer(Some(self.window), POLL_TIMER);
        }
        self.polling = false;
        if self.raw_registered {
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            };
            match unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) }
            {
                Ok(()) => self.raw_registered = false,
                Err(error) => {
                    self.report(&format!(
                        "Could not release background keyboard input: {error}"
                    ));
                    return;
                }
            }
        }
        self.reset_input();
        if self.recording() {
            return;
        }
        if self.settings.input_mode == InputMode::RawInput {
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
                hwndTarget: self.window,
            };
            match unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32) }
            {
                Ok(()) => {
                    self.raw_registered = true;
                    KEYBOARD.with(|state| state.borrow_mut().raw = true);
                    return;
                }
                Err(error) => {
                    self.warning = Some("Background input could not start. Compatibility mode is active for this session.".into());
                    log(
                        &self.directory,
                        &format!("Background keyboard registration failed: {error}"),
                    );
                }
            }
        }
        self.polling = unsafe { SetTimer(Some(self.window), POLL_TIMER, 15, None) } != 0;
        if !self.polling {
            self.report("Could not start compatibility keyboard polling.");
        }
    }

    fn connect(&mut self) -> Result<&mut Endpoint> {
        let id = self
            .settings
            .device_id
            .as_deref()
            .context("Choose your headset in Settings.")?;
        if self.endpoint.as_ref().map(|endpoint| endpoint.id.as_str()) != Some(id) {
            self.endpoint = None;
            self.watched = false;
            self.watch_retry = Instant::now();
            self.endpoint = Some(Endpoint::open(id)?);
        }
        self.endpoint
            .as_ref()
            .expect("selected endpoint was opened")
            .ensure_active()?;
        Ok(self
            .endpoint
            .as_mut()
            .expect("selected endpoint was opened"))
    }

    fn refresh(&mut self) {
        // Read recovery once per update, and do not tear down COM on a storage error.
        let backup = self
            .settings
            .device_id
            .as_deref()
            .map(|id| self.store.find(id))
            .transpose();
        let backup = match backup {
            Ok(backup) => backup.flatten(),
            Err(error) => {
                self.snapshot.warning = Some(format!("{error:#}"));
                self.update_tray();
                self.refresh_ui();
                return;
            }
        };
        let result = (|| -> Result<Snapshot> {
            self.connect()?;
            let endpoint = self
                .endpoint
                .as_mut()
                .expect("selected endpoint was opened");
            if !self.read_only {
                volume::maintain(endpoint, backup.as_ref())?;
            }
            let snapshot = endpoint.snapshot_with_backup(backup.as_ref())?;
            if !self.watched && Instant::now() >= self.watch_retry {
                match endpoint.watch(self.window, VOLUME_MESSAGE, self.pending.clone()) {
                    Ok(()) => self.watched = true,
                    Err(error) => {
                        self.watch_retry = Instant::now() + Duration::from_secs(30);
                        log(
                            &self.directory,
                            &format!("Could not watch microphone volume: {error:#}"),
                        );
                    }
                }
            }
            Ok(snapshot)
        })();
        self.snapshot = match result {
            Ok(snapshot) => {
                self.last_error = None;
                snapshot
            }
            Err(error) => {
                self.endpoint = None;
                self.watched = false;
                Snapshot {
                    name: self
                        .settings
                        .device_name
                        .clone()
                        .unwrap_or_else(|| "Choose your headset in Settings".into()),
                    warning: Some(format!("{error:#}")),
                    ..Snapshot::default()
                }
            }
        };
        self.update_tray();
        self.refresh_ui();
    }

    fn refresh_ui(&mut self) {
        let Some(ui) = &self.ui else {
            return;
        };
        if !ui.visible() {
            return;
        }
        let selected = ui.selected_device();
        let preview = if let Some(device) =
            selected.filter(|device| Some(&device.id) != self.settings.device_id.as_ref())
        {
            let result = (|| -> Result<Snapshot> {
                if self.preview.as_ref().map(|endpoint| endpoint.id.as_str()) != Some(&device.id) {
                    self.preview = Some(Endpoint::open(&device.id)?);
                }
                self.preview.as_ref().unwrap().snapshot(&self.store)
            })();
            Some(result.unwrap_or_else(|error| {
                self.preview = None;
                Snapshot {
                    name: device.name.clone(),
                    warning: Some(format!("{error:#}")),
                    ..Snapshot::default()
                }
            }))
        } else {
            self.preview = None;
            selected.is_none().then(|| Snapshot {
                name: "Choose your headset in Settings".into(),
                ..Snapshot::default()
            })
        };
        ui.set_status(
            preview.as_ref().unwrap_or(&self.snapshot),
            self.warning.as_deref(),
            self.read_only,
        );
    }

    fn update_tray(&mut self) {
        let (icon, state) = if !self.snapshot.available {
            (3, "Unavailable")
        } else if self.snapshot.warning.is_some() {
            (3, "Needs attention")
        } else if self.snapshot.volume_minimized {
            (2, "Volume minimized")
        } else if self.snapshot.muted {
            (1, "Windows reports muted")
        } else {
            (0, "Active")
        };
        let tip = format!(
            "MuteMic - {state} - {}\n{}",
            input::key_label(self.settings.hotkey),
            self.snapshot.name
        );
        if icon != self.last_icon || tip != self.last_tip {
            self.tray.hIcon = self.icons[icon];
            copy_text(&mut self.tray.szTip, &tip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.tray);
            }
            self.last_icon = icon;
            self.last_tip = tip;
        }
    }

    fn toggle(&mut self) {
        if self.read_only {
            return;
        }
        if self.settings.device_id.is_none() {
            self.show_settings();
            return;
        }
        let mode = self.settings.mute_mode;
        let result = (|| {
            self.connect()?;
            self.endpoint
                .as_mut()
                .expect("selected endpoint was opened")
                .toggle(mode, &self.store)
        })();
        match result {
            Ok(snapshot) => {
                if self.settings.play_sounds {
                    let sound = if snapshot.muted {
                        &self.muted_sound
                    } else {
                        &self.active_sound
                    };
                    unsafe {
                        let _ = PlaySoundW(
                            PCWSTR(sound.as_ptr().cast()),
                            None,
                            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
                        );
                    }
                }
                log(
                    &self.directory,
                    if snapshot.volume_minimized {
                        "Microphone volume minimized"
                    } else if snapshot.muted {
                        "Windows reports muted"
                    } else {
                        "Microphone active"
                    },
                );
                self.snapshot = snapshot;
                self.update_tray();
                self.refresh_ui();
            }
            Err(error) => {
                self.refresh();
                self.report(&format!(
                    "Could not toggle the selected microphone: {error:#}"
                ));
            }
        }
    }

    fn show_settings(&mut self) {
        let result = (|| -> Result<()> {
            if self.ui.as_ref().is_some_and(|ui| ui.visible()) {
                unsafe {
                    let window = self.ui.as_ref().unwrap().window;
                    let _ = ShowWindow(window, SW_RESTORE);
                    let _ = SetForegroundWindow(window);
                }
                return Ok(());
            }
            let devices = audio::devices()?;
            if self.ui.is_none() {
                self.ui = Some(Ui::new(self.instance, self.window)?);
            }
            self.ui
                .as_mut()
                .unwrap()
                .open(&self.settings, devices, self.read_only)?;
            self.refresh_ui();
            Ok(())
        })();
        if let Err(error) = result {
            self.report(&format!("Could not open settings: {error:#}"));
        }
    }

    fn save(&mut self) -> Result<()> {
        if self.read_only {
            bail!("Settings cannot be saved in read-only profiling mode.");
        }
        let settings = self
            .ui
            .as_ref()
            .context("Settings are not open.")?
            .draft()?;
        if let Some(id) = &self.settings.device_id
            && self.store.find(id)?.is_some()
            && (self.settings.device_id != settings.device_id
                || self.settings.mute_mode != settings.mute_mode)
        {
            bail!(
                "Restore the current microphone volume before changing microphone or mute method."
            );
        }
        if settings.start_with_windows != self.settings.start_with_windows {
            startup::save(
                &self.executable,
                &self.directory,
                settings.start_with_windows,
                || settings.save(&self.directory),
            )?;
        } else {
            settings.save(&self.directory)?;
        }
        let input_changed = self.settings.hotkey != settings.hotkey
            || self.settings.input_mode != settings.input_mode;
        let device_changed = self.settings.device_id != settings.device_id;
        self.settings = settings;
        self.warning = None;
        if device_changed {
            self.endpoint = None;
            self.watched = false;
        }
        if input_changed {
            self.configure_input();
        }
        self.preview = None;
        self.refresh();
        self.ui.as_ref().unwrap().hide();
        Ok(())
    }

    fn report(&mut self, message: &str) {
        if self.last_error.as_deref() == Some(message) {
            return;
        }
        self.last_error = Some(message.into());
        log(&self.directory, message);
        let mut balloon = self.tray;
        balloon.uFlags = NIF_INFO;
        balloon.dwInfoFlags = NIIF_WARNING;
        copy_text(&mut balloon.szInfoTitle, "MuteMic needs attention");
        copy_text(&mut balloon.szInfo, message);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &balloon);
        }
    }

    fn menu(&mut self) {
        self.refresh();
        let label = if self.snapshot.volume_minimized {
            "Restore microphone volume"
        } else if self.snapshot.muted {
            "Unmute microphone"
        } else if self.settings.mute_mode == crate::storage::MuteMode::VolumeZero {
            "Minimize microphone volume"
        } else {
            "Mute microphone"
        };
        self.effect = Some(Effect::Menu {
            window: self.window,
            enabled: self.snapshot.available && !self.read_only,
            label,
        });
    }

    fn command(&mut self, id: u16, notification: u16) {
        match id {
            ui::TOGGLE => self.toggle(),
            ui::SETTINGS => self.show_settings(),
            ui::EXIT => unsafe {
                PostQuitMessage(0);
            },
            ui::SAVE | 1 => {
                if let Err(error) = self.save() {
                    self.effect = Some(Effect::Error(format!(
                        "Could not save settings.\n\n{error:#}"
                    )));
                }
            }
            ui::CANCEL | 2 => {
                self.preview = None;
                if let Some(ui) = &self.ui {
                    ui.hide();
                }
            }
            ui::REFRESH => match audio::devices() {
                Ok(devices) => {
                    if let Some(ui) = &mut self.ui {
                        ui.refresh_devices(&self.settings, devices);
                    }
                    self.refresh_ui();
                }
                Err(error) => self.report(&format!("Could not list microphones: {error:#}")),
            },
            ui::DEVICE if notification == CBN_SELCHANGE as u16 => self.refresh_ui(),
            ui::MINIMUM => self.refresh_ui(),
            ui::RECORD => {
                if let Some(ui) = &mut self.ui
                    && let Err(error) = ui.begin_recording()
                {
                    self.effect = Some(Effect::Error(format!(
                        "Could not record shortcut: {error:#}"
                    )));
                }
                self.configure_input();
            }
            ui::WINDOWS_SETTINGS => unsafe {
                let _ = ShellExecuteW(
                    Some(self.window),
                    w!("open"),
                    w!("control.exe"),
                    w!("mmsys.cpl,,1"),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                );
            },
            _ => (),
        }
    }

    fn message(
        &mut self,
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        if message == self.settings_message {
            self.show_settings();
            return Some(LRESULT(0));
        }
        if message == self.taskbar_message {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_ADD, &self.tray);
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &self.tray);
            }
            return Some(LRESULT(0));
        }
        match message {
            WM_POWERBROADCAST | WM_WTSSESSION_CHANGE => self.reset_input(),
            WM_TIMER if wparam.0 == STATUS_TIMER => self.refresh(),
            WM_TIMER if wparam.0 == POLL_TIMER && self.polling && !self.recording() => {
                let down = self.held();
                let toggle = KEYBOARD.with(|state| {
                    let mut state = state.borrow_mut();
                    let toggle = state.tracker.process(0, down);
                    state.tracker.release_wait(!down);
                    toggle
                });
                if toggle {
                    unsafe {
                        let _ =
                            PostMessageW(Some(self.window), TOGGLE_MESSAGE, WPARAM(0), LPARAM(0));
                    }
                }
            }
            TOGGLE_MESSAGE => self.toggle(),
            VOLUME_MESSAGE => {
                self.pending.store(false, Ordering::Release);
                self.refresh();
            }
            TRAY_MESSAGE => match lparam.0 as u32 & 0xffff {
                WM_LBUTTONUP | NIN_SELECT | TRAY_KEY_SELECT => self.show_settings(),
                WM_RBUTTONUP | WM_CONTEXTMENU => self.menu(),
                _ => (),
            },
            WM_COMMAND => self.command(
                (wparam.0 & 0xffff) as u16,
                ((wparam.0 >> 16) & 0xffff) as u16,
            ),
            WM_KEYDOWN | WM_SYSKEYDOWN
                if self.ui.as_ref().and_then(|ui| ui.recorder) == Some(window) =>
            {
                let key = wparam.0 as u16;
                let modifiers = unsafe {
                    [VK_CONTROL, VK_MENU, VK_SHIFT]
                        .into_iter()
                        .any(|key| GetAsyncKeyState(key.0 as i32) < 0)
                };
                if key == VK_ESCAPE.0 || (!modifiers && input::supported(key)) {
                    self.ui
                        .as_mut()
                        .unwrap()
                        .end_recording((key != VK_ESCAPE.0).then_some(key));
                    self.configure_input();
                }
            }
            WM_CLOSE => {
                if window == self.window {
                    unsafe {
                        PostQuitMessage(0);
                    }
                } else if self.ui.as_ref().and_then(|ui| ui.recorder) == Some(window) {
                    self.ui.as_mut().unwrap().end_recording(None);
                    self.configure_input();
                } else if let Some(ui) = &self.ui {
                    ui.hide();
                    self.preview = None;
                }
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

fn tray_menu(window: HWND, enabled: bool, label: &str) -> Result<()> {
    unsafe {
        let menu = CreatePopupMenu()?;
        let result = (|| -> Result<()> {
            let flags = if enabled {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            AppendMenuW(
                menu,
                flags,
                ui::TOGGLE as usize,
                PCWSTR(wide(label).as_ptr()),
            )?;
            AppendMenuW(menu, MF_STRING, ui::SETTINGS as usize, w!("Settings..."))?;
            AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
            AppendMenuW(menu, MF_STRING, ui::EXIT as usize, w!("Exit"))?;
            let mut point = POINT::default();
            GetCursorPos(&mut point)?;
            let _ = SetForegroundWindow(window);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                window,
                None,
            )
            .0 as usize;
            if selected != 0 {
                let _ = PostMessageW(Some(window), WM_COMMAND, WPARAM(selected), LPARAM(0));
            }
            let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
            Ok(())
        })();
        let _ = DestroyMenu(menu);
        result
    }
}

impl Drop for App {
    fn drop(&mut self) {
        KEYBOARD.with(|state| state.borrow_mut().raw = false);
        self.endpoint = None;
        self.ui = None;
        unsafe {
            let _ = PlaySoundW(PCWSTR::null(), None, SND_FLAGS(0));
            let _ = KillTimer(Some(self.window), STATUS_TIMER);
            let _ = KillTimer(Some(self.window), POLL_TIMER);
            if self.raw_registered {
                let _ = RegisterRawInputDevices(
                    &[RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 6,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: HWND::default(),
                    }],
                    size_of::<RAWINPUTDEVICE>() as u32,
                );
            }
            let _ = WTSUnRegisterSessionNotification(self.window);
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.tray);
            for icon in self.icons {
                let _ = DestroyIcon(icon);
            }
            let _ = DestroyWindow(self.window);
        }
        log(&self.directory, "Rust application stopped");
    }
}

fn copy_text<const N: usize>(target: &mut [u16; N], text: &str) {
    target.fill(0);
    let mut offset = 0;
    for character in text.chars() {
        let length = character.len_utf16();
        if offset + length >= N {
            break;
        }
        character.encode_utf16(&mut target[offset..offset + length]);
        offset += length;
    }
}

fn tone(start: f64, end: f64) -> Vec<u8> {
    const RATE: u32 = 22050;
    const COUNT: u32 = RATE / 8;
    let mut bytes = Vec::with_capacity(44 + COUNT as usize * 2);
    bytes.extend(b"RIFF");
    bytes.extend((36 + COUNT * 2).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(RATE.to_le_bytes());
    bytes.extend((RATE * 2).to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend((COUNT * 2).to_le_bytes());
    let mut phase = 0.0;
    for index in 0..COUNT {
        let progress = index as f64 / COUNT as f64;
        phase += std::f64::consts::TAU * (start + (end - start) * progress) / RATE as f64;
        let envelope = (index as f64 / 120.0).min(1.0) * ((COUNT - index) as f64 / 400.0).min(1.0);
        bytes.extend(((phase.sin() * envelope * 3200.0) as i16).to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_aliases_share_one_instance_identity() {
        let directory = crate::storage::tests::temp_dir();
        let child = directory.join("child");
        fs::create_dir(&child).unwrap();
        assert_eq!(
            data_directory(&directory).unwrap(),
            data_directory(&child.join("..")).unwrap()
        );
        assert_eq!(
            data_directory(&directory).unwrap(),
            data_directory(&directory.join("")).unwrap()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn nested_notifications_are_coalesced_without_losing_shortcut_actions() {
        let mut queue = DeferredMessages::default();
        let event = |id| Message {
            window: HWND::default(),
            id,
            wparam: WPARAM(0),
            lparam: LPARAM(0),
        };
        queue.push(event(TOGGLE_MESSAGE));
        for _ in 0..1000 {
            queue.push(event(VOLUME_MESSAGE));
        }
        queue.push(event(TOGGLE_MESSAGE));
        queue.push(event(WM_CLOSE));
        assert_eq!(
            queue.0.iter().map(|message| message.id).collect::<Vec<_>>(),
            vec![TOGGLE_MESSAGE, VOLUME_MESSAGE, TOGGLE_MESSAGE, WM_CLOSE]
        );
        assert!(!queue.push(event(WM_INPUT)));
        assert!(!queue.push(event(WM_WINDOWPOSCHANGED)));
        assert!(!queue.push(event(WM_KEYDOWN)));
    }

    #[test]
    fn reentrant_close_is_handled_without_destroying_the_window_early() {
        DEFERRED.with(|queue| queue.borrow_mut().0.clear());
        APP.with(|slot| {
            let _borrow = slot.borrow_mut();
            // An invalid HWND is intentional: deferred close must never reach DefWindowProc.
            assert_eq!(
                unsafe { window_proc(HWND::default(), WM_CLOSE, WPARAM(0), LPARAM(0)) },
                LRESULT(0)
            );
        });
        DEFERRED.with(|queue| {
            let mut queue = queue.borrow_mut();
            assert_eq!(queue.0.pop_front().unwrap().id, WM_CLOSE);
            assert!(queue.0.is_empty());
        });
    }

    #[test]
    fn shell_text_truncation_preserves_surrogate_pairs() {
        let mut target = [0; 3];
        copy_text(&mut target, "a😀");
        assert_eq!(String::from_utf16(&target[..1]).unwrap(), "a");
        assert_eq!(target[1..], [0, 0]);
        copy_text(&mut target, "😀b");
        assert_eq!(String::from_utf16(&target[..2]).unwrap(), "😀");
        assert_eq!(target[2], 0);
    }
}
