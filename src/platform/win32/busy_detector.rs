// Busy-context detector : `SHQueryUserNotificationState` covers
// fullscreen game / presentation / QUNS_BUSY ; WASAPI covers "audio is
// playing" (YouTube on the second monitor, Spotify in the background,
// a game with music but no window focus, ...).
//
// COM init is scoped to the audio poll : the service tick runs on a
// single thread, one CoInitializeEx / CoUninitialize per call keeps
// COM apartment state out of the tick thread's steady state. If
// session-5 smoke reveals the overhead matters, promote to a
// thread-local OnceCell.

use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Shell::SHQueryUserNotificationState;

use crate::signals::quns_state_is_busy;

/// True if the user is in a state where a popup would be disruptive :
/// a busy QUNS state (full-screen game / presentation / D3D), OR any
/// active WASAPI render session.
pub fn is_busy() -> bool {
    let (_, quns, audio) = busy_snapshot();
    quns || audio
}

/// Returns `(quns_state_raw, quns_busy, audio_busy)`. `quns_state_raw`
/// is the exact `SHQueryUserNotificationState` return value (1..=7 per
/// the QUERY_USER_NOTIFICATION_STATE enum, or 0 on API failure) so logs
/// can distinguish `QUNS_BUSY` (2, Focus Assist / DND) from
/// `QUNS_D3D_FULL_SCREEN` (3) from `QUNS_PRESENTATION_MODE` (4). All
/// three collapse to `quns_busy = true` via
/// [`crate::signals::quns_state_is_busy`], the boolean the scheduler
/// consumes. `audio_busy` is any active WASAPI render session. One
/// call — same COM init cost as `is_busy()`.
pub fn busy_snapshot() -> (u32, bool, bool) {
    let quns_raw = quns_state_raw();
    (quns_raw, quns_state_is_busy(quns_raw), audio_busy())
}

fn quns_state_raw() -> u32 {
    // Failure treated as 0 (out-of-range → quns_state_is_busy returns
    // false) so a Shell API glitch defaults to "not busy" and errs
    // toward firing the popup rather than silently deferring forever.
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => state.0 as u32,
        Err(_) => 0,
    }
}

fn audio_busy() -> bool {
    unsafe {
        // MULTITHREADED apartment : matches the tick thread which is
        // not a UI / STA context. Second CoInitializeEx on the same
        // thread with the same flags returns S_FALSE (harmless) ;
        // still call CoUninitialize to balance the ref count.
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let busy = audio_busy_inner().unwrap_or(false);
        if init.is_ok() {
            CoUninitialize();
        }
        busy
    }
}

unsafe fn audio_busy_inner() -> windows::core::Result<bool> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)?;
    let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let count = collection.GetCount()?;
    for i in 0..count {
        let device = collection.Item(i)?;
        let mgr: IAudioSessionManager2 =
            device.Activate::<IAudioSessionManager2>(CLSCTX_INPROC_SERVER, None)?;
        let sessions = mgr.GetSessionEnumerator()?;
        let sess_count = sessions.GetCount()?;
        for j in 0..sess_count {
            let session = sessions.GetSession(j)?;
            if session.GetState()? == AudioSessionStateActive {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
