//! Win32 pointer ownership is confined to this adapter. No window is shown.
#![allow(unsafe_code)]

use crate::vault_control::VaultController;
use cshell_ipc::VaultSessionState as SessionState;
use std::{
    cell::Cell,
    io, ptr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, SetLastError, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Power::{
            HPOWERNOTIFY, RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification,
        },
        RemoteDesktop::{
            NOTIFY_FOR_THIS_SESSION, ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTSActive,
            WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW, WTSRegisterSessionNotification,
            WTSSessionInfoEx, WTSUnRegisterSessionNotification,
        },
        Threading::{GetCurrentProcessId, GetCurrentThreadId},
    },
    UI::WindowsAndMessaging::*,
};

const TIMER_ID: usize = 1;
const OBSERVATION_INTERVAL_MS: u32 = 1_000;
// Present in the documented WM_WTSSESSION_CHANGE contract, omitted by this SDK binding.
const WTS_SESSION_DESKTOP_READY: u32 = 0x0f;

pub(super) struct WindowsSessionMonitor {
    stopped: Arc<AtomicBool>,
    thread_id: Arc<Mutex<Option<u32>>>,
    worker: Option<JoinHandle<()>>,
}
impl WindowsSessionMonitor {
    pub(super) fn start(vault: &Arc<VaultController>) -> io::Result<Self> {
        // Report unavailable until registration and a WTS query succeed.
        // Observations do not change the user-controlled Vault lifetime.
        vault.observe_desktop_session(SessionState::Unavailable);
        let weak = Arc::downgrade(vault);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(Mutex::new(None));
        let stop = stopped.clone();
        let id = thread_id.clone();
        let worker = thread::Builder::new()
            .name("cshell-session-lock".into())
            .spawn(move || {
                let _identity = ThreadIdentity {
                    id: id.clone(),
                    vault: weak.clone(),
                };
                // SAFETY: MSG is a fully initialized Win32 structure and PeekMessage
                // creates a queue on this thread, before publishing its thread ID.
                unsafe {
                    let mut message = MSG::default();
                    PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
                    *id.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(GetCurrentThreadId());
                }
                while !stop.load(Ordering::Acquire) {
                    if run_window(weak.clone(), stop.clone()).is_err() {
                        observe(&weak, SessionState::Unavailable);
                        // One owned thread, bounded retry rate; only monitor setup is retried.
                        thread::park_timeout(Duration::from_secs(2));
                    }
                }
                observe(&weak, SessionState::Unavailable);
            })?;
        Ok(Self {
            stopped,
            thread_id,
            worker: Some(worker),
        })
    }
}
impl Drop for WindowsSessionMonitor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            {
                // The exit guard uses this same mutex. Holding it through the
                // post prevents sending WM_QUIT to an OS-reused thread ID.
                let id = self
                    .thread_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(id) = *id {
                    // SAFETY: PostThreadMessage holds no caller-owned pointers. The
                    // queue belongs to the owned thread; failure is covered by its timer.
                    unsafe {
                        PostThreadMessageW(id, WM_QUIT, 0, 0);
                    }
                }
            }
            let _joined = worker.join();
        }
    }
}
struct ThreadIdentity {
    id: Arc<Mutex<Option<u32>>>,
    vault: Weak<VaultController>,
}
impl Drop for ThreadIdentity {
    fn drop(&mut self) {
        observe(&self.vault, SessionState::Unavailable);
        *self
            .id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}
fn observe(vault: &Weak<VaultController>, state: SessionState) {
    if let Some(vault) = vault.upgrade() {
        vault.observe_desktop_session(state);
    }
}
struct Context {
    vault: Weak<VaultController>,
    session_id: u32,
    suspended: Cell<bool>,
    stopped: Arc<AtomicBool>,
}
impl Context {
    fn refresh(&self) {
        if self.suspended.get() {
            observe(&self.vault, SessionState::Suspended);
        } else {
            observe(&self.vault, query_session(self.session_id));
        }
    }
}
struct NativeWindow {
    hwnd: HWND,
    instance: HINSTANCE,
    class_name: Vec<u16>,
    // Its allocation is stable from WM_NCCREATE until DestroyWindow returns.
    _context: Box<Context>,
    registered: bool,
    power_notification: HPOWERNOTIFY,
    timer: bool,
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        observe(&self._context.vault, SessionState::Unavailable);
        // SAFETY: All resources were created on the current thread. The callback
        // clears GWLP_USERDATA during WM_NCDESTROY before the Box is released.
        unsafe {
            if self.timer {
                KillTimer(self.hwnd, TIMER_ID);
            }
            if self.registered {
                WTSUnRegisterSessionNotification(self.hwnd);
            }
            if self.power_notification != 0 {
                UnregisterSuspendResumeNotification(self.power_notification);
            }
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class_name.as_ptr(), self.instance);
        }
    }
}
fn run_window(vault: Weak<VaultController>, stopped: Arc<AtomicBool>) -> io::Result<()> {
    let mut session_id = 0;
    // SAFETY: The API writes one initialized DWORD to a live stack location.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let context = Box::new(Context {
        vault,
        session_id,
        suspended: Cell::new(false),
        stopped,
    });
    let class_name: Vec<u16> = format!("CShellSessionLock-{:016x}", rand::random::<u64>())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: Null module name queries the module already loaded in this process.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        return Err(io::Error::last_os_error());
    }
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class_name.as_ptr(),
        ..Default::default()
    };
    // SAFETY: Class name and callback remain live until unregistering the class.
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // A hidden top-level window receives power broadcasts. HWND_MESSAGE would
    // suppress those broadcasts. No WS_VISIBLE or ShowWindow is used.
    // SAFETY: Context remains boxed throughout the entire window lifetime.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::from_ref(context.as_ref()).cast(),
        )
    };
    if hwnd.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: No window survived creation. Its class is owned by this thread.
        unsafe {
            UnregisterClassW(class_name.as_ptr(), instance);
        }
        return Err(error);
    }
    let mut window = NativeWindow {
        hwnd,
        instance,
        class_name,
        _context: context,
        registered: false,
        power_notification: 0,
        timer: false,
    };
    // SAFETY: HWND belongs to this thread and is valid until window's Drop.
    if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    window.registered = true;
    // SAFETY: Register a valid owned HWND, never a callback pointer. Its handle
    // is retired before DestroyWindow on the same thread.
    window.power_notification =
        unsafe { RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) };
    if window.power_notification == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The timer is attached to the live window, with no callback pointer.
    if unsafe { SetTimer(hwnd, TIMER_ID, OBSERVATION_INTERVAL_MS, None) } == 0 {
        return Err(io::Error::last_os_error());
    }
    window.timer = true;
    window._context.refresh();
    while !window._context.stopped.load(Ordering::Acquire) {
        let mut message = MSG::default();
        // SAFETY: GetMessage writes to a valid MSG and filters the current thread queue.
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            break;
        }
        // SAFETY: message came from GetMessage and callbacks use the boxed context.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
fn query_session(session_id: u32) -> SessionState {
    let mut buffer = ptr::null_mut();
    let mut length = 0;
    // SAFETY: Both out pointers are live. The returned buffer is owned by WTS.
    let success = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSSessionInfoEx,
            &mut buffer,
            &mut length,
        )
    };
    if success == 0 || buffer.is_null() {
        if !buffer.is_null() {
            // SAFETY: WTS allocated this buffer, including on a partial failure.
            unsafe {
                WTSFreeMemory(buffer.cast());
            }
        }
        return SessionState::Unavailable;
    }
    let state = if length as usize >= std::mem::size_of::<WTSINFOEXW>() {
        // SAFETY: The API's information class guarantees this layout. A bounded
        // unaligned copy avoids relying on undocumented allocation alignment.
        let info = unsafe { ptr::read_unaligned(buffer.cast::<WTSINFOEXW>()) };
        if info.Level == 1 {
            // SAFETY: Level 1 selects the WTSInfoExLevel1 union member.
            let current = unsafe { info.Data.WTSInfoExLevel1 };
            if current.SessionId != session_id {
                SessionState::Unavailable
            } else if current.SessionState != WTSActive {
                SessionState::Disconnected
            } else {
                match current.SessionFlags {
                    0 => SessionState::Locked,
                    1 => SessionState::Active,
                    _ => SessionState::Unavailable,
                }
            }
        } else {
            SessionState::Unavailable
        }
    } else {
        SessionState::Unavailable
    };
    // SAFETY: Exactly one free for the allocation from WTSQuerySessionInformation.
    unsafe {
        WTSFreeMemory(buffer.cast());
    }
    state
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    parameter: WPARAM,
    value: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // An already-bound window must never replace its context from a message.
        if unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } != 0 || value == 0 {
            return 0;
        }
        // SAFETY: Win32 supplies a CREATESTRUCTW for WM_NCCREATE. The caller's
        // Box<Context> remains alive until after WM_NCDESTROY.
        let creation = unsafe { &*(value as *const CREATESTRUCTW) };
        if creation.lpCreateParams.is_null() {
            return 0;
        }
        // SAFETY: GWLP_USERDATA is an owned window field. Zero is the previous
        // value on success; distinguish it from failure via the Win32 last error.
        unsafe {
            SetLastError(0);
        }
        if unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, creation.lpCreateParams as isize) } == 0
            && unsafe { GetLastError() } != 0
        {
            return 0;
        }
        return 1;
    }
    // SAFETY: This field is populated only by WM_NCCREATE on our registered class.
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Context;
    if !pointer.is_null() {
        // SAFETY: Box ownership belongs to NativeWindow; callback access is shared,
        // on the same thread. Interior flags use Cell to allow reentrant callbacks.
        let context = unsafe { &*pointer };
        match message {
            WM_CLOSE => {
                // Teardown belongs to the RAII owner after the message loop.
                unsafe {
                    PostQuitMessage(0);
                }
                return 0;
            }
            WM_WTSSESSION_CHANGE if value as u32 == context.session_id => {
                match parameter as u32 {
                    WTS_SESSION_LOCK => observe(&context.vault, SessionState::Locked),
                    WTS_SESSION_LOGOFF | WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT => {
                        observe(&context.vault, SessionState::Disconnected);
                    }
                    WTS_SESSION_UNLOCK
                    | WTS_SESSION_LOGON
                    | WTS_CONSOLE_CONNECT
                    | WTS_REMOTE_CONNECT
                    | WTS_SESSION_DESKTOP_READY => context.refresh(),
                    _ => {}
                }
                return 0;
            }
            WM_POWERBROADCAST => {
                match parameter as u32 {
                    PBT_APMSUSPEND => {
                        context.suspended.set(true);
                        observe(&context.vault, SessionState::Suspended);
                    }
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL => {
                        context.suspended.set(false);
                        observe(&context.vault, SessionState::Unavailable);
                        context.refresh();
                    }
                    _ => {}
                }
                return 1;
            }
            WM_TIMER if parameter == TIMER_ID => {
                if context.stopped.load(Ordering::Acquire) {
                    // SAFETY: Requests termination of this thread's message loop.
                    unsafe {
                        PostQuitMessage(0);
                    }
                } else {
                    context.refresh();
                }
                return 0;
            }
            WM_NCDESTROY => {
                // SAFETY: Stop exposing the context before its Box can be dropped.
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
            }
            _ => {}
        }
    }
    // SAFETY: All unhandled messages are delegated to the system window procedure.
    unsafe { DefWindowProcW(hwnd, message, parameter, value) }
}
