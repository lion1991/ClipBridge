//! Event-driven clipboard listener using `AddClipboardFormatListener`.
//!
//! Spawns a worker thread that owns a hidden message-only window. The
//! window receives `WM_CLIPBOARDUPDATE` whenever the system clipboard
//! changes; the user-supplied callback is invoked on that thread.
//!
//! Lifecycle: dropping the `ClipboardListener` posts `WM_USER_STOP` to the
//! worker, which exits its message loop, removes the listener, and tears
//! down the window. The Drop impl waits for the worker to join before
//! returning.

use std::{
    io,
    sync::{mpsc, Arc},
    thread::JoinHandle,
};

use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{
            DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener},
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
            GetWindowLongPtrW, PostMessageW, RegisterClassExW, SetWindowLongPtrW,
            TranslateMessage, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
            WM_USER, WNDCLASSEXW,
        },
    },
};

// WM_CLIPBOARDUPDATE isn't currently re-exported by the `windows` crate's
// public surface; the value is fixed at 0x031D since Vista.
const WM_CLIPBOARDUPDATE: u32 = 0x031D;
const WM_USER_STOP: u32 = WM_USER + 1;

type CallbackFn = dyn Fn() + Send + Sync;

struct CallbackBox {
    cb: Arc<CallbackFn>,
}

pub struct ClipboardListener {
    thread: Option<JoinHandle<()>>,
    stop_hwnd: isize,
}

impl ClipboardListener {
    /// Spawn the listener thread. The callback fires on every clipboard
    /// change. Returns `Err` if the message-only window or listener
    /// registration fails — the caller can then fall back to polling.
    pub fn start<F>(callback: F) -> io::Result<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let callback: Arc<CallbackFn> = Arc::new(callback);
        let (hwnd_tx, hwnd_rx) = mpsc::channel::<isize>();

        let cb_for_thread = callback.clone();
        let thread = std::thread::Builder::new()
            .name("clipbridge-clipboard-listener".into())
            .spawn(move || run_listener_thread(cb_for_thread, hwnd_tx))?;

        let stop_hwnd = hwnd_rx
            .recv()
            .map_err(|e| io::Error::other(format!("listener init: {e}")))?;
        if stop_hwnd == 0 {
            // Worker bailed during setup; let it exit cleanly.
            let _ = thread.join();
            return Err(io::Error::other("listener thread failed to initialise"));
        }

        Ok(Self {
            thread: Some(thread),
            stop_hwnd,
        })
    }
}

impl Drop for ClipboardListener {
    fn drop(&mut self) {
        if self.stop_hwnd != 0 {
            unsafe {
                let hwnd = HWND(self.stop_hwnd as *mut core::ffi::c_void);
                let _ = PostMessageW(Some(hwnd), WM_USER_STOP, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn run_listener_thread(callback: Arc<CallbackFn>, hwnd_tx: mpsc::Sender<isize>) {
    let class_name: PCWSTR = w!("ClipBridgeClipboardListener");
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(_) => {
                let _ = hwnd_tx.send(0);
                return;
            }
        };

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        // Result is ignored: a non-zero ATOM means success, but registering
        // the same class twice during the process lifetime returns an error
        // that we treat as harmless.
        let _ = RegisterClassExW(&wc);

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("ClipBridge Clipboard Listener"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(_) => {
                let _ = hwnd_tx.send(0);
                return;
            }
        };

        // Stash the callback so wnd_proc can find it.
        let boxed = Box::new(CallbackBox { cb: callback });
        let raw = Box::into_raw(boxed);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);

        if AddClipboardFormatListener(hwnd).is_err() {
            // Cleanup partial state before returning.
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(raw));
            let _ = DestroyWindow(hwnd);
            let _ = hwnd_tx.send(0);
            return;
        }

        let _ = hwnd_tx.send(hwnd.0 as isize);

        // Pump messages until WM_USER_STOP arrives or GetMessageW fails.
        let mut msg = MSG::default();
        loop {
            let got = GetMessageW(&mut msg, Some(hwnd), 0, 0);
            if !got.as_bool() {
                break;
            }
            if msg.message == WM_USER_STOP {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup: drop the callback box, unregister the listener,
        // destroy the window.
        let stored = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        if stored != 0 {
            drop(Box::from_raw(stored as *mut CallbackBox));
        }
        let _ = RemoveClipboardFormatListener(hwnd);
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if raw != 0 {
            // SAFETY: pointer was placed by run_listener_thread and is only
            // cleared after the message loop exits, so this dereference is
            // valid for the lifetime of any incoming WM_CLIPBOARDUPDATE.
            let inner = &*(raw as *const CallbackBox);
            (inner.cb)();
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
