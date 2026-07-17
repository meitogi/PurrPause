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
    quns_busy() || audio_busy()
}

fn quns_busy() -> bool {
    // Failure treated as "not busy" — err on the side of firing the
    // popup rather than silently deferring on a Shell API glitch.
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => quns_state_is_busy(state.0 as u32),
        Err(_) => false,
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
