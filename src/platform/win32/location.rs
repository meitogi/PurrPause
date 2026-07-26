// Windows known-folder resolution for the install-location guard.
//
// The break popup runs WebView2 as an unelevated worker that cannot create its
// UserDataFolder inside a Windows-protected root (Program Files / Windows), so
// an install there is doomed. `protected_roots` enumerates those roots by their
// stable KNOWNFOLDERID GUIDs — NOT by string-matching "Program Files" (the drive
// letter varies and WOW64 redirection would fool a literal match).
//
// `recommended_target_dir` returns the user-writable folder the guard dialog
// offers to open (%LOCALAPPDATA%\Microsoft\SystemHealthAgent).

use std::path::PathBuf;

use windows::core::{GUID, PWSTR};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, FOLDERID_LocalAppData, FOLDERID_ProgramFiles,
    FOLDERID_ProgramFilesX86, FOLDERID_Windows, KF_FLAG_DEFAULT,
};

/// Resolve a KNOWNFOLDERID to its filesystem path, or `None` on failure.
/// Frees the shell-allocated string with `CoTaskMemFree` per the API contract
/// (SHGetKnownFolderPath needs no COM apartment).
fn known_folder(rfid: &GUID) -> Option<PathBuf> {
    unsafe {
        let pwstr: PWSTR = SHGetKnownFolderPath(rfid, KF_FLAG_DEFAULT, None).ok()?;
        if pwstr.is_null() {
            return None;
        }
        let path = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// Windows-protected roots an install must never land in : both Program Files
/// views (a 64-bit process sees x64 + x86) and the Windows directory. Any that
/// fail to resolve are skipped — the guard treats an empty list as "not
/// protected", so a resolver failure never falsely blocks an install.
pub fn protected_roots() -> Vec<PathBuf> {
    [
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
        &FOLDERID_Windows,
    ]
    .into_iter()
    .filter_map(known_folder)
    .collect()
}

/// The recommended, user-writable install destination the guard dialog opens :
/// `%LOCALAPPDATA%\Microsoft\SystemHealthAgent`. `None` if LocalAppData can't be
/// resolved.
pub fn recommended_target_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData).map(|p| p.join("Microsoft").join("SystemHealthAgent"))
}
