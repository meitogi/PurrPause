// Session 3 will install the guard from `modes::service` and drain
// the `PowerEvent` channel ; until then the module has no production
// caller. Remove this attribute at wire-up time.
#![allow(dead_code)]

// Suspend / resume broadcast listener. A hidden message-only window
// registers for `WM_POWERBROADCAST` via
// `RegisterSuspendResumeNotification` and forwards the two events the
// service cares about (`PBT_APMSUSPEND` / `PBT_APMRESUMEAUTOMATIC`) on
// an mpsc channel. Timestamps are captured inside `wndproc` at the
// moment the broadcast lands, not on the receiver side.
//
// Runs on a dedicated thread : the service tick has no message pump,
// so setting `GetMessageW` on the tick thread would either deadlock
// the loop or force a non-blocking pump that competes with the tick
// cadence. A dedicated thread is simpler and lets Drop tear it down
// cleanly with `WM_QUIT`.

use std::sync::mpsc::{channel as std_channel, Sender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification, HPOWERNOTIFY,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PostThreadMessageW, RegisterClassW, TranslateMessage, UnregisterClassW,
    DEVICE_NOTIFY_WINDOW_HANDLE, HMENU, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_POWERBROADCAST, WM_QUIT, WNDCLASSW,
};

use crate::signals::{classify_wm_powerbroadcast, PowerEventKind};

/// Semantic power broadcast forwarded to the service. Timestamp is
/// captured inside `wndproc` — represents the wall-clock at which the
/// broadcast arrived, not the time the receiver drained the channel.
#[derive(Debug)]
pub enum PowerEvent {
    Suspend(SystemTime),
    Resume(SystemTime),
}

/// Handle owned by the caller. Drop unregisters the power notification,
/// tears down the message-only window, and joins the pump thread.
pub struct PowerNotifyGuard {
    hwnd: HwndSend,
    handle: HpowerNotifySend,
    pump_thread_id: u32,
    pump_join: Option<JoinHandle<()>>,
}

// TX is stored in a Mutex-guarded static so the extern "system" wndproc
// (which cannot capture) can reach it. Only one PowerNotifyGuard is
// expected to be live at a time — the second `install` call overwrites
// the sender, and Drop clears it.
static TX: Mutex<Option<Sender<PowerEvent>>> = Mutex::new(None);

/// Create a hidden message-only window, register for suspend / resume
/// broadcasts, and forward matching `PowerEvent`s on `tx`. The pump
/// runs on a dedicated thread. Drop the returned guard to stop
/// listening.
pub fn install(tx: Sender<PowerEvent>) -> Result<PowerNotifyGuard> {
    *TX.lock().unwrap() = Some(tx);

    // The pump thread creates the window (WNDCLASSW must be owned by
    // the thread that owns the message pump) and reports back the HWND
    // + HPOWERNOTIFY + thread id via a one-shot channel.
    let (setup_tx, setup_rx) =
        std_channel::<Result<(HwndSend, HpowerNotifySend, u32), String>>();

    let pump_join = thread::spawn(move || {
        let setup = unsafe { create_message_only_window() };
        match setup {
            Ok((hwnd, handle)) => {
                let tid = unsafe { GetCurrentThreadId() };
                let _ = setup_tx.send(Ok((HwndSend(hwnd), HpowerNotifySend(handle), tid)));
                run_message_pump();
            }
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
            }
        }
    });

    let (hwnd, handle, tid) = setup_rx
        .recv()
        .context("power_notify pump setup channel closed")?
        .map_err(|e| anyhow!("power_notify pump setup failed: {e}"))?;

    Ok(PowerNotifyGuard {
        hwnd,
        handle,
        pump_thread_id: tid,
        pump_join: Some(pump_join),
    })
}

impl Drop for PowerNotifyGuard {
    fn drop(&mut self) {
        unsafe {
            // Stop receiving broadcasts first, then quit the pump so
            // no event lands after the sender is cleared.
            let _ = UnregisterSuspendResumeNotification(self.handle.0);
            let _ = PostThreadMessageW(self.pump_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.pump_join.take() {
            let _ = join.join();
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd.0);
            // Class was registered with GetModuleHandleW(None) — same
            // hinstance is what unregister expects. Ignore failure ; if
            // the process is exiting the class goes away with it.
            if let Ok(hinst) = GetModuleHandleW(None) {
                let _ = UnregisterClassW(CLASS_NAME, Some(HINSTANCE(hinst.0)));
            }
        }
        *TX.lock().unwrap() = None;
    }
}

const CLASS_NAME: windows::core::PCWSTR = w!("PurrPausePwrNotify");

unsafe fn create_message_only_window() -> Result<(HWND, HPOWERNOTIFY)> {
    let hinstance = HINSTANCE(
        GetModuleHandleW(None)
            .context("GetModuleHandleW(None)")?
            .0,
    );
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // Non-zero atom = success ; zero + GetLastError = failure. Second
    // registration with the same name returns a non-fatal error (ok
    // to swallow) — but for a message-only window we do want a fresh
    // class name uniquely per install call. Ignoring the result is
    // safe because CreateWindowExW will fail loud if the class is
    // truly missing.
    let _atom = RegisterClassW(&class);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        CLASS_NAME,
        w!("PurrPause Power Notify"),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        Some(HMENU(std::ptr::null_mut())),
        Some(hinstance),
        None,
    )
    .context("CreateWindowExW(HWND_MESSAGE)")?;

    let handle = RegisterSuspendResumeNotification(
        HANDLE(hwnd.0),
        DEVICE_NOTIFY_WINDOW_HANDLE,
    )
    .context("RegisterSuspendResumeNotification")?;

    Ok((hwnd, handle))
}

fn run_message_pump() {
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_POWERBROADCAST {
        if let Some(kind) = classify_wm_powerbroadcast(wparam.0) {
            let now = SystemTime::now();
            let evt = match kind {
                PowerEventKind::Suspend => PowerEvent::Suspend(now),
                PowerEventKind::Resume => PowerEvent::Resume(now),
            };
            if let Ok(guard) = TX.lock() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.send(evt);
                }
            }
        }
        // Per MSDN : return TRUE to acknowledge the broadcast.
        return LRESULT(1);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// HWND and HPOWERNOTIFY hold raw pointers and are !Send by default ;
// wrap them so the pump thread can hand them back to the guard's Drop
// on the main thread. Safe here because :
//   - The HWND is used only for DestroyWindow (a syscall, thread-safe).
//   - The HPOWERNOTIFY is used only for UnregisterSuspendResumeNotification.
struct HwndSend(HWND);
unsafe impl Send for HwndSend {}
struct HpowerNotifySend(HPOWERNOTIFY);
unsafe impl Send for HpowerNotifySend {}
