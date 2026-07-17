// WASAPI-based "user is consuming media" detector : any active
// audio render session counts as screen time, muted or not
// (design intent : muting a background video is still watching).
//
// COM init is scoped to the poll : the service tick runs on a
// single thread, one CoInitializeEx / CoUninitialize per call
// keeps COM apartment state out of the tick thread's steady state.
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

/// True if any WASAPI render endpoint currently has an active
/// session — i.e. an app is asking the OS to play audio (video
/// with sound, music, notifications). Muted state is intentionally
/// NOT checked : the goal is "screen time" and a muted YouTube tab
/// still means the user is watching.
pub fn playing_video_or_audio() -> bool {
    unsafe {
        // MULTITHREADED apartment : matches the tick thread which is
        // not a UI / STA context. Second CoInitializeEx on the same
        // thread with the same flags returns S_FALSE (harmless) ;
        // still call CoUninitialize to balance the ref count.
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let busy = inner().unwrap_or(false);
        if init.is_ok() {
            CoUninitialize();
        }
        busy
    }
}

unsafe fn inner() -> windows::core::Result<bool> {
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
