//! Native LPAC launch with a strict handle list. Async files use Tokio's
//! bounded blocking adapter, matching its Windows child-stdio implementation.
use super::*;
use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::ExitStatus,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    Storage::FileSystem::*,
    System::{Pipes::CreatePipe, Threading::*},
};

pub(crate) struct WindowsChild {
    process: OwnedHandle,
    pub(crate) stdin: Option<tokio::fs::File>,
    pub(crate) stdout: Option<tokio::fs::File>,
}
impl WindowsChild {
    pub(crate) fn id(&self) -> Option<u32> {
        let pid = unsafe { GetProcessId(self.raw_handle()) };
        (pid != 0).then_some(pid)
    }
    pub(crate) fn raw_handle(&self) -> HANDLE {
        self.process.as_raw_handle()
    }
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match unsafe { WaitForSingleObject(self.raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.raw_handle(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub(crate) fn start_kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        if unsafe { TerminateProcess(self.raw_handle(), 3) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
impl Drop for WindowsChild {
    fn drop(&mut self) {
        let _ = self.start_kill();
    }
}
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct AttributeBlock([u8; 16]);
struct Attributes {
    words: Vec<AttributeBlock>,
}
impl Attributes {
    fn new(count: u32) -> Result<Self, ScriptWorkerError> {
        let mut length = 0;
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut length);
        }
        if length == 0 || length > 64 * 1024 {
            return Err(ScriptWorkerError::Isolation);
        }
        let mut value = Self {
            words: vec![AttributeBlock([0; 16]); length.div_ceil(size_of::<AttributeBlock>())],
        };
        if unsafe { InitializeProcThreadAttributeList(value.pointer(), count, 0, &mut length) } == 0
        {
            // No successful list exists to delete on this path.
            value.words.clear();
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(value)
    }
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.words.as_mut_ptr().cast()
    }
    fn set<T>(&mut self, key: u32, value: &T) -> Result<(), ScriptWorkerError> {
        // All values are declared before the list and outlive its destructor.
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                key as usize,
                (value as *const T).cast(),
                size_of::<T>(),
                ptr::null_mut(),
                ptr::null(),
            )
        } == 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        if !self.words.is_empty() {
            unsafe {
                DeleteProcThreadAttributeList(self.pointer());
            }
        }
    }
}
fn pipe() -> Result<(OwnedHandle, OwnedHandle), ScriptWorkerError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(ScriptWorkerError::Isolation);
    }
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}
fn path_wide(path: &Path) -> Result<Vec<u16>, ScriptWorkerError> {
    let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.is_empty() || value.iter().any(|c| *c == 0 || *c == b'"' as u16) {
        return Err(ScriptWorkerError::Isolation);
    }
    value.push(0);
    Ok(value)
}
fn environment() -> Result<Vec<u16>, ScriptWorkerError> {
    // OS-derived SystemRoot supports side-by-side DLL loading without trusting
    // a caller environment override. No user environment or values are copied.
    let mut root = vec![0u16; 32_768];
    let length = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW(
            root.as_mut_ptr(),
            root.len() as u32,
        )
    } as usize;
    if length == 0 || length >= root.len() {
        return Err(ScriptWorkerError::Isolation);
    }
    let mut block = identity::wide("SystemRoot=");
    block.pop();
    block.extend_from_slice(&root[..length]);
    block.push(0);
    block.extend(identity::wide("TZ=UTC0"));
    block.push(0);
    Ok(block)
}
fn stage(
    source: &Path,
    directory: &Path,
    sid: &identity::Sid,
) -> Result<PathBuf, ScriptWorkerError> {
    // Copy only this executable. No project/config/DLL directory ACL is changed.
    let mut input = File::open(source).map_err(|_| ScriptWorkerError::Isolation)?;
    let metadata = input.metadata().map_err(|_| ScriptWorkerError::Isolation)?;
    const MAX: u64 = 128 * 1024 * 1024;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX {
        return Err(ScriptWorkerError::Isolation);
    }
    let target = directory.join("cshell-script-worker.exe");
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&target)
        .map_err(|_| ScriptWorkerError::Isolation)?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| ScriptWorkerError::Isolation)?;
        if count == 0 {
            break;
        }
        copied += count as u64;
        if copied > MAX {
            return Err(ScriptWorkerError::Isolation);
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| ScriptWorkerError::Isolation)?;
    }
    if copied != metadata.len()
        || input
            .metadata()
            .map_err(|_| ScriptWorkerError::Isolation)?
            .len()
            != copied
    {
        return Err(ScriptWorkerError::Isolation);
    }
    output
        .sync_all()
        .map_err(|_| ScriptWorkerError::Isolation)?;
    drop(output);
    identity::grant_read_execute(&target, sid)?;
    identity::grant_read_execute(directory, sid)?;
    Ok(target)
}
pub(super) fn spawn(
    containment: &mut ParentContainment,
    source: &Path,
    directory: &Path,
) -> Result<WindowsChild, ScriptWorkerError> {
    let executable = stage(source, directory, &containment.profile.sid)?;
    let application = path_wide(&executable)?;
    let directory = path_wide(directory)?;
    let mut command = vec![b'"' as u16];
    command.extend_from_slice(&application[..application.len() - 1]);
    command.extend(
        OsStr::new(&format!(
            "\" {} --parent-pid {}",
            crate::WORKER_STDIO_ARGUMENT,
            std::process::id()
        ))
        .encode_wide(),
    );
    command.push(0);
    let (input_read, input_write) = pipe()?;
    let (output_read, output_write) = pipe()?;
    for handle in [&input_write, &output_read] {
        if unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(ScriptWorkerError::Isolation);
        }
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let null = unsafe {
        CreateFileW(
            identity::wide("NUL").as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if null == INVALID_HANDLE_VALUE {
        return Err(ScriptWorkerError::Isolation);
    }
    let null = unsafe { OwnedHandle::from_raw_handle(null) };
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: containment.profile.sid.pointer(),
        Capabilities: ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let handles = [
        input_read.as_raw_handle(),
        output_write.as_raw_handle(),
        null.as_raw_handle(),
    ];
    let jobs = [containment
        .handle
        .as_ref()
        .ok_or(ScriptWorkerError::Isolation)?
        .as_raw_handle()];
    const ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 1;
    let lpac = ALL_APPLICATION_PACKAGES_OPT_OUT;
    // SDK policy macros are omitted by windows-sys. Documented DWORD64 fields:
    // Win32k disable (28), extension point disable (32), dynamic code deny (36).
    let mitigations: u64 = (1 << 28) | (1 << 32) | (1 << 36);
    let mut list = Attributes::new(5)?;
    list.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities)?;
    list.set(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY, &lpac)?;
    list.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
    list.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    list.set(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigations)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = list.pointer();
    let environment = environment()?;
    let mut information = PROCESS_INFORMATION::default();
    // SAFETY: every buffer/attribute value is initialized and lives through
    // list destruction. Only the three explicit inheritable handles are given
    // to this child; LPAC and its Job are installed atomically at creation.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            &startup.StartupInfo,
            &mut information,
        )
    } == 0
    {
        return Err(ScriptWorkerError::Isolation);
    }
    let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(information.hThread) };
    containment.profile.started();
    // No fallible initialization after creating the live process.
    drop(thread);
    Ok(WindowsChild {
        process,
        stdin: Some(tokio::fs::File::from_std(File::from(input_write))),
        stdout: Some(tokio::fs::File::from_std(File::from(output_read))),
    })
}
