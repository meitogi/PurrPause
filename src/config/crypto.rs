// DPAPI-machine wrap for state.dat contents.
//
// On Windows : delegate to `platform::win32::dpapi` (CryptProtectData with
// CRYPTPROTECT_LOCAL_MACHINE — clef dérivée du secret machine LSA).
//
// On non-Windows : identity function. Only used to keep `cargo test` on
// Linux exercising the serde+file-IO pipeline. The devcontainer can't
// exec .exe, and real host smoke does the actual DPAPI validation.

use anyhow::Result;

#[cfg(windows)]
pub fn encrypt(plaintext: &[u8]) -> Result<Vec<u8>> {
    crate::platform::win32::dpapi::encrypt(plaintext)
}

#[cfg(windows)]
pub fn decrypt(ciphertext: &[u8]) -> Result<Vec<u8>> {
    crate::platform::win32::dpapi::decrypt(ciphertext)
}

#[cfg(not(windows))]
pub fn encrypt(plaintext: &[u8]) -> Result<Vec<u8>> {
    Ok(plaintext.to_vec())
}

#[cfg(not(windows))]
pub fn decrypt(ciphertext: &[u8]) -> Result<Vec<u8>> {
    Ok(ciphertext.to_vec())
}
