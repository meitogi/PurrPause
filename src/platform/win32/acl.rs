// Apply a restrictive DACL to C:\ProgramData\DiagnosticsCache\.
//
// SDDL breakdown :
//   D:P                   — DACL protected (no inheritance from parent)
//   (A;OICI;FA;;;SY)      — Allow SYSTEM Full, inherited by children
//   (A;OICI;FA;;;BA)      — Allow BUILTIN\Administrators Full, inherited
//   (A;OICI;GRGX;;;BU)    — Allow BUILTIN\Users Read+Execute, inherited
//   (D;OICI;DC;;;BU)      — Deny BUILTIN\Users FILE_DELETE_CHILD, inherited
//
// Effect for a child admin : can read (state.dat opaque anyway), can't
// delete files without first taking ownership through the Windows
// properties dialog — 3+ clicks of friction, consistent with the design
// philosophy "friction, not fort-knox".

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};

const SDDL: &str =
    "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)(D;OICI;DC;;;BU)";

pub fn apply_diagnostics_cache_dacl(path: &Path) -> Result<()> {
    let sddl_wide: HSTRING = SDDL.into();
    let mut sd = PSECURITY_DESCRIPTOR::default();

    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl_wide,
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;

    // Extract the DACL from the just-built security descriptor. On the
    // absolute-format SD returned here, offset math via GetSecurityDescriptorDacl
    // is cleaner than manual layout.
    let dacl = unsafe { extract_dacl(sd)? };

    let path_wide: HSTRING = path.as_os_str().into();
    let ret = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR::from_raw(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };

    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0 as _)));
    }

    if ret.is_ok() {
        Ok(())
    } else {
        Err(anyhow!("SetNamedSecurityInfoW: {ret:?}"))
    }
}

unsafe fn extract_dacl(sd: PSECURITY_DESCRIPTOR) -> Result<*const ACL> {
    use windows::core::BOOL;
    use windows::Win32::Security::GetSecurityDescriptorDacl;
    let mut present = BOOL(0);
    let mut defaulted = BOOL(0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)
        .context("GetSecurityDescriptorDacl")?;
    if present.0 == 0 || dacl.is_null() {
        return Err(anyhow!("DACL missing from parsed SDDL"));
    }
    Ok(dacl as *const ACL)
}
