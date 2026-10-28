//! SID/token ownership stays in this adapter; no token or SID enters JS/IPC.
use super::*;
use crate::ScriptRunId;
use std::{
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, LocalFree},
    Security::{Authorization::*, Isolation::*, *},
    System::Threading::OpenProcessToken,
};

pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn name(run_id: ScriptRunId) -> Vec<u16> {
    wide(&format!("CShell.Script.{run_id}"))
}
#[cfg(feature = "supervisor")]
pub(super) fn cleanup_orphan(run_id: ScriptRunId) -> bool {
    // Called only under the durable slot lock after exit/absence proof.
    let result = unsafe { DeleteAppContainerProfile(name(run_id).as_ptr()) };
    result >= 0 // The API also succeeds for a nonexistent profile.
}
struct ApiSid(PSID);
impl Drop for ApiSid {
    fn drop(&mut self) {
        unsafe {
            FreeSid(self.0);
        }
    }
}
pub(super) struct Sid {
    words: Vec<u32>,
}
impl Sid {
    pub(super) fn pointer(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }
    fn copy(raw: PSID) -> Result<Self, ScriptWorkerError> {
        // SAFETY: input comes from a successful SID API or token information.
        if raw.is_null() || unsafe { IsValidSid(raw) } == 0 {
            return Err(ScriptWorkerError::Isolation);
        }
        let length = unsafe { GetLengthSid(raw) };
        if !(8..=68).contains(&length) {
            return Err(ScriptWorkerError::Isolation);
        }
        let sid = Self {
            words: vec![0; (length as usize).div_ceil(size_of::<u32>())],
        };
        if unsafe { CopySid(length, sid.pointer(), raw) } == 0 {
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(sid)
    }
    fn derive(run_id: ScriptRunId) -> Result<Self, ScriptWorkerError> {
        let mut raw = ptr::null_mut();
        // SAFETY: terminated moniker and valid output, freed by ApiSid.
        if unsafe { DeriveAppContainerSidFromAppContainerName(name(run_id).as_ptr(), &mut raw) } < 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        let raw = ApiSid(raw);
        Self::copy(raw.0)
    }
    fn text(&self) -> Result<String, ScriptWorkerError> {
        let mut raw = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.pointer(), &mut raw) } == 0 {
            return Err(ScriptWorkerError::Isolation);
        }
        let allocation = Local(raw.cast());
        // SID strings are bounded by the maximum SID shape (15 subauthorities).
        let mut length = 0;
        while length < 256 && unsafe { *raw.add(length) } != 0 {
            length += 1;
        }
        if length == 256 {
            return Err(ScriptWorkerError::Isolation);
        }
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(raw, length) })
            .map_err(|_| ScriptWorkerError::Isolation);
        drop(allocation);
        value
    }
}
struct Local(*mut core::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn token(process: HANDLE) -> Result<OwnedHandle, ScriptWorkerError> {
    let mut raw = ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
struct Information {
    words: Vec<usize>,
    bytes: usize,
}
impl Information {
    fn get(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> Result<Self, ScriptWorkerError> {
        let mut length = 0;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                ptr::null_mut(),
                0,
                &mut length,
            );
        }
        if length == 0 || length > 16 * 1024 {
            return Err(ScriptWorkerError::Isolation);
        }
        let mut words = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        let capacity = length;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                words.as_mut_ptr().cast(),
                capacity,
                &mut length,
            )
        } == 0
            || length > capacity
        {
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(Self {
            words,
            bytes: length as usize,
        })
    }
    fn first_u32(&self) -> Result<u32, ScriptWorkerError> {
        if self.bytes < size_of::<u32>() {
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(unsafe { self.words.as_ptr().cast::<u32>().read() })
    }
    fn sid(&self, offset: usize) -> Result<Sid, ScriptWorkerError> {
        if offset + size_of::<PSID>() > self.bytes {
            return Err(ScriptWorkerError::Isolation);
        }
        // Token SID pointers must point into this same returned allocation.
        let base = self.words.as_ptr() as usize;
        let raw = unsafe {
            self.words
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<PSID>()
                .read_unaligned()
        };
        let address = raw as usize;
        if address < base
            || address
                .checked_add(8)
                .is_none_or(|end| end > base + self.bytes)
        {
            return Err(ScriptWorkerError::Isolation);
        }
        let count = unsafe { *((raw as *const u8).add(1)) } as usize;
        if count > 15 || address + 8 + 4 * count > base + self.bytes {
            return Err(ScriptWorkerError::Isolation);
        }
        Sid::copy(raw)
    }
}
pub(super) fn verify(process: HANDLE, run_id: ScriptRunId) -> Result<(), ScriptWorkerError> {
    let token = token(process)?;
    if Information::get(&token, TokenIsAppContainer)?.first_u32()? != 1
        || Information::get(&token, TokenIsLessPrivilegedAppContainer)?.first_u32()? != 1
        || Information::get(&token, TokenCapabilities)?.first_u32()? != 0
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let actual = Information::get(&token, TokenAppContainerSid)?.sid(0)?;
    let expected = Sid::derive(run_id)?;
    if unsafe { EqualSid(actual.pointer(), expected.pointer()) } == 0 {
        return Err(ScriptWorkerError::Isolation);
    }
    let integrity = Information::get(&token, TokenIntegrityLevel)?.sid(0)?;
    if integrity.words.len() < 3 {
        return Err(ScriptWorkerError::Isolation);
    }
    // SID revision, one subauthority, mandatory-label authority and Low or less.
    let bytes = unsafe { std::slice::from_raw_parts(integrity.pointer().cast::<u8>(), 12) };
    if bytes[1] != 1
        || bytes[2..8] != [0, 0, 0, 0, 0, 16]
        || u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) > 0x1000
    {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(())
}

#[cfg(feature = "supervisor")]
pub(super) struct Profile {
    name: Vec<u16>,
    pub(super) sid: Sid,
    active: bool,
    result: Option<bool>,
}
#[cfg(feature = "supervisor")]
impl Profile {
    pub(super) fn create(run_id: ScriptRunId) -> Result<Self, ScriptWorkerError> {
        let name = name(run_id);
        let display = wide("CShell ephemeral script");
        let mut raw = ptr::null_mut();
        // Fresh moniker only: an existing profile is never adopted or deleted.
        if unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                display.as_ptr(),
                ptr::null(),
                0,
                &mut raw,
            )
        } < 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        let raw = ApiSid(raw);
        let sid = match Sid::copy(raw.0) {
            Ok(sid) => sid,
            Err(error) => {
                unsafe {
                    DeleteAppContainerProfile(name.as_ptr());
                }
                return Err(error);
            }
        };
        Ok(Self {
            name,
            sid,
            active: false,
            result: None,
        })
    }
    pub(super) fn started(&mut self) {
        self.active = true;
    }
    pub(super) fn cleanup(&mut self) -> bool {
        if let Some(result) = self.result {
            return result;
        }
        self.active = false; // Caller has either no child or an exit proof.
        let result = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) } >= 0;
        self.result = Some(result);
        result
    }
}
#[cfg(feature = "supervisor")]
impl Drop for Profile {
    fn drop(&mut self) {
        if !self.active && self.result.is_none() {
            self.cleanup();
        }
    }
}
#[cfg(feature = "supervisor")]
pub(super) fn grant_read_execute(
    path: &std::path::Path,
    package: &Sid,
) -> Result<(), ScriptWorkerError> {
    use std::os::windows::ffi::OsStrExt;
    let user = Information::get(&token(unsafe { GetCurrentProcess() })?, TokenUser)?.sid(0)?;
    let descriptor = wide(&format!(
        "D:P(A;;FA;;;SY)(A;;FA;;;{})(A;;GRGX;;;{})",
        user.text()?,
        package.text()?
    ));
    let mut raw = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut raw,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let allocation = Local(raw);
    let mut dacl = ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorDacl(raw, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if path[..path.len() - 1].contains(&0) {
        return Err(ScriptWorkerError::Isolation);
    }
    let result = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    drop(allocation);
    if result != 0 {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(())
}
