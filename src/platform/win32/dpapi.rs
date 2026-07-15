// DPAPI-machine wrap for state.dat.
//
// `CryptProtectData` / `CryptUnprotectData` with `CRYPTPROTECT_LOCAL_MACHINE`
// derive the key from the machine's LSA secret rather than the user
// profile — so the SYSTEM service and any elevated admin process on the
// same host can decrypt, but copying state.dat to another machine yields
// undecryptable bytes.

use anyhow::{anyhow, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE,
};

pub fn encrypt(plaintext: &[u8]) -> Result<Vec<u8>> {
    crypt(plaintext, /* protect */ true)
}

pub fn decrypt(ciphertext: &[u8]) -> Result<Vec<u8>> {
    crypt(ciphertext, /* protect */ false)
}

fn crypt(input: &[u8], protect: bool) -> Result<Vec<u8>> {
    let mut in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();

    let result = unsafe {
        if protect {
            CryptProtectData(
                &mut in_blob,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_LOCAL_MACHINE,
                &mut out_blob,
            )
        } else {
            CryptUnprotectData(
                &mut in_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_LOCAL_MACHINE,
                &mut out_blob,
            )
        }
    };

    result.map_err(|e| anyhow!("{}: {e}", if protect { "CryptProtectData" } else { "CryptUnprotectData" }))?;

    // Copy out then free the blob owned by wincrypt via LocalFree.
    let slice = unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) };
    let owned = slice.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData as _)));
    }
    Ok(owned)
}
