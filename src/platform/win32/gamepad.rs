// XInput poll + diff signal. `LASTINPUTINFO` only tracks keyboard +
// mouse, so a couch-gaming session where the user only touches a pad
// would appear idle without this. Called from the service tick (~30 s
// cadence) — XInput is local and O(1) per slot, cost is negligible.
//
// The diff key is `dwPacketNumber`, which XInput guarantees increments
// if and only if button / stick / trigger state changes. Storing the
// four packet numbers per slot (Option<u32>, None = disconnected) is
// enough to detect any transition without importing `XINPUT_STATE`
// into the pure-helper layer.

use std::sync::Mutex;

use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};

use crate::signals::{snapshot_differs, GamepadSnapshot};

const XINPUT_MAX_SLOTS: u32 = 4;
// XInput uses standard Win32 error codes ; only ERROR_SUCCESS (0) means
// the slot is populated. Anything else (typically
// ERROR_DEVICE_NOT_CONNECTED = 1167) means "slot empty".
const ERROR_SUCCESS: u32 = 0;

static PREV_SNAPSHOT: Mutex<Option<GamepadSnapshot>> = Mutex::new(None);

/// True if any connected XInput slot shows a delta since the previous
/// call. First call primes the snapshot and returns false (no prior
/// state to diff against).
pub fn gamepad_active_since_last_call() -> bool {
    let mut current: GamepadSnapshot = [None; 4];
    for i in 0..XINPUT_MAX_SLOTS {
        let mut state = XINPUT_STATE::default();
        let rc = unsafe { XInputGetState(i, &mut state) };
        if rc == ERROR_SUCCESS {
            current[i as usize] = Some(state.dwPacketNumber);
        }
    }
    let mut guard = PREV_SNAPSHOT.lock().unwrap();
    let differs = match guard.as_ref() {
        Some(prev) => snapshot_differs(prev, &current),
        None => false,
    };
    *guard = Some(current);
    differs
}
