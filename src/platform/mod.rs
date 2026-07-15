pub mod argv;
pub mod registry;

#[cfg(windows)]
pub mod win32;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::Hash;
use std::path::Path;

use anyhow::Result;

/// Owned, terminatable child process. Blanket impl covers `SpawnedChild`
/// on Windows ; fakes cover it in unit tests. `pub(crate)` keeps the trait
/// out of the public API surface — no downstream can build on it.
pub(crate) trait ChildHandle: Send + Sync {
    fn terminate(&self);
    // Only consumed by unit-test assertions ; production callers touch the
    // concrete SpawnedChild's `pid` field directly for tracing.
    #[cfg_attr(not(test), allow(dead_code))]
    fn pid(&self) -> u32;
}

/// Factory for `ChildHandle` instances. `spawn_in_active_user_session`
/// is the sole production impl (via [`crate::platform::win32::spawn_user::RealSpawner`]) ;
/// unit tests inject a fake so the kill-previous state machine can be
/// exercised without touching the Windows API.
pub(crate) trait Spawner {
    type Child: ChildHandle;
    fn spawn(&self, exe: &Path, args: &[&OsStr]) -> Result<Self::Child>;
}

/// Pure kernel : spawn the new child first, kill the predecessor only on
/// success. If spawn fails, the predecessor stays alive — a stale popup
/// beats none at all. Caller owns the returned child (typically stashes it
/// into its own `Option<S::Child>` and updates side-state like runtime.dat).
pub(crate) fn spawn_replacing<S: Spawner>(
    spawner: &S,
    exe: &Path,
    args: &[&OsStr],
    predecessor: &mut Option<S::Child>,
) -> Result<S::Child> {
    let new_child = spawner.spawn(exe, args)?;
    if let Some(prev) = predecessor.take() {
        prev.terminate();
        drop(prev);
    }
    Ok(new_child)
}

/// Same, keyed. Only the same-key predecessor is terminated ; other keys
/// remain untouched (widget palier isolation).
pub(crate) fn spawn_replacing_keyed<S: Spawner, K: Hash + Eq + Copy>(
    spawner: &S,
    exe: &Path,
    args: &[&OsStr],
    key: K,
    map: &mut HashMap<K, S::Child>,
) -> Result<()> {
    let new_child = spawner.spawn(exe, args)?;
    if let Some(prev) = map.remove(&key) {
        prev.terminate();
        drop(prev);
    }
    map.insert(key, new_child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// Fake child : records terminate() calls into a shared log so tests can
    /// assert the kill order without touching Windows.
    struct FakeChild {
        pid: u32,
        terminated_log: Arc<Mutex<Vec<u32>>>,
    }

    impl ChildHandle for FakeChild {
        fn terminate(&self) {
            self.terminated_log.lock().unwrap().push(self.pid);
        }
        fn pid(&self) -> u32 {
            self.pid
        }
    }

    /// Fake spawner : monotonic PID counter. `fail_next` flips the next
    /// spawn to fail exactly once.
    struct FakeSpawner {
        next_pid: AtomicU32,
        fail_next: AtomicBool,
        terminated_log: Arc<Mutex<Vec<u32>>>,
    }

    impl FakeSpawner {
        fn new() -> Self {
            Self {
                next_pid: AtomicU32::new(1000),
                fail_next: AtomicBool::new(false),
                terminated_log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn fail_next_spawn(&self) {
            self.fail_next.store(true, Ordering::SeqCst);
        }

        fn terminated(&self) -> Vec<u32> {
            self.terminated_log.lock().unwrap().clone()
        }
    }

    impl Spawner for FakeSpawner {
        type Child = FakeChild;
        fn spawn(&self, _exe: &Path, _args: &[&OsStr]) -> Result<Self::Child> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                anyhow::bail!("fake spawn failure");
            }
            Ok(FakeChild {
                pid: self.next_pid.fetch_add(1, Ordering::SeqCst),
                terminated_log: Arc::clone(&self.terminated_log),
            })
        }
    }

    fn dummy_exe() -> &'static Path {
        Path::new("C:\\fake\\SystemHealthAgent.exe")
    }

    // -------- spawn_replacing (popup state machine) --------

    #[test]
    fn first_spawn_stores_child_without_terminating_anything() {
        let spawner = FakeSpawner::new();
        let mut prev: Option<FakeChild> = None;
        let child = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev).unwrap();
        assert_eq!(child.pid(), 1000);
        assert!(spawner.terminated().is_empty());
        prev = Some(child);
        assert_eq!(prev.as_ref().unwrap().pid(), 1000);
    }

    #[test]
    fn second_spawn_terminates_predecessor_exactly_once() {
        let spawner = FakeSpawner::new();
        let mut prev: Option<FakeChild> = None;
        let c1 = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev).unwrap();
        prev = Some(c1);
        let c2 = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev).unwrap();
        assert_eq!(c2.pid(), 1001);
        assert_eq!(spawner.terminated(), vec![1000]);
    }

    #[test]
    fn spawn_failure_preserves_predecessor_unchanged() {
        let spawner = FakeSpawner::new();
        let mut prev: Option<FakeChild> = None;
        let c1 = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev).unwrap();
        let c1_pid = c1.pid();
        prev = Some(c1);

        spawner.fail_next_spawn();
        let result = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev);
        assert!(result.is_err());
        // Predecessor MUST still be there, MUST NOT have been terminated.
        assert!(prev.is_some());
        assert_eq!(prev.as_ref().unwrap().pid(), c1_pid);
        assert!(spawner.terminated().is_empty());
    }

    #[test]
    fn consecutive_spawn_failures_leave_predecessor_intact() {
        let spawner = FakeSpawner::new();
        let mut prev: Option<FakeChild> = None;
        prev = Some(spawn_replacing(&spawner, dummy_exe(), &[], &mut prev).unwrap());
        let original_pid = prev.as_ref().unwrap().pid();

        for _ in 0..3 {
            spawner.fail_next_spawn();
            let _ = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev);
        }
        assert!(prev.is_some());
        assert_eq!(prev.as_ref().unwrap().pid(), original_pid);
        assert!(spawner.terminated().is_empty());
    }

    #[test]
    fn terminate_order_matches_replacement_sequence() {
        let spawner = FakeSpawner::new();
        let mut prev: Option<FakeChild> = None;
        for _ in 0..4 {
            let c = spawn_replacing(&spawner, dummy_exe(), &[], &mut prev).unwrap();
            prev = Some(c);
        }
        // 4 spawns → 3 terminations of the first 3 PIDs, in order.
        assert_eq!(spawner.terminated(), vec![1000, 1001, 1002]);
    }

    // -------- spawn_replacing_keyed (widget palier state machine) --------

    #[test]
    fn keyed_spawn_isolates_different_keys() {
        let spawner = FakeSpawner::new();
        let mut map: HashMap<u32, FakeChild> = HashMap::new();
        spawn_replacing_keyed(&spawner, dummy_exe(), &[], 10u32, &mut map).unwrap();
        spawn_replacing_keyed(&spawner, dummy_exe(), &[], 15u32, &mut map).unwrap();
        // Firing palier 15 must not disturb palier 10.
        assert!(map.contains_key(&10));
        assert!(map.contains_key(&15));
        assert!(spawner.terminated().is_empty());
    }

    #[test]
    fn keyed_spawn_same_key_terminates_and_replaces() {
        let spawner = FakeSpawner::new();
        let mut map: HashMap<u32, FakeChild> = HashMap::new();
        spawn_replacing_keyed(&spawner, dummy_exe(), &[], 10u32, &mut map).unwrap();
        spawn_replacing_keyed(&spawner, dummy_exe(), &[], 10u32, &mut map).unwrap();
        assert_eq!(spawner.terminated(), vec![1000]);
        assert_eq!(map.get(&10).unwrap().pid(), 1001);
    }

    #[test]
    fn keyed_spawn_failure_preserves_all_map_entries() {
        let spawner = FakeSpawner::new();
        let mut map: HashMap<u32, FakeChild> = HashMap::new();
        spawn_replacing_keyed(&spawner, dummy_exe(), &[], 10u32, &mut map).unwrap();
        spawn_replacing_keyed(&spawner, dummy_exe(), &[], 15u32, &mut map).unwrap();

        spawner.fail_next_spawn();
        let result = spawn_replacing_keyed(&spawner, dummy_exe(), &[], 10u32, &mut map);
        assert!(result.is_err());
        // Both entries survive intact.
        assert_eq!(map.get(&10).unwrap().pid(), 1000);
        assert_eq!(map.get(&15).unwrap().pid(), 1001);
        assert!(spawner.terminated().is_empty());
    }
}
