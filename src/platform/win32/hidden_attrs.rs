// Set FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM on a directory or
// file so it's invisible in Explorer even with "Show hidden files"
// enabled — the "Show protected operating system files" toggle is
// off by default and gated by a scary UAC-style confirmation.

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{
    SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM,
};

pub fn set_hidden_system(path: &Path) -> Result<()> {
    let wide: HSTRING = path.as_os_str().into();
    unsafe { SetFileAttributesW(&wide, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) }
        .with_context(|| format!("SetFileAttributesW({})", path.display()))
}
