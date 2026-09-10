//! Audited Windows security boundary for per-user IPC objects.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetFileSecurityW,
    GetKernelObjectSecurity, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const SECURITY_INFORMATION: u32 = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
const SET_SECURITY_INFORMATION: u32 =
    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// Self-relative descriptor allocated by `LocalAlloc` inside the SDDL converter.
pub(crate) struct PrivateSecurityDescriptor {
    pointer: PSECURITY_DESCRIPTOR,
}

impl PrivateSecurityDescriptor {
    pub(crate) fn current_user(inheritable: bool) -> io::Result<Self> {
        let sid = current_user_sid_string()?;
        let inheritance = if inheritable { "OICI" } else { "" };
        let sddl = format!("O:{sid}D:P(A;{inheritance};GA;;;{sid})(A;{inheritance};GA;;;SY)");
        descriptor_from_sddl(&sddl)
    }

    pub(crate) fn security_attributes(&mut self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
            lpSecurityDescriptor: self.pointer,
            bInheritHandle: 0,
        }
    }

    pub(crate) fn create_named_pipe(
        &mut self,
        options: &ServerOptions,
        name: &str,
    ) -> io::Result<NamedPipeServer> {
        let mut attributes = self.security_attributes();
        // SAFETY: the security descriptor and attributes remain alive for the call;
        // Windows copies the descriptor while creating the kernel object.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
        }
    }
}

impl Drop for PrivateSecurityDescriptor {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            // SAFETY: `pointer` came from the SDDL converter and is released once.
            unsafe {
                LocalFree(self.pointer);
            }
        }
    }
}

pub(crate) fn secure_path(path: &Path, inheritable: bool) -> io::Result<()> {
    let expected = PrivateSecurityDescriptor::current_user(inheritable)?;
    let path = wide_path(path);
    // SAFETY: both pointers are valid and NUL-terminated/the descriptor outlives the call.
    if unsafe { SetFileSecurityW(path.as_ptr(), SET_SECURITY_INFORMATION, expected.pointer) } == 0 {
        return Err(io::Error::last_os_error());
    }
    validate_path_descriptor(&path, &expected)
}

pub(crate) fn validate_path(path: &Path, inheritable: bool) -> io::Result<()> {
    let expected = PrivateSecurityDescriptor::current_user(inheritable)?;
    validate_path_descriptor(&wide_path(path), &expected)
}

pub(crate) fn validate_named_pipe(server: &NamedPipeServer) -> io::Result<()> {
    let handle = server.as_raw_handle().cast::<c_void>();
    let expected = PrivateSecurityDescriptor::current_user(false)?;
    let actual = read_kernel_descriptor(handle)?;
    validate_descriptor(actual.as_ptr().cast_mut().cast(), expected.pointer)
}

fn descriptor_from_sddl(sddl: &str) -> io::Result<PrivateSecurityDescriptor> {
    let encoded: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pointer = null_mut();
    // SAFETY: `encoded` is NUL-terminated and `pointer` is a valid out parameter.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            encoded.as_ptr(),
            SDDL_REVISION_1,
            &mut pointer,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(PrivateSecurityDescriptor { pointer })
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: the pseudo process handle is valid and `token` is an out parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut bytes_needed = 0_u32;
    // SAFETY: this is the documented sizing call; a null buffer with length zero is valid.
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut bytes_needed);
    }
    if bytes_needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = aligned_storage(bytes_needed)?;
    // SAFETY: `storage` is aligned and contains at least `bytes_needed` writable bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            bytes_needed,
            &mut bytes_needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `TokenUser` query initializes a `TOKEN_USER` at the buffer start.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    sid_to_string(user.User.Sid)
}

fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut encoded = null_mut();
    // SAFETY: `sid` comes from a successful token query and `encoded` is an out parameter.
    if unsafe { ConvertSidToStringSidW(sid, &mut encoded) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let encoded = LocalWideString(encoded);
    let mut length = 0_usize;
    // A Windows SID string is much shorter; the bound also protects malformed FFI output.
    while length <= 256 {
        // SAFETY: the converter returns a NUL-terminated allocation.
        if unsafe { *encoded.0.add(length) } == 0 {
            // SAFETY: all elements through `length` were checked inside the allocation.
            let units = unsafe { std::slice::from_raw_parts(encoded.0, length) };
            return String::from_utf16(units)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid user SID"));
        }
        length += 1;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "user SID string exceeds the safety bound",
    ))
}

fn validate_path_descriptor(path: &[u16], expected: &PrivateSecurityDescriptor) -> io::Result<()> {
    let actual = read_path_descriptor(path)?;
    validate_descriptor(actual.as_ptr().cast_mut().cast(), expected.pointer)
}

fn read_path_descriptor(path: &[u16]) -> io::Result<Vec<usize>> {
    let mut bytes_needed = 0_u32;
    // SAFETY: this is the documented sizing call with a valid path and null output buffer.
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            SECURITY_INFORMATION,
            null_mut(),
            0,
            &mut bytes_needed,
        );
    }
    let mut storage = descriptor_storage(bytes_needed)?;
    // SAFETY: `storage` is aligned and large enough for the reported descriptor size.
    if unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            SECURITY_INFORMATION,
            storage.as_mut_ptr().cast(),
            bytes_needed,
            &mut bytes_needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

fn read_kernel_descriptor(handle: HANDLE) -> io::Result<Vec<usize>> {
    let mut bytes_needed = 0_u32;
    // SAFETY: this is the documented sizing call for a live kernel handle.
    unsafe {
        GetKernelObjectSecurity(
            handle,
            SECURITY_INFORMATION,
            null_mut(),
            0,
            &mut bytes_needed,
        );
    }
    let mut storage = descriptor_storage(bytes_needed)?;
    // SAFETY: `storage` is aligned and large enough for the reported descriptor size.
    if unsafe {
        GetKernelObjectSecurity(
            handle,
            SECURITY_INFORMATION,
            storage.as_mut_ptr().cast(),
            bytes_needed,
            &mut bytes_needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

fn descriptor_storage(bytes_needed: u32) -> io::Result<Vec<usize>> {
    if bytes_needed == 0 {
        return Err(io::Error::last_os_error());
    }
    aligned_storage(bytes_needed)
}

fn aligned_storage(bytes_needed: u32) -> io::Result<Vec<usize>> {
    let bytes = usize::try_from(bytes_needed)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "security buffer is too large"))?;
    let words = bytes.div_ceil(size_of::<usize>());
    Ok(vec![0_usize; words])
}

fn validate_descriptor(
    actual: PSECURITY_DESCRIPTOR,
    expected: PSECURITY_DESCRIPTOR,
) -> io::Result<()> {
    let actual_parts = descriptor_parts(actual)?;
    let expected_parts = descriptor_parts(expected)?;
    // SAFETY: descriptor parsing validated both owner SID pointers.
    if unsafe { EqualSid(actual_parts.owner, expected_parts.owner) } == 0 {
        return Err(insecure(
            "security descriptor owner is not the current user",
        ));
    }
    if actual_parts.dacl.is_null() || expected_parts.dacl.is_null() {
        return Err(insecure("security descriptor has a null DACL"));
    }
    validate_allowed_principals(actual_parts.dacl, expected_parts.dacl, expected_parts.owner)?;
    if actual_parts.control & SE_DACL_PROTECTED == 0 {
        return Err(insecure("security descriptor DACL permits inheritance"));
    }
    Ok(())
}

fn validate_allowed_principals(actual: *mut ACL, expected: *mut ACL, user: PSID) -> io::Result<()> {
    let expected_sids = allowed_ace_sids(expected)?;
    let system = expected_sids
        .into_iter()
        // SAFETY: both SIDs belong to the live expected descriptor.
        .find(|sid| unsafe { EqualSid(*sid, user) } == 0)
        .ok_or_else(|| insecure("expected descriptor lacks the LocalSystem principal"))?;
    let actual_sids = allowed_ace_sids(actual)?;
    if actual_sids.len() != 2 {
        return Err(insecure(
            "security descriptor DACL has an unexpected principal count",
        ));
    }
    let mut has_user = false;
    let mut has_system = false;
    for sid in actual_sids {
        // SAFETY: all SIDs belong to the live actual/expected descriptors.
        if unsafe { EqualSid(sid, user) } != 0 {
            has_user = true;
        // SAFETY: all SIDs belong to the live actual/expected descriptors.
        } else if unsafe { EqualSid(sid, system) } != 0 {
            has_system = true;
        } else {
            return Err(insecure(
                "security descriptor DACL grants access outside the current user and SYSTEM",
            ));
        }
    }
    if !has_user || !has_system {
        return Err(insecure(
            "security descriptor DACL must grant the current user and SYSTEM",
        ));
    }
    Ok(())
}

fn allowed_ace_sids(acl: *mut ACL) -> io::Result<Vec<PSID>> {
    // SAFETY: `acl` came from a parsed security descriptor and is non-null.
    let count = unsafe { (*acl).AceCount };
    let mut sids = Vec::with_capacity(usize::from(count));
    for index in 0..u32::from(count) {
        let mut ace = null_mut();
        // SAFETY: `index` is within `AceCount` and `ace` is a valid out parameter.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: every ACE begins with `ACE_HEADER`; only the allowed layout is cast below.
        if unsafe { (*allowed).Header.AceType } != ACCESS_ALLOWED_ACE_TYPE {
            return Err(insecure(
                "security descriptor DACL contains a non-allow ACE",
            ));
        }
        // SAFETY: for an ACCESS_ALLOWED_ACE, `SidStart` is the first byte of its SID.
        let sid = unsafe { std::ptr::addr_of_mut!((*allowed).SidStart).cast::<c_void>() };
        sids.push(sid);
    }
    Ok(sids)
}

fn descriptor_parts(descriptor: PSECURITY_DESCRIPTOR) -> io::Result<DescriptorParts> {
    let mut owner = null_mut();
    let mut owner_defaulted = 0;
    // SAFETY: the caller supplies a live security descriptor and valid out parameters.
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut dacl_present = 0;
    let mut dacl = null_mut();
    let mut dacl_defaulted = 0;
    // SAFETY: the caller supplies a live security descriptor and valid out parameters.
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: the caller supplies a live security descriptor and valid out parameters.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null() || dacl_present == 0 {
        return Err(insecure("security descriptor lacks an owner or DACL"));
    }
    Ok(DescriptorParts {
        owner,
        dacl,
        control,
    })
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn insecure(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

struct DescriptorParts {
    owner: PSID,
    dacl: *mut ACL,
    control: u16,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this wrapper uniquely owns the handle returned by `OpenProcessToken`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct LocalWideString(*mut u16);

impl Drop for LocalWideString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the string came from `ConvertSidToStringSidW` and is released once.
            unsafe {
                LocalFree(self.0.cast::<c_void>());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        PrivateSecurityDescriptor, current_user_sid_string, descriptor_from_sddl,
        validate_descriptor,
    };

    #[test]
    fn private_descriptor_accepts_only_current_user_and_system() {
        let descriptor = PrivateSecurityDescriptor::current_user(false).unwrap();
        validate_descriptor(descriptor.pointer, descriptor.pointer).unwrap();
    }

    #[test]
    fn descriptor_with_everyone_access_is_rejected() {
        let sid = current_user_sid_string().unwrap();
        let expected = PrivateSecurityDescriptor::current_user(false).unwrap();
        let actual = descriptor_from_sddl(&format!(
            "O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)(A;;GR;;;WD)"
        ))
        .unwrap();
        let error = validate_descriptor(actual.pointer, expected.pointer).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
