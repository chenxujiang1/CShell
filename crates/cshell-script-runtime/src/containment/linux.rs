//! Linux offline access policy. The trusted startup path runs before this
//! filter; no user JS runs until TSYNC has covered every existing worker thread.
#![allow(unsafe_code)]
use crate::ScriptWorkerError;

#[cfg(all(
    target_pointer_width = "64",
    target_endian = "little",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native {
    use super::*;
    use libc::{c_long, sock_filter, sock_fprog};
    use std::mem::MaybeUninit;

    // Linux UAPI classic BPF encodings; seccomp_data: nr at 0, arch at 4,
    // six u64 arguments from 16. This adapter supports little-endian LP64 only.
    const LOAD: u16 = 0x20;
    const JEQ: u16 = 0x15;
    const JGT: u16 = 0x25;
    const JSET: u16 = 0x45;
    const AND: u16 = 0x54;
    const RET: u16 = 0x06;
    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64

    fn instruction(code: u16, k: u32) -> sock_filter {
        sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }
    fn jump(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter { code, jt, jf, k }
    }
    fn low(argument: u32) -> u32 {
        16 + argument * 8
    }
    fn high(argument: u32) -> u32 {
        low(argument) + 4
    }
    fn canonical(program: &mut Vec<sock_filter>, argument: u32) {
        // Reject high-word aliases of scalar int/protection/flag arguments.
        program.extend([
            instruction(LOAD, high(argument)),
            jump(JEQ, 0, 1, 0),
            instruction(RET, DENY),
        ]);
    }
    fn one_of(argument: u32, values: &[u32]) -> Vec<sock_filter> {
        let mut body = Vec::new();
        canonical(&mut body, argument);
        body.push(instruction(LOAD, low(argument)));
        for value in values {
            body.extend([jump(JEQ, *value, 0, 1), instruction(RET, ALLOW)]);
        }
        body.push(instruction(RET, DENY));
        body
    }
    fn no_exec(body: &mut Vec<sock_filter>) {
        canonical(body, 2);
        body.extend([
            instruction(LOAD, low(2)),
            jump(JSET, libc::PROT_EXEC as u32, 0, 1),
            instruction(RET, DENY),
        ]);
    }
    fn rule(
        program: &mut Vec<sock_filter>,
        syscall: c_long,
        body: &[sock_filter],
    ) -> Result<(), ScriptWorkerError> {
        // Matching bodies always return. A nonmatching syscall skips the whole
        // body, retaining nr in the accumulator for the next comparison.
        let skip = u8::try_from(body.len()).map_err(|_| ScriptWorkerError::Isolation)?;
        if body.is_empty() || body.last().is_none_or(|last| last.code != RET) {
            return Err(ScriptWorkerError::Isolation);
        }
        program.push(jump(JEQ, syscall as u32, 0, skip));
        program.extend_from_slice(body);
        Ok(())
    }
    fn filter(pid: u32) -> Result<Vec<sock_filter>, ScriptWorkerError> {
        let mut program = vec![
            instruction(LOAD, 4),
            jump(JEQ, ARCH, 1, 0),
            instruction(RET, libc::SECCOMP_RET_KILL_PROCESS),
            instruction(LOAD, 0),
        ];
        #[cfg(target_arch = "x86_64")]
        program.extend([
            jump(JSET, 0x4000_0000, 0, 1),
            instruction(RET, libc::SECCOMP_RET_KILL_PROCESS),
        ]);

        for syscall in [libc::SYS_read, libc::SYS_readv] {
            rule(&mut program, syscall, &one_of(0, &[0]))?;
        }
        for syscall in [libc::SYS_write, libc::SYS_writev] {
            rule(&mut program, syscall, &one_of(0, &[1, 2]))?;
        }
        for syscall in [libc::SYS_close, libc::SYS_fstat] {
            rule(&mut program, syscall, &one_of(0, &[0, 1, 2]))?;
        }
        let mut fcntl = Vec::new();
        canonical(&mut fcntl, 0);
        fcntl.extend([
            instruction(LOAD, low(0)),
            jump(JGT, 2, 0, 1),
            instruction(RET, DENY),
        ]);
        fcntl.extend(one_of(1, &[libc::F_GETFD as u32, libc::F_GETFL as u32]));
        rule(&mut program, libc::SYS_fcntl, &fcntl)?;

        let mut mapping = Vec::new();
        no_exec(&mut mapping);
        canonical(&mut mapping, 3);
        let map_flags = libc::MAP_PRIVATE
            | libc::MAP_ANONYMOUS
            | libc::MAP_NORESERVE
            | libc::MAP_STACK
            | libc::MAP_FIXED
            | libc::MAP_FIXED_NOREPLACE;
        mapping.extend([
            instruction(LOAD, low(3)),
            jump(JSET, !map_flags as u32, 0, 1),
            instruction(RET, DENY),
            instruction(AND, (libc::MAP_TYPE | libc::MAP_ANONYMOUS) as u32),
            jump(JEQ, (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as u32, 1, 0),
            instruction(RET, DENY),
            instruction(RET, ALLOW),
        ]);
        rule(&mut program, libc::SYS_mmap, &mapping)?;
        let mut protection = Vec::new();
        no_exec(&mut protection);
        protection.push(instruction(RET, ALLOW));
        rule(&mut program, libc::SYS_mprotect, &protection)?;
        let mut remapping = Vec::new();
        canonical(&mut remapping, 3);
        remapping.extend([
            instruction(LOAD, low(3)),
            jump(
                JSET,
                !(libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED) as u32,
                0,
                1,
            ),
            instruction(RET, DENY),
            instruction(RET, ALLOW),
        ]);
        rule(&mut program, libc::SYS_mremap, &remapping)?;
        rule(
            &mut program,
            libc::SYS_madvise,
            &one_of(
                2,
                &[
                    libc::MADV_NORMAL as u32,
                    libc::MADV_DONTNEED as u32,
                    libc::MADV_FREE as u32,
                ],
            ),
        )?;
        // libc abort/raise may signal this process's threads, never the parent
        // or another process. The kernel checks tid membership in this tgid.
        rule(&mut program, libc::SYS_tgkill, &one_of(0, &[pid]))?;
        // Dynamic negative clock IDs can encode another PID or a device fd;
        // permit only named system clocks and this process/thread's CPU clocks.
        rule(
            &mut program,
            libc::SYS_clock_gettime,
            &one_of(
                0,
                &[
                    libc::CLOCK_REALTIME as u32,
                    libc::CLOCK_MONOTONIC as u32,
                    libc::CLOCK_MONOTONIC_RAW as u32,
                    libc::CLOCK_BOOTTIME as u32,
                    libc::CLOCK_PROCESS_CPUTIME_ID as u32,
                    libc::CLOCK_THREAD_CPUTIME_ID as u32,
                ],
            ),
        )?;
        rule(
            &mut program,
            libc::SYS_clock_nanosleep,
            &one_of(
                0,
                &[
                    libc::CLOCK_REALTIME as u32,
                    libc::CLOCK_MONOTONIC as u32,
                    libc::CLOCK_BOOTTIME as u32,
                ],
            ),
        )?;

        for syscall in [
            libc::SYS_brk,
            libc::SYS_munmap,
            libc::SYS_gettimeofday,
            libc::SYS_nanosleep,
            libc::SYS_futex,
            libc::SYS_sched_yield,
            libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_rt_sigreturn,
            libc::SYS_sigaltstack,
            libc::SYS_set_tid_address,
            libc::SYS_set_robust_list,
            libc::SYS_rseq,
            libc::SYS_getrandom,
            libc::SYS_getpid,
            libc::SYS_gettid,
            libc::SYS_getppid,
            libc::SYS_restart_syscall,
            libc::SYS_exit,
            libc::SYS_exit_group,
        ] {
            rule(&mut program, syscall, &[instruction(RET, ALLOW)])?;
        }
        #[cfg(target_arch = "x86_64")]
        rule(&mut program, libc::SYS_time, &[instruction(RET, ALLOW)])?;
        // Includes path opens/stat/readlink, sockets, io_uring, exec/fork/clone,
        // ptrace/process_vm/pidfd, descriptor passing/duplication and prctl.
        program.push(instruction(RET, DENY));
        if program.len() > 4096 {
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(program)
    }
    pub(super) fn close_extra_descriptors() -> Result<(), ScriptWorkerError> {
        // SAFETY: only the dedicated worker's table is changed. No descriptor
        // beyond the private stdio protocol is part of its runtime contract.
        if unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                0 as libc::c_uint,
            )
        } != 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        for fd in 0..=2 {
            let mut status = MaybeUninit::<libc::stat>::uninit();
            // SAFETY: output points to enough aligned space and is read only
            // after fstat succeeded. No pathname is inspected.
            if unsafe { libc::fstat(fd, status.as_mut_ptr()) } != 0 {
                return Err(ScriptWorkerError::Isolation);
            }
            let status = unsafe { status.assume_init() };
            let kind = status.st_mode & libc::S_IFMT;
            if (fd < 2 && kind != libc::S_IFIFO)
                || (fd == 2 && (kind != libc::S_IFCHR || status.st_rdev != libc::makedev(1, 3)))
            {
                return Err(ScriptWorkerError::Isolation);
            }
            // SAFETY: F_GETFL takes no extra argument and only queries this fd.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0
                || (fd == 0 && flags & libc::O_ACCMODE != libc::O_RDONLY)
                || (fd == 1 && flags & libc::O_ACCMODE != libc::O_WRONLY)
            {
                return Err(ScriptWorkerError::Isolation);
            }
        }
        Ok(())
    }
    pub(super) fn install() -> Result<(), ScriptWorkerError> {
        // Repeat after trusted thread/input setup, before initializing JS.
        close_extra_descriptors()?;
        // SAFETY: getpid has no caller-owned pointers or external effects.
        let mut program = filter(unsafe { libc::getpid() } as u32)?;
        let descriptor = sock_fprog {
            len: u16::try_from(program.len()).map_err(|_| ScriptWorkerError::Isolation)?,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: descriptor and instructions remain alive through this call;
        // the kernel copies them. NNP was set before creating worker threads.
        // TSYNC returning a positive tid is a failure, not successful install.
        if unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER as libc::c_ulong,
                libc::SECCOMP_FILTER_FLAG_TSYNC as libc::c_ulong,
                &descriptor as *const sock_fprog,
            )
        } != 0
        {
            return Err(ScriptWorkerError::Isolation);
        }
        Ok(())
    }
}
pub(super) fn prepare() -> Result<(), ScriptWorkerError> {
    #[cfg(all(
        target_pointer_width = "64",
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        native::close_extra_descriptors()
    }
    #[cfg(not(all(
        target_pointer_width = "64",
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        Err(ScriptWorkerError::Isolation)
    }
}
pub(super) fn install() -> Result<(), ScriptWorkerError> {
    #[cfg(all(
        target_pointer_width = "64",
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        native::install()
    }
    #[cfg(not(all(
        target_pointer_width = "64",
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        Err(ScriptWorkerError::Isolation)
    }
}
