//! Worker tray controls run on a separate Win32 message thread.
#![allow(unsafe_code)]
use anyhow::{Result, ensure};
use std::{cell::RefCell, sync::mpsc};
use windows_sys::Win32::{
    Foundation::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};
#[derive(Debug, Clone, Copy)]
pub enum Action {
    Open,
    Pause,
    Resume,
    Exit,
}
thread_local! {
    static ACTIONS: RefCell<Option<mpsc::Sender<Action>>> = const { RefCell::new(None) };
    static PAUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ICON: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
const CALLBACK: u32 = WM_APP + 1;
const PAUSE_STATE: u32 = WM_APP + 2;
pub struct Tray {
    window: isize,
    paused: std::cell::Cell<Option<bool>>,
}
impl Tray {
    pub fn set_paused(&self, paused: bool) {
        if self.paused.replace(Some(paused)) == Some(paused) {
            return;
        }
        // SAFETY: this value-only message targets the hidden window owned by this tray.
        unsafe {
            PostMessageW(self.window as HWND, PAUSE_STATE, usize::from(paused), 0);
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window as HWND,
            uID: 1,
            ..Default::default()
        };
        // SAFETY: data names the tray icon owned by this worker and stays live for the call.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
        // SAFETY: PostMessage accepts this thread-owned window handle and value-only parameters.
        unsafe {
            PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
        }
    }
}
fn publish(action: Action) {
    ACTIONS.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            let _sent = sender.send(action);
        }
    });
}
unsafe extern "system" fn procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if TASKBAR_CREATED.with(|slot| slot.get() == message && message != 0) {
        ICON.with(|slot| {
            if let Some(data) = slot.borrow().as_ref() {
                // SAFETY: the owning message thread retains live icon data and its window.
                unsafe {
                    Shell_NotifyIconW(NIM_ADD, data);
                }
            }
        });
        return 0;
    }
    match message {
        PAUSE_STATE => {
            PAUSED.with(|slot| slot.set(wparam != 0));
            0
        }
        CALLBACK => {
            if lparam as u32 == WM_LBUTTONDBLCLK {
                publish(Action::Open);
            } else if lparam as u32 == WM_RBUTTONUP
                && let Err(error) = popup(window)
            {
                tracing::warn!(%error, "Tray menu could not open");
            }
            0
        }
        WM_CLOSE => {
            // SAFETY: this is the live window receiving WM_CLOSE on its owning thread.
            unsafe {
                DestroyWindow(window);
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: PostQuitMessage posts a value-only quit request to this message thread.
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => {
            // SAFETY: forwarding the unmodified message to the standard window procedure is required.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
    }
}
fn popup(window: HWND) -> Result<()> {
    // SAFETY: CreatePopupMenu creates a uniquely owned menu without pointers.
    let menu = unsafe { CreatePopupMenu() };
    ensure!(!menu.is_null(), "Cannot create tray menu");
    struct Menu(HMENU);
    impl Drop for Menu {
        fn drop(&mut self) {
            // SAFETY: this menu was created by CreatePopupMenu and is destroyed once.
            unsafe {
                DestroyMenu(self.0);
            }
        }
    }
    let _owned = Menu(menu);
    for (id, label) in [
        (1, "Open Flummox"),
        if PAUSED.with(|slot| slot.get()) {
            (3, "Resume background work")
        } else {
            (2, "Pause background work")
        },
        (4, "Exit"),
    ] {
        let label: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
        // SAFETY: menu is live and label is terminated and copied by AppendMenuW.
        let result = unsafe { AppendMenuW(menu, MF_STRING, id, label.as_ptr()) };
        ensure!(result != 0, "Cannot add tray action");
    }
    let mut point = POINT::default();
    // SAFETY: point is writable for one POINT.
    unsafe {
        GetCursorPos(&mut point);
    }
    // SAFETY: the hidden top-level window owns this popup on its message thread.
    unsafe {
        SetForegroundWindow(window);
    }
    // SAFETY: the live menu and owner window remain valid while the modal popup runs.
    let chosen = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY,
            point.x,
            point.y,
            0,
            window,
            std::ptr::null(),
        )
    };
    match chosen {
        1 => publish(Action::Open),
        2 => publish(Action::Pause),
        3 => publish(Action::Resume),
        4 => publish(Action::Exit),
        _ => {}
    }
    // SAFETY: this value-only message completes popup focus handling for the live owner.
    unsafe {
        PostMessageW(window, WM_NULL, 0, 0);
    }
    Ok(())
}
pub fn spawn() -> Result<(Tray, mpsc::Receiver<Action>)> {
    let (sender, receiver) = mpsc::channel();
    let (ready, result) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run(sender, ready.clone());
        if let Err(error) = result {
            let _sent = ready.send(Err(error.to_string()));
        }
    });
    let window = result
        .recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok((
        Tray {
            window,
            paused: std::cell::Cell::new(None),
        },
        receiver,
    ))
}
fn run(
    sender: mpsc::Sender<Action>,
    ready: mpsc::Sender<std::result::Result<isize, String>>,
) -> Result<()> {
    ACTIONS.with(|slot| *slot.borrow_mut() = Some(sender));
    let taskbar: Vec<u16> = "TaskbarCreated".encode_utf16().chain(Some(0)).collect();
    // SAFETY: the registered message name is terminated and copied by Windows.
    let taskbar = unsafe { RegisterWindowMessageW(taskbar.as_ptr()) };
    ensure!(taskbar != 0, "Cannot register tray restart notification");
    TASKBAR_CREATED.with(|slot| slot.set(taskbar));
    let class: Vec<u16> = "FlummoxWorkerTray".encode_utf16().chain(Some(0)).collect();
    // SAFETY: null requests the currently executing module's handle.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let definition = WNDCLASSW {
        lpfnWndProc: Some(procedure),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: class is terminated and the procedure remains valid for the process lifetime.
    let registered = unsafe { RegisterClassW(&definition) };
    ensure!(registered != 0, "Cannot register tray window");
    // SAFETY: class is registered, strings are terminated and the hidden top-level window takes no creation pointer.
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    ensure!(!window.is_null(), "Cannot create tray window");
    // SAFETY: IDI_APPLICATION is a system-owned resource identifier and null selects system resources.
    let icon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>())?,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    for (unit, character) in data
        .szTip
        .iter_mut()
        .zip("Flummox background worker".encode_utf16())
    {
        *unit = character;
    }
    // SAFETY: the icon data points to a live window and its terminated tooltip is in the zero-initialized buffer.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
    ensure!(added != 0, "Cannot create worker tray icon");
    ICON.with(|slot| *slot.borrow_mut() = Some(data));
    let _sent = ready.send(Ok(window as isize));
    let mut message = MSG::default();
    loop {
        // SAFETY: message is writable and null selects all messages for this owning thread.
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        // SAFETY: message was initialized by GetMessageW.
        unsafe {
            TranslateMessage(&message);
        }
        // SAFETY: message was initialized by GetMessageW and its window belongs to this thread.
        unsafe {
            DispatchMessageW(&message);
        }
    }
    // SAFETY: this removes only the tray icon identified by this worker's window and ID.
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
    }
    Ok(())
}
