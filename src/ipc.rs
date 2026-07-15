// Named-pipe IPC server. One-way (inbound-only) : the config UI writes
// single-byte commands, the service reads and dispatches them. Message
// framing is trivial (one byte = one command) so we skip serde/JSON.
//
// Pipe name blends into native Windows-service pipe conventions
// (`SystemEventsBroker-*`, `TrustedInstaller-Rpc`, ...). Explicit SDDL
// grants BUILTIN\Users the minimum access needed to CreateFile the pipe
// as a write client (GW+FR), so the future user-mode config UI can
// signal us without needing SYSTEM.

pub const PIPE_NAME: &str = r"\\.\pipe\SystemHealthAgent-Signal";

#[cfg(windows)]
const PIPE_SDDL: &str = "D:(A;;GWFR;;;BU)(A;;FA;;;SY)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// Force-spawn a popup immediately + reset the cycle clock.
    TriggerPopupNow,
    /// Re-read state.dat from disk into the in-memory config.
    Reload,
    /// Graceful shutdown of the service (rare — SCM stop is the normal path).
    Shutdown,
}

impl Message {
    /// Parse one framed message. Only single-byte messages are accepted ;
    /// anything else returns `None` and the caller closes the connection.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 1 {
            return None;
        }
        match bytes[0] {
            0x01 => Some(Message::TriggerPopupNow),
            0x02 => Some(Message::Reload),
            0x03 => Some(Message::Shutdown),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn to_byte(self) -> u8 {
        // Consumed by tests here and by the future config-UI client that
        // writes to the pipe. Not called in the production service path.
        match self {
            Message::TriggerPopupNow => 0x01,
            Message::Reload => 0x02,
            Message::Shutdown => 0x03,
        }
    }
}

#[cfg(windows)]
mod win {
    use super::*;

    use std::mem::size_of;
    use std::sync::mpsc;

    use anyhow::{anyhow, Context, Result};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        GENERIC_WRITE, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
        FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        NMPWAIT_USE_DEFAULT_WAIT, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects, INFINITE};
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// Wrapper for a HANDLE that we've decided is safe to send across
    /// threads. Win32 event handles are process-scoped and can be
    /// signaled from any thread ; the wrapper is just needed to satisfy
    /// Rust's `Send` bound on the closure captured by `thread::spawn`.
    #[derive(Copy, Clone)]
    pub struct SendableHandle(pub HANDLE);
    unsafe impl Send for SendableHandle {}
    unsafe impl Sync for SendableHandle {}

    /// Create a manual-reset auto-cleared event that the pipe server
    /// blocks on. Signal it via `SetEvent` from the service main thread
    /// to unblock a pending `ConnectNamedPipe` / `ReadFile`.
    pub fn create_cancel_event() -> Result<HANDLE> {
        let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .context("CreateEventW(cancel)")?;
        Ok(h)
    }

    /// Blocking accept-loop. Returns when `cancel_event` fires or a
    /// `Shutdown` message is received. One connection = one message
    /// (message-mode pipe, 1-byte payload).
    pub fn run_server(cancel_event: SendableHandle, tx: mpsc::Sender<Message>) -> Result<()> {
        let sa_holder = SecAttrs::from_sddl(PIPE_SDDL)?;
        let name_wide: HSTRING = PIPE_NAME.into();

        loop {
            let pipe = unsafe {
                CreateNamedPipeW(
                    PCWSTR::from_raw(name_wide.as_ptr()),
                    PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    0,   // out buffer size ignored for inbound-only
                    16,  // in buffer size (we only read 1 byte, keep small)
                    0,   // default timeout
                    Some(sa_holder.as_ptr()),
                )
            };

            if pipe.is_invalid() {
                let err = unsafe { GetLastError() };
                tracing::warn!(?err, "CreateNamedPipeW failed ; retrying in 1s");
                std::thread::sleep(std::time::Duration::from_secs(1));
                if is_cancelled(cancel_event.0) {
                    return Ok(());
                }
                continue;
            }

            match accept_once(pipe, cancel_event.0) {
                Ok(AcceptOutcome::Cancelled) => {
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                    return Ok(());
                }
                Ok(AcceptOutcome::Message(msg)) => {
                    tracing::debug!(?msg, "ipc message received");
                    let _ = tx.send(msg);
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                    if msg == Message::Shutdown {
                        return Ok(());
                    }
                }
                Ok(AcceptOutcome::Ignored) => {
                    // Client connected but sent garbage. Drop the
                    // connection and keep listening.
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "pipe accept failed ; continuing");
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                        let _ = CloseHandle(pipe);
                    }
                }
            }
        }
    }

    enum AcceptOutcome {
        Cancelled,
        Message(Message),
        Ignored,
    }

    fn is_cancelled(cancel: HANDLE) -> bool {
        // Zero-timeout wait to peek at the event without blocking.
        let handles = [cancel];
        let r = unsafe { WaitForMultipleObjects(&handles, false, 0) };
        r == WAIT_OBJECT_0
    }

    fn accept_once(pipe: HANDLE, cancel: HANDLE) -> Result<AcceptOutcome> {
        let connect_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .context("CreateEventW(connect)")?;

        let mut overlap = OVERLAPPED::default();
        overlap.hEvent = connect_event;

        // ConnectNamedPipe with overlapped I/O :
        //   returns FALSE ; GetLastError = ERROR_IO_PENDING when waiting
        //   returns FALSE ; GetLastError = ERROR_PIPE_CONNECTED when a client
        //     connected between CreateNamedPipeW and ConnectNamedPipe
        //   returns TRUE (unusual — treat like PIPE_CONNECTED)
        let ret = unsafe { ConnectNamedPipe(pipe, Some(&mut overlap)) };
        let already_connected = match ret {
            Ok(()) => true,
            Err(_) => {
                let err = unsafe { GetLastError() };
                if err == ERROR_PIPE_CONNECTED {
                    true
                } else if err == ERROR_IO_PENDING {
                    false
                } else {
                    unsafe { let _ = CloseHandle(connect_event); }
                    return Err(anyhow!("ConnectNamedPipe: {err:?}"));
                }
            }
        };

        if !already_connected {
            let handles = [overlap.hEvent, cancel];
            let waited = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            if waited == WAIT_OBJECT_0 {
                // Connection completed — proceed to read.
            } else if waited.0 == WAIT_OBJECT_0.0 + 1 {
                // Cancel signaled — abort the pending connect.
                unsafe {
                    let _ = CancelIoEx(pipe, Some(&overlap));
                    let _ = CloseHandle(connect_event);
                }
                return Ok(AcceptOutcome::Cancelled);
            } else {
                unsafe { let _ = CloseHandle(connect_event); }
                return Err(anyhow!("WaitForMultipleObjects(connect): {waited:?}"));
            }
        }

        // Read one message byte, again with overlapped semantics so we
        // can honour the cancel event mid-read.
        let mut buf = [0u8; 1];
        let mut read_overlap = OVERLAPPED::default();
        read_overlap.hEvent = connect_event; // reuse : manual-reset, cleared here
        unsafe { let _ = ResetEvent(connect_event); }

        let read_ret = unsafe {
            ReadFile(
                pipe,
                Some(&mut buf),
                None,
                Some(&mut read_overlap),
            )
        };

        let sync_done = match read_ret {
            Ok(()) => true,
            Err(_) => {
                let err = unsafe { GetLastError() };
                if err == ERROR_IO_PENDING {
                    false
                } else {
                    unsafe { let _ = CloseHandle(connect_event); }
                    return Err(anyhow!("ReadFile: {err:?}"));
                }
            }
        };

        if !sync_done {
            let handles = [read_overlap.hEvent, cancel];
            let waited = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            if waited == WAIT_OBJECT_0 {
                // Read completed.
            } else if waited.0 == WAIT_OBJECT_0.0 + 1 {
                unsafe {
                    let _ = CancelIoEx(pipe, Some(&read_overlap));
                    let _ = CloseHandle(connect_event);
                }
                return Ok(AcceptOutcome::Cancelled);
            } else {
                unsafe { let _ = CloseHandle(connect_event); }
                return Err(anyhow!("WaitForMultipleObjects(read): {waited:?}"));
            }
        }

        // Retrieve the actual byte count.
        let mut bytes: u32 = 0;
        let overlapped_result = unsafe {
            GetOverlappedResult(pipe, &read_overlap, &mut bytes, false)
        };
        unsafe { let _ = CloseHandle(connect_event); }
        overlapped_result.context("GetOverlappedResult(read)")?;

        Ok(match Message::parse(&buf[..bytes as usize]) {
            Some(msg) => AcceptOutcome::Message(msg),
            None => AcceptOutcome::Ignored,
        })
    }

    /// Owns a `PSECURITY_DESCRIPTOR` and the `SECURITY_ATTRIBUTES` struct
    /// that references it. `LocalFree`s on drop.
    struct SecAttrs {
        sd: PSECURITY_DESCRIPTOR,
        sa: SECURITY_ATTRIBUTES,
    }

    impl SecAttrs {
        fn from_sddl(sddl: &str) -> Result<Self> {
            let sddl_wide: HSTRING = sddl.into();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    &sddl_wide,
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )
            }
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW(pipe)")?;

            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            Ok(Self { sd, sa })
        }

        fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
            &self.sa as *const _
        }
    }

    impl Drop for SecAttrs {
        fn drop(&mut self) {
            if !self.sd.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.sd.0 as _)));
                }
            }
        }
    }

    /// User-mode client that writes a single-byte `Message` to the
    /// service's IPC pipe. Called from the config UI's Général tab
    /// (« Déclencher une pause maintenant ») and after any save
    /// (`Message::Reload`). The pipe SDDL grants `GW+FR` to
    /// `BUILTIN\Users` so no elevation is required.
    ///
    /// Retries a few times on `ERROR_PIPE_BUSY` — the pipe is
    /// message-mode with `PIPE_UNLIMITED_INSTANCES` server-side so
    /// contention is rare, but a service restart can transiently
    /// close the pipe.
    pub fn send(msg: Message) -> Result<()> {
        let name_wide: HSTRING = PIPE_NAME.into();
        const ATTEMPTS: u32 = 3;
        const WAIT_MS: u32 = 500;

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
                    let write_result = write_one_byte(h, msg.to_byte());
                    unsafe { let _ = CloseHandle(h); }
                    return write_result;
                }
                _ => {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_PIPE_BUSY {
                        // Server is busy accepting another client ; wait
                        // for its default timeout then retry.
                        let waited = unsafe {
                            WaitNamedPipeW(
                                PCWSTR::from_raw(name_wide.as_ptr()),
                                NMPWAIT_USE_DEFAULT_WAIT,
                            )
                        };
                        // WaitNamedPipeW returns BOOL (nonzero = OK).
                        // On failure fall back to a plain sleep.
                        if !waited.as_bool() {
                            std::thread::sleep(std::time::Duration::from_millis(WAIT_MS as u64));
                        }
                        last_err = Some(anyhow!("pipe busy (retrying)"));
                        continue;
                    }
                    return Err(anyhow!("CreateFileW({PIPE_NAME}) failed: {err:?}"));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("send: exhausted retries")))
    }

    fn write_one_byte(handle: HANDLE, byte: u8) -> Result<()> {
        let buf = [byte];
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(handle, Some(&buf), Some(&mut written), None)
        };
        ok.context("WriteFile(pipe)")?;
        if written != 1 {
            anyhow::bail!("short write: {written} bytes");
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{create_cancel_event, run_server, send, SendableHandle};

#[cfg(not(windows))]
pub fn send(_msg: Message) -> anyhow::Result<()> {
    anyhow::bail!("ipc::send is Windows-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trigger_byte() {
        assert_eq!(Message::parse(&[0x01]), Some(Message::TriggerPopupNow));
    }

    #[test]
    fn parse_reload_byte() {
        assert_eq!(Message::parse(&[0x02]), Some(Message::Reload));
    }

    #[test]
    fn parse_shutdown_byte() {
        assert_eq!(Message::parse(&[0x03]), Some(Message::Shutdown));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert_eq!(Message::parse(&[]), None);
    }

    #[test]
    fn parse_unknown_byte_returns_none() {
        assert_eq!(Message::parse(&[0x99]), None);
    }

    #[test]
    fn parse_extra_bytes_returns_none() {
        // Single-message framing: multi-byte payload is rejected so the
        // client can't smuggle intent past the parser.
        assert_eq!(Message::parse(&[0x01, 0x02]), None);
    }

    #[test]
    fn to_byte_roundtrip() {
        for m in [Message::TriggerPopupNow, Message::Reload, Message::Shutdown] {
            assert_eq!(Message::parse(&[m.to_byte()]), Some(m));
        }
    }

    // Wire-shape tests for the pipe client. The actual CreateFileW /
    // WriteFile roundtrip requires a running server so it's part of
    // the Windows host smoke — here we assert that the payload
    // sent over the wire is exactly one byte per Message variant.
    #[test]
    fn client_wire_shape_trigger_is_0x01() {
        assert_eq!(Message::TriggerPopupNow.to_byte(), 0x01);
    }

    #[test]
    fn client_wire_shape_reload_is_0x02() {
        assert_eq!(Message::Reload.to_byte(), 0x02);
    }

    #[test]
    fn client_wire_shape_shutdown_is_0x03() {
        assert_eq!(Message::Shutdown.to_byte(), 0x03);
    }

    #[test]
    fn pipe_name_matches_server_contract() {
        // The config UI's send() opens exactly the same pipe path the
        // service exposes via run_server.
        assert_eq!(PIPE_NAME, r"\\.\pipe\SystemHealthAgent-Signal");
    }
}
