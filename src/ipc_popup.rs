// Popup content pipe. Direction is the inverse of `ipc.rs` : the POPUP
// process is the server (one pipe per popup, name embeds its PID) and
// the SYSTEM service is the client. Carries in-place content updates so
// a daily-pause fire can mutate the RUNNING popup's DOM instead of
// kill+respawning it (WebView2 flash). Same framing philosophy as
// `ipc.rs` : leading tag byte + LE payload, message-mode pipe delivers
// one whole frame per connection — no stream framing. Variable-length
// strings are length-prefixed (u32 LE + UTF-8 bytes).
//
// Wire format (tags start at 0x10 so a log line can never be confused
// with the service pipe's 0x01-0x05 range) :
//   0x10 + lp title + lp subtitle + lp dismiss_label
//        + 8 LE bytes (u64 duration_secs) + 1 byte mode   UpdateContent
//   0x11                                                  Dismiss
//   0x12 + 4 LE bytes (u32 palier_minutes)
//        + 8 LE bytes (u64 seconds_until)                 RestartCountdown
//
// `lp` = u32 LE byte length + that many UTF-8 bytes.
// mode byte : 0 = interval, 1 = daily-pause.
//
// Pipe name blends into the same native-service camouflage convention
// as `SystemHealthAgent-Signal` (a "purrpause" pipe would be greppable
// in Process Explorer and break the anti-tamper disguise).

/// Prefix of the per-popup pipe ; the popup's PID is appended.
pub const POPUP_PIPE_PREFIX: &str = r"\\.\pipe\SystemHealthAgent-Render-";

/// Full pipe path for the popup running as `pid`.
pub fn popup_pipe_name(pid: u32) -> String {
    format!("{POPUP_PIPE_PREFIX}{pid}")
}

/// Upper bound on one framed message. Titles / subtitles are config-UI
/// strings ; 16 KiB is generous headroom. Also used as the pipe's
/// in-buffer size so message-mode delivery is never split
/// (ERROR_MORE_DATA).
pub const POPUP_MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// Which content family an `UpdateContent` carries. The popup mirrors
/// it into `body.dataset.popupKind` so the page can style per-kind if
/// ever needed ; Rust-side it is informational (logged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupContentMode {
    Interval,
    DailyPause,
}

impl PopupContentMode {
    pub fn as_u8(self) -> u8 {
        match self {
            PopupContentMode::Interval => 0,
            PopupContentMode::DailyPause => 1,
        }
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(PopupContentMode::Interval),
            1 => Some(PopupContentMode::DailyPause),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupMessage {
    /// Swap the live popup's title / subtitle / dismiss label and
    /// restart its countdown with a fresh duration. Sent by the service
    /// when the daily pause fires while an interval popup is on screen.
    UpdateContent {
        title: String,
        subtitle: String,
        dismiss_label: String,
        duration_secs: u64,
        mode: PopupContentMode,
    },
    /// Close the popup through its clean-exit path (exit code 0, the
    /// service treats it as a normal dismiss). No production sender yet ;
    /// exercised via the `--popup-ipc` debug CLI.
    Dismiss,
    /// Restart only the countdown display / unlock timer. `palier_minutes`
    /// is informational (logged popup-side). No production sender yet.
    RestartCountdown { palier_minutes: u32, seconds_until: u64 },
}

/// Read one u32-length-prefixed UTF-8 string at `*cursor`, advancing it.
fn read_lp_string(bytes: &[u8], cursor: &mut usize) -> Option<String> {
    let len_end = cursor.checked_add(4)?;
    let len_bytes: [u8; 4] = bytes.get(*cursor..len_end)?.try_into().ok()?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let str_end = len_end.checked_add(len)?;
    let s = String::from_utf8(bytes.get(len_end..str_end)?.to_vec()).ok()?;
    *cursor = str_end;
    Some(s)
}

fn write_lp_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = cursor.checked_add(4)?;
    let b: [u8; 4] = bytes.get(*cursor..end)?.try_into().ok()?;
    *cursor = end;
    Some(u32::from_le_bytes(b))
}

fn read_u64(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let end = cursor.checked_add(8)?;
    let b: [u8; 8] = bytes.get(*cursor..end)?.try_into().ok()?;
    *cursor = end;
    Some(u64::from_le_bytes(b))
}

impl PopupMessage {
    /// Parse one framed message. Strict : truncation, oversize declared
    /// string length, invalid UTF-8, unknown mode / tag, or trailing
    /// bytes all yield `None` — the server drops the connection (same
    /// "no smuggled intent" posture as `ipc::Message::parse`).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (&tag, rest) = bytes.split_first()?;
        let mut cursor = 0usize;
        let msg = match tag {
            0x10 => {
                let title = read_lp_string(rest, &mut cursor)?;
                let subtitle = read_lp_string(rest, &mut cursor)?;
                let dismiss_label = read_lp_string(rest, &mut cursor)?;
                let duration_secs = read_u64(rest, &mut cursor)?;
                let mode = PopupContentMode::from_u8(*rest.get(cursor)?)?;
                cursor += 1;
                PopupMessage::UpdateContent {
                    title,
                    subtitle,
                    dismiss_label,
                    duration_secs,
                    mode,
                }
            }
            0x11 => PopupMessage::Dismiss,
            0x12 => {
                let palier_minutes = read_u32(rest, &mut cursor)?;
                let seconds_until = read_u64(rest, &mut cursor)?;
                PopupMessage::RestartCountdown {
                    palier_minutes,
                    seconds_until,
                }
            }
            _ => return None,
        };
        // Reject trailing bytes after a well-formed body.
        if cursor != rest.len() {
            return None;
        }
        Some(msg)
    }

    /// Encode a message to its wire bytes. Debug-asserts the frame fits
    /// the pipe in-buffer ; the client also rejects oversize frames
    /// before writing (release builds).
    pub fn to_bytes(&self) -> Vec<u8> {
        let v = self.encode();
        debug_assert!(
            v.len() <= POPUP_MAX_MESSAGE_BYTES,
            "popup ipc frame exceeds pipe buffer"
        );
        v
    }

    fn encode(&self) -> Vec<u8> {
        match self {
            PopupMessage::UpdateContent {
                title,
                subtitle,
                dismiss_label,
                duration_secs,
                mode,
            } => {
                let mut v = Vec::with_capacity(
                    1 + 12 + title.len() + subtitle.len() + dismiss_label.len() + 9,
                );
                v.push(0x10);
                write_lp_string(&mut v, title);
                write_lp_string(&mut v, subtitle);
                write_lp_string(&mut v, dismiss_label);
                v.extend_from_slice(&duration_secs.to_le_bytes());
                v.push(mode.as_u8());
                v
            }
            PopupMessage::Dismiss => vec![0x11],
            PopupMessage::RestartCountdown {
                palier_minutes,
                seconds_until,
            } => {
                let mut v = Vec::with_capacity(13);
                v.push(0x12);
                v.extend_from_slice(&palier_minutes.to_le_bytes());
                v.extend_from_slice(&seconds_until.to_le_bytes());
                v
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use super::*;

    use anyhow::{anyhow, Context, Result};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_WRITE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE,
        OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_MESSAGE,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    use crate::ipc::{accept_frame, create_cancel_event, write_frame, AcceptFrame, SecAttrs};

    /// SYSTEM (the service) gets full access ; Interactive Users get
    /// write + read-attributes so the same-desktop `--popup-ipc` debug
    /// CLI can connect. Deliberately NO BUILTIN\Users grant — unlike
    /// the service signal pipe, nothing user-wide should be able to
    /// mutate popup content.
    const POPUP_PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GWFR;;;IU)";

    /// Blocking accept loop for THIS process's popup pipe. One
    /// connection = one message, mirroring `ipc::run_server`. Never
    /// returns in practice : the cancel event exists to satisfy
    /// `accept_frame` but is never signaled — the popup's tao
    /// `event_loop.run` diverges, so process exit is the only teardown
    /// and it takes this thread with it.
    pub fn run_popup_server(mut on_message: impl FnMut(PopupMessage) + Send + 'static) -> Result<()> {
        let sa_holder = SecAttrs::from_sddl(POPUP_PIPE_SDDL)?;
        let name = popup_pipe_name(std::process::id());
        let name_wide: HSTRING = name.as_str().into();
        let cancel = create_cancel_event().context("create cancel event (popup pipe)")?;

        loop {
            let pipe = unsafe {
                CreateNamedPipeW(
                    PCWSTR::from_raw(name_wide.as_ptr()),
                    PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    0, // out buffer size ignored for inbound-only
                    POPUP_MAX_MESSAGE_BYTES as u32,
                    0, // default timeout
                    Some(sa_holder.as_ptr()),
                )
            };

            if pipe.is_invalid() {
                let err = unsafe { GetLastError() };
                tracing::warn!(?err, "CreateNamedPipeW(popup) failed ; retrying in 1s");
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            let mut buf = vec![0u8; POPUP_MAX_MESSAGE_BYTES];
            match accept_frame(pipe, cancel, &mut buf) {
                Ok(AcceptFrame::Cancelled) => {
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                    return Ok(());
                }
                Ok(AcceptFrame::Frame(n)) => {
                    match PopupMessage::parse(&buf[..n]) {
                        Some(msg) => {
                            tracing::debug!(?msg, "popup ipc message received");
                            on_message(msg);
                        }
                        None => {
                            // Client connected but sent garbage. Drop the
                            // connection and keep listening.
                            tracing::debug!(bytes = n, "popup ipc: unparseable frame dropped");
                        }
                    }
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "popup pipe accept failed ; continuing");
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                }
            }
        }
    }

    /// Fast-fail client used by the service tick loop (and the debug
    /// CLI). Deliberately snappier than `ipc::send` : the tick loop
    /// must fall back to kill+respawn promptly, so
    /// `ERROR_FILE_NOT_FOUND` is terminal (no pipe means the popup has
    /// no server : pre-session-4 binary or already gone) and
    /// `ERROR_PIPE_BUSY` gets a single `WaitNamedPipeW(250ms)` retry.
    /// Worst case ~500 ms against the 5 s tick cadence.
    pub fn send_to_popup(pid: u32, msg: &PopupMessage) -> Result<()> {
        let bytes = msg.to_bytes();
        if bytes.len() > POPUP_MAX_MESSAGE_BYTES {
            anyhow::bail!(
                "popup ipc message too large: {} bytes (max {POPUP_MAX_MESSAGE_BYTES})",
                bytes.len()
            );
        }

        let name = popup_pipe_name(pid);
        let name_wide: HSTRING = name.as_str().into();
        const ATTEMPTS: u32 = 2;
        const BUSY_WAIT_MS: u32 = 250;

        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..ATTEMPTS {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR::from_raw(name_wide.as_ptr()),
                    GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            };

            match handle {
                Ok(h) if !h.is_invalid() => {
                    let write_result = write_frame(h, &bytes);
                    unsafe { let _ = CloseHandle(h); }
                    return write_result;
                }
                _ => {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_PIPE_BUSY {
                        // Instance exists but is mid-accept ; short bounded
                        // wait, then one retry.
                        let waited = unsafe {
                            WaitNamedPipeW(PCWSTR::from_raw(name_wide.as_ptr()), BUSY_WAIT_MS)
                        };
                        if !waited.as_bool() {
                            std::thread::sleep(std::time::Duration::from_millis(
                                BUSY_WAIT_MS as u64,
                            ));
                        }
                        last_err = Some(anyhow!("popup pipe busy (retrying)"));
                        continue;
                    }
                    // ERROR_FILE_NOT_FOUND and anything else : terminal.
                    // The caller's fallback (kill+respawn) must fire now,
                    // not after a retry window.
                    if err == ERROR_FILE_NOT_FOUND {
                        return Err(anyhow!("popup pipe absent: {name}"));
                    }
                    return Err(anyhow!("CreateFileW({name}) failed: {err:?}"));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("send_to_popup: exhausted retries")))
    }
}

#[cfg(windows)]
pub use win::{run_popup_server, send_to_popup};

#[cfg(not(windows))]
pub fn send_to_popup(_pid: u32, _msg: &PopupMessage) -> anyhow::Result<()> {
    anyhow::bail!("ipc_popup::send_to_popup is Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_update() -> PopupMessage {
        PopupMessage::UpdateContent {
            title: "Pause déjeuner".to_string(),
            subtitle: "On mange à midi ☕".to_string(),
            dismiss_label: "J'ai fini".to_string(),
            duration_secs: 1800,
            mode: PopupContentMode::DailyPause,
        }
    }

    #[test]
    fn update_content_roundtrip_basic() {
        let msg = sample_update();
        assert_eq!(PopupMessage::parse(&msg.to_bytes()), Some(msg));
    }

    #[test]
    fn update_content_roundtrip_empty_strings() {
        let msg = PopupMessage::UpdateContent {
            title: String::new(),
            subtitle: String::new(),
            dismiss_label: String::new(),
            duration_secs: 0,
            mode: PopupContentMode::Interval,
        };
        let bytes = msg.to_bytes();
        // 1 tag + 3 * 4 length prefixes + 8 duration + 1 mode.
        assert_eq!(bytes.len(), 22);
        assert_eq!(PopupMessage::parse(&bytes), Some(msg));
    }

    #[test]
    fn update_content_roundtrip_utf8_accents() {
        let msg = PopupMessage::UpdateContent {
            title: "Pause déjà là — çà ira 🐱".to_string(),
            subtitle: "élève, à bientôt".to_string(),
            dismiss_label: "Terminé".to_string(),
            duration_secs: u64::MAX,
            mode: PopupContentMode::DailyPause,
        };
        assert_eq!(PopupMessage::parse(&msg.to_bytes()), Some(msg));
    }

    #[test]
    fn update_content_wire_shape_exact_bytes() {
        let msg = PopupMessage::UpdateContent {
            title: "ab".to_string(),
            subtitle: "c".to_string(),
            dismiss_label: String::new(),
            duration_secs: 2,
            mode: PopupContentMode::DailyPause,
        };
        let expected = [
            0x10, // tag
            0x02, 0x00, 0x00, 0x00, b'a', b'b', // title
            0x01, 0x00, 0x00, 0x00, b'c', // subtitle
            0x00, 0x00, 0x00, 0x00, // dismiss_label (empty)
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // duration_secs = 2 LE
            0x01, // mode = daily-pause
        ];
        assert_eq!(msg.to_bytes(), expected);
    }

    #[test]
    fn update_content_truncated_rejected_at_every_prefix() {
        // Every strict prefix is missing at least the trailing mode
        // byte, so all of them must be rejected.
        let bytes = sample_update().to_bytes();
        for n in 0..bytes.len() {
            assert_eq!(PopupMessage::parse(&bytes[..n]), None, "prefix len {n}");
        }
    }

    #[test]
    fn update_content_trailing_bytes_rejected() {
        let mut bytes = sample_update().to_bytes();
        bytes.push(0x00);
        assert_eq!(PopupMessage::parse(&bytes), None);
    }

    #[test]
    fn update_content_len_exceeding_buffer_rejected() {
        // Declared title length (u32::MAX) far exceeds the remaining
        // bytes ; the checked cursor math must reject, not panic.
        let mut bytes = vec![0x10];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"ab");
        assert_eq!(PopupMessage::parse(&bytes), None);
    }

    #[test]
    fn update_content_invalid_utf8_rejected() {
        let mut bytes = vec![0x10];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8 title
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.push(0x01);
        assert_eq!(PopupMessage::parse(&bytes), None);
    }

    #[test]
    fn update_content_unknown_mode_byte_rejected() {
        let mut bytes = sample_update().to_bytes();
        let last = bytes.len() - 1;
        bytes[last] = 0x02; // only 0 / 1 are valid modes
        assert_eq!(PopupMessage::parse(&bytes), None);
    }

    #[test]
    fn dismiss_roundtrip_is_single_byte_0x11() {
        let bytes = PopupMessage::Dismiss.to_bytes();
        assert_eq!(bytes, vec![0x11]);
        assert_eq!(PopupMessage::parse(&bytes), Some(PopupMessage::Dismiss));
    }

    #[test]
    fn restart_countdown_roundtrip() {
        let msg = PopupMessage::RestartCountdown {
            palier_minutes: 15,
            seconds_until: 900,
        };
        let bytes = msg.to_bytes();
        assert_eq!(bytes.len(), 13);
        assert_eq!(bytes[0], 0x12);
        assert_eq!(PopupMessage::parse(&bytes), Some(msg));
    }

    #[test]
    fn restart_countdown_truncated_rejected() {
        let bytes = PopupMessage::RestartCountdown {
            palier_minutes: 5,
            seconds_until: 300,
        }
        .to_bytes();
        for n in 0..bytes.len() {
            assert_eq!(PopupMessage::parse(&bytes[..n]), None, "prefix len {n}");
        }
    }

    #[test]
    fn restart_countdown_trailing_rejected() {
        let mut bytes = PopupMessage::RestartCountdown {
            palier_minutes: 5,
            seconds_until: 300,
        }
        .to_bytes();
        bytes.push(0xAA);
        assert_eq!(PopupMessage::parse(&bytes), None);
    }

    #[test]
    fn parse_empty_and_unknown_tag_rejected() {
        assert_eq!(PopupMessage::parse(&[]), None);
        assert_eq!(PopupMessage::parse(&[0x01]), None); // service-pipe tag
        assert_eq!(PopupMessage::parse(&[0x99]), None);
    }

    #[test]
    fn popup_pipe_name_embeds_pid() {
        assert_eq!(
            popup_pipe_name(4242),
            r"\\.\pipe\SystemHealthAgent-Render-4242"
        );
    }
}
