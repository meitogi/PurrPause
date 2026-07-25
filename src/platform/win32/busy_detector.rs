// WASAPI-based "user is consuming media" detector : any active
// audio render session counts as screen time, muted or not
// (design intent : muting a background video is still watching).
//
// # Session 0 pitfall
//
// WASAPI audio sessions are per-session, like window stations : a
// SYSTEM service in Session 0 enumerating render endpoints only sees
// Session 0's (empty) audio graph, never the interactive user's, so
// a call from the service tick returns false forever. Same trap as
// `GetLastInputInfo` (see `idle.rs`) — the caller must therefore be
// the user-session `--input-probe` child, which samples this at each
// poll and ships the flag to the service inside `Message::InputStamp`.
//
// COM init is scoped to the poll : one CoInitializeEx /
// CoUninitialize per call keeps COM apartment state out of the
// probe's poll thread's steady state.
//
// Historical note : earlier versions also consumed
// SHQueryUserNotificationState (QUNS) to catch fullscreen / DND /
// presentation contexts. Removed in b.7 — QUNS false-positives on
// VMs and Focus Assist auto-rules made it more noise than signal.
// Alt+pincode on the popup is the intended escape hatch for the
// rare "I'm mid-presentation, don't interrupt" case.

use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

/// One WASAPI sweep with diagnostic counts. `endpoints` = active
/// render endpoints seen, `sessions` = audio sessions across them.
/// The counts discriminate "no render device at all" (VM audio
/// unplugged → endpoints 0) from "device fine but no app has an
/// active session" when `playing` stays false against expectations.
pub struct Detection {
    pub playing: bool,
    pub endpoints: u32,
    pub sessions: u32,
}

/// True if any WASAPI render endpoint currently has an active
/// session — i.e. an app is asking the OS to play audio (video
/// with sound, music, notifications). Muted state is intentionally
/// NOT checked : the goal is "screen time" and a muted YouTube tab
/// still means the user is watching. Errors are logged and collapse
/// to a zeroed, not-playing detection.
pub fn detect() -> Detection {
    unsafe {
        // MULTITHREADED apartment : matches the poll thread which is
        // not a UI / STA context. Second CoInitializeEx on the same
        // thread with the same flags returns S_FALSE (harmless) ;
        // still call CoUninitialize to balance the ref count.
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let detection = match inner() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "WASAPI busy check failed - assuming not playing"
                );
                Detection { playing: false, endpoints: 0, sessions: 0 }
            }
        };
        if init.is_ok() {
            CoUninitialize();
        }
        detection
    }
}

unsafe fn inner() -> windows::core::Result<Detection> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)?;
    let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let count = collection.GetCount()?;
    let mut playing = false;
    let mut total_sessions: u32 = 0;
    for i in 0..count {
        let device = collection.Item(i)?;
        let mgr: IAudioSessionManager2 =
            device.Activate::<IAudioSessionManager2>(CLSCTX_INPROC_SERVER, None)?;
        let sessions = mgr.GetSessionEnumerator()?;
        let sess_count = sessions.GetCount()?;
        total_sessions += sess_count.max(0) as u32;
        for j in 0..sess_count {
            let session = sessions.GetSession(j)?;
            if session.GetState()? == AudioSessionStateActive {
                playing = true;
            }
        }
    }
    Ok(Detection { playing, endpoints: count, sessions: total_sessions })
}
