//! Readonly console snapshot and public IOKit power notifications.
//! Console keys are a checked compatibility schema, not a stable Apple API.
#![allow(unsafe_code)]

use crate::vault_control::VaultController;
use cshell_ipc::VaultSessionState as State;
use std::{
    cell::Cell,
    ffi::{CStr, c_char, c_void},
    ptr,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

type Cf = *const c_void;
type ReadResult<T> = Result<T, ()>;
const UTF8: u32 = 0x0800_0100;
const SINT64: isize = 4;
const CAN_SLEEP: u32 = 0xe000_0270;
const WILL_SLEEP: u32 = 0xe000_0280;
const WILL_NOT_SLEEP: u32 = 0xe000_0290;
const HAS_POWERED_ON: u32 = 0xe000_0300;
const WILL_POWER_ON: u32 = 0xe000_0320;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: Cf);
    fn CFGetTypeID(object: Cf) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: Cf) -> isize;
    fn CFArrayGetValueAtIndex(array: Cf, index: isize) -> Cf;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: Cf, key: Cf) -> Cf;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Cf) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(value: Cf, kind: isize, output: *mut c_void) -> u8;
    fn CFStringCreateWithCString(allocator: Cf, text: *const c_char, encoding: u32) -> Cf;
    fn CFRunLoopGetCurrent() -> Cf;
    fn CFRunLoopAddSource(run_loop: Cf, source: Cf, mode: Cf);
    fn CFRunLoopRemoveSource(run_loop: Cf, source: Cf, mode: Cf);
    fn CFRunLoopRunInMode(mode: Cf, seconds: f64, return_after_source: u8) -> i32;
    static kCFRunLoopDefaultMode: Cf;
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryGetRootEntry(main_port: u32) -> u32;
    fn IOServiceMatching(name: *const c_char) -> Cf;
    fn IOServiceGetMatchingService(main_port: u32, matching: Cf) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: Cf, allocator: Cf, options: u32) -> Cf;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegisterForSystemPower(
        context: *mut c_void,
        port: *mut *mut c_void,
        callback: Option<unsafe extern "C" fn(*mut c_void, u32, u32, *mut c_void)>,
        notifier: *mut u32,
    ) -> u32;
    fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> Cf;
    fn IONotificationPortDestroy(port: *mut c_void);
    fn IOServiceClose(connection: u32) -> i32;
    fn IOAllowPowerChange(connection: u32, notification: isize) -> i32;
}
#[repr(C)]
#[derive(Default)]
struct Timebase {
    numerator: u32,
    denominator: u32,
}
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}
struct CfOwned(Cf);
impl CfOwned {
    fn new(value: Cf) -> ReadResult<Self> {
        if value.is_null() {
            Err(())
        } else {
            Ok(Self(value))
        }
    }
    fn key(text: &CStr) -> ReadResult<Self> {
        // SAFETY: Constant C strings are nul-terminated and the returned create
        // reference belongs to this guard, exactly once.
        Self::new(unsafe { CFStringCreateWithCString(ptr::null(), text.as_ptr(), UTF8) })
    }
}
impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: Only owned Create/Copy references enter this guard.
        unsafe {
            CFRelease(self.0);
        }
    }
}
struct IoObject(u32);
impl IoObject {
    fn new(value: u32) -> ReadResult<Self> {
        if value == 0 { Err(()) } else { Ok(Self(value)) }
    }
    fn property(&self, name: &CStr) -> ReadResult<CfOwned> {
        let key = CfOwned::key(name)?;
        // SAFETY: Both the IO handle and CF key are live; Create returns ownership.
        CfOwned::new(unsafe { IORegistryEntryCreateCFProperty(self.0, key.0, ptr::null(), 0) })
    }
}
impl Drop for IoObject {
    fn drop(&mut self) {
        // SAFETY: Root/MatchingService returns one owned IOKit object reference.
        unsafe {
            IOObjectRelease(self.0);
        }
    }
}
// Borrowed values never outlive their owning CFArray/CFDictionary snapshot.
fn field(dictionary: Cf, key: &CStr) -> ReadResult<Cf> {
    // SAFETY: Validate the parent before treating it as a dictionary.
    if dictionary.is_null() || unsafe { CFGetTypeID(dictionary) != CFDictionaryGetTypeID() } {
        return Err(());
    }
    let key = CfOwned::key(key)?;
    // SAFETY: The dictionary and key remain alive for the entire get operation.
    let value = unsafe { CFDictionaryGetValue(dictionary, key.0) };
    if value.is_null() { Err(()) } else { Ok(value) }
}
fn boolean(value: Cf) -> ReadResult<bool> {
    // SAFETY: Check type before using the CFBoolean API.
    if value.is_null() || unsafe { CFGetTypeID(value) != CFBooleanGetTypeID() } {
        return Err(());
    }
    Ok(unsafe { CFBooleanGetValue(value) } != 0)
}
fn number(value: Cf) -> ReadResult<u64> {
    // SAFETY: Check type, then request a lossless signed 64-bit conversion into a
    // correctly aligned stack variable. Reject negative or out-of-range values.
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return Err(());
    }
    let mut output = 0_i64;
    if unsafe { CFNumberGetValue(value, SINT64, ptr::from_mut(&mut output).cast()) } == 0 {
        return Err(());
    }
    u64::try_from(output).map_err(|_| ())
}
struct ConsoleProbe {
    root: IoObject,
    resources: IoObject,
    uid: u32,
}
impl ConsoleProbe {
    fn new() -> ReadResult<Self> {
        // SAFETY: A null main port selects the system default. The matching
        // dictionary is consumed by IOServiceGetMatchingService even on failure.
        let root = IoObject::new(unsafe { IORegistryGetRootEntry(0) })?;
        let matching = unsafe { IOServiceMatching(c"IOResources".as_ptr()) };
        if matching.is_null() {
            return Err(());
        }
        let resources = IoObject::new(unsafe { IOServiceGetMatchingService(0, matching) })?;
        // SAFETY: geteuid reads this process's own effective identity.
        Ok(Self {
            root,
            resources,
            uid: unsafe { libc::geteuid() },
        })
    }
    fn seed(&self) -> ReadResult<u64> {
        number(self.resources.property(c"IOConsoleUsersSeed")?.0)
    }
    fn snapshot(&self) -> ReadResult<(u64, State)> {
        let before = self.seed()?;
        let users = self.root.property(c"IOConsoleUsers")?;
        // SAFETY: Reject mismatched type before array access; indices are bounded.
        if unsafe { CFGetTypeID(users.0) != CFArrayGetTypeID() } {
            return Err(());
        }
        let count = unsafe { CFArrayGetCount(users.0) };
        if !(0..=64).contains(&count) {
            return Err(());
        }
        let mut selected = None;
        let mut console_count = 0;
        for index in 0..count {
            // SAFETY: index lies within the validated array bounds; the array
            // owns the returned dictionary until this snapshot is released.
            let user = unsafe { CFArrayGetValueAtIndex(users.0, index) };
            if !boolean(field(user, c"kCGSSessionOnConsoleKey")?)? {
                continue;
            }
            console_count += 1;
            let uid = number(field(user, c"kCGSSessionUserIDKey")?)?;
            if uid != u64::from(self.uid) {
                continue;
            }
            let login_done = boolean(field(user, c"kCGSessionLoginDoneKey")?)?;
            let locked = boolean(field(user, c"CGSSessionScreenIsLocked")?)?;
            if selected.is_some() {
                return Err(());
            }
            selected = Some(if !login_done {
                State::Disconnected
            } else if locked {
                State::Locked
            } else {
                State::Active
            });
        }
        let after = self.seed()?;
        if before != after || console_count > 1 {
            return Err(());
        }
        Ok((after, selected.unwrap_or(State::Disconnected)))
    }
}
struct Context {
    vault: Weak<VaultController>,
    connection: Cell<u32>,
    suspended: Cell<bool>,
    failed: Cell<bool>,
}
impl Context {
    fn observe(&self, state: State) {
        if let Some(vault) = self.vault.upgrade() {
            vault.observe_desktop_session(state);
        }
    }
}
struct PowerMonitor {
    connection: u32,
    notifier: u32,
    port: *mut c_void,
    source: Cf,
    run_loop: Cf,
    context: Box<Context>,
}
impl PowerMonitor {
    fn new(vault: Weak<VaultController>) -> ReadResult<Self> {
        // Box allocation stays stable across moving the RAII wrapper. Callbacks
        // run only on this thread's CFRunLoop after its source is added below.
        let context = Box::new(Context {
            vault,
            connection: Cell::new(0),
            suspended: Cell::new(false),
            failed: Cell::new(false),
        });
        let mut result = Self {
            connection: 0,
            notifier: 0,
            port: ptr::null_mut(),
            source: ptr::null(),
            run_loop: ptr::null(),
            context,
        };
        // SAFETY: Both outputs are live fields. The stable boxed refcon is not
        // released until the source and notification registration are retired.
        result.connection = unsafe {
            IORegisterForSystemPower(
                ptr::from_ref(result.context.as_ref()).cast_mut().cast(),
                &mut result.port,
                Some(power_callback),
                &mut result.notifier,
            )
        };
        if result.connection == 0 || result.port.is_null() || result.notifier == 0 {
            return Err(());
        }
        result.context.connection.set(result.connection);
        // SAFETY: The notification port owns its source; the current run loop is
        // borrowed for this owned thread's lifetime. Neither is CFReleased here.
        result.source = unsafe { IONotificationPortGetRunLoopSource(result.port) };
        result.run_loop = unsafe { CFRunLoopGetCurrent() };
        if result.source.is_null() || result.run_loop.is_null() {
            return Err(());
        }
        unsafe {
            CFRunLoopAddSource(result.run_loop, result.source, kCFRunLoopDefaultMode);
        }
        Ok(result)
    }
}
impl Drop for PowerMonitor {
    fn drop(&mut self) {
        self.context.observe(State::Unavailable);
        // SAFETY: Cleanup occurs on the owning thread, before dropping refcon.
        // Source is borrowed from the notification port and is never released twice.
        unsafe {
            if !self.source.is_null() && !self.run_loop.is_null() {
                CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopDefaultMode);
            }
            if self.notifier != 0 {
                IODeregisterForSystemPower(&mut self.notifier);
            }
            if !self.port.is_null() {
                IONotificationPortDestroy(self.port);
            }
            if self.connection != 0 {
                IOServiceClose(self.connection);
            }
        }
    }
}
unsafe extern "C" fn power_callback(
    pointer: *mut c_void,
    _service: u32,
    message: u32,
    argument: *mut c_void,
) {
    if pointer.is_null() {
        return;
    }
    // SAFETY: Refcon is the stable Context allocation owned by PowerMonitor.
    let context = unsafe { &*pointer.cast::<Context>() };
    match message {
        CAN_SLEEP | WILL_SLEEP => {
            if message == WILL_SLEEP {
                context.suspended.set(true);
            }
            // Acknowledge before acquiring any Vault lock. Never veto OS sleep
            // and never let KDF/file work delay the required power acknowledgement.
            if unsafe { IOAllowPowerChange(context.connection.get(), argument as isize) } != 0 {
                context.failed.set(true);
            }
            if message == WILL_SLEEP {
                context.observe(State::Suspended);
            }
        }
        WILL_POWER_ON => {
            context.suspended.set(true);
            context.observe(State::Suspended);
        }
        HAS_POWERED_ON => {
            context.suspended.set(false);
            context.observe(State::Unavailable);
        }
        WILL_NOT_SLEEP => {
            if context.suspended.replace(false) {
                context.observe(State::Unavailable);
            }
        }
        _ => {}
    }
}
fn continuous_ns(timebase: &Timebase) -> ReadResult<u64> {
    if timebase.denominator == 0 {
        return Err(());
    }
    // SAFETY: Reads the kernel's continuous tick counter, including sleep.
    let ticks = unsafe { mach_continuous_time() };
    let value =
        u128::from(ticks) * u128::from(timebase.numerator) / u128::from(timebase.denominator);
    u64::try_from(value).map_err(|_| ())
}
pub(super) fn run(vault: Weak<VaultController>, stopped: Arc<AtomicBool>) {
    while !stopped.load(Ordering::Acquire) && vault.strong_count() > 0 {
        let _result = attempt(vault.clone(), &stopped);
        if let Some(vault) = vault.upgrade() {
            vault.observe_desktop_session(State::Unavailable);
        }
        if !stopped.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_secs(2));
        }
    }
}
fn attempt(vault: Weak<VaultController>, stopped: &AtomicBool) -> ReadResult<()> {
    let probe = ConsoleProbe::new()?;
    let power = PowerMonitor::new(vault)?;
    let mut timebase = Timebase::default();
    // SAFETY: Writes one repr(C) mach_timebase_info_data_t stack value.
    if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.numerator == 0 {
        return Err(());
    }
    let mut last_time = continuous_ns(&timebase)?;
    let mut previous_seed = None;
    let mut next_poll = Instant::now();
    while !stopped.load(Ordering::Acquire) && power.context.vault.strong_count() > 0 {
        if power.context.failed.get() {
            return Err(());
        }
        let now = continuous_ns(&timebase)?;
        if now
            .checked_sub(last_time)
            .is_none_or(|gap| gap >= 3_000_000_000)
        {
            power.context.observe(State::Unavailable);
        }
        last_time = now;
        if power.context.suspended.get() {
            power.context.observe(State::Suspended);
        } else if Instant::now() >= next_poll {
            let (seed, state) = probe.snapshot()?;
            if previous_seed.is_some_and(|old| old != seed) {
                power.context.observe(State::Unavailable);
            }
            previous_seed = Some(seed);
            power.context.observe(state);
            next_poll = Instant::now() + Duration::from_millis(500);
        }
        // SAFETY: Only the owned power source executes callbacks. Short slices
        // permit shutdown without sending callbacks to another/reused thread.
        let outcome = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, 1) };
        if outcome == 1 {
            return Err(());
        } // finished: no registered source remains
    }
    Ok(())
}
