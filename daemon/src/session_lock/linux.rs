//! Readonly logind adapter; no shell commands, session mutations or user-bus access.
#![allow(unsafe_code)]

use crate::vault_control::VaultController;
use cshell_ipc::VaultSessionState as State;
use futures::{
    StreamExt,
    stream::{LocalBoxStream, select_all},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zbus::{
    Connection, Proxy,
    proxy::CacheProperties,
    zvariant::{OwnedObjectPath, OwnedValue},
};

const SERVICE: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const READ_TIMEOUT: Duration = Duration::from_millis(1_500);
type ReadResult<T> = Result<T, ()>;
type Properties = HashMap<String, OwnedValue>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Binding {
    path: String,
    id: String,
    uid: u32,
    created: u64,
}
enum Event {
    Lock,
    Locked,
    Refresh,
    Poll,
    Sleep(bool),
    Shutdown(bool),
    Failed,
}
#[derive(Default)]
struct Restrictions {
    lock_requested: bool,
    locked_observed: bool,
}

pub(super) fn run(vault: Weak<VaultController>, stopped: Arc<AtomicBool>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let mut binding = None;
        let mut restrictions = Restrictions::default();
        while !stopped.load(Ordering::Acquire) && vault.strong_count() > 0 {
            tokio::select! {
                biased;
                _ = stop_requested(&stopped) => break,
                _ = attempt(&vault, &mut binding, &mut restrictions) => {}
            }
            observe(&vault, State::Unavailable);
            tokio::select! {
                _ = stop_requested(&stopped) => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    });
}
async fn stop_requested(stopped: &AtomicBool) {
    while !stopped.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
fn observe(vault: &Weak<VaultController>, state: State) {
    if let Some(vault) = vault.upgrade() {
        vault.observe_desktop_session(state);
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = ReadResult<T>>) -> ReadResult<T> {
    tokio::time::timeout(READ_TIMEOUT, future)
        .await
        .map_err(|_| ())?
}
async fn proxy<'a>(
    connection: &Connection,
    destination: &'a str,
    path: &'a str,
    interface: &'a str,
) -> ReadResult<Proxy<'a>> {
    zbus::proxy::Builder::<Proxy<'a>>::new(connection)
        .destination(destination)
        .map_err(|_| ())?
        .path(path)
        .map_err(|_| ())?
        .interface(interface)
        .map_err(|_| ())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|_| ())
}
async fn all(properties: &Proxy<'_>, interface: &str) -> ReadResult<Properties> {
    let values: Properties = properties
        .call("GetAll", &(interface,))
        .await
        .map_err(|_| ())?;
    if values.len() > 128 || values.keys().any(|key| key.len() > 128) {
        return Err(());
    }
    Ok(values)
}
fn take<T: TryFrom<OwnedValue>>(properties: &mut Properties, name: &str) -> ReadResult<T> {
    properties
        .remove(name)
        .ok_or(())?
        .try_into()
        .map_err(|_| ())
}
fn session_snapshot(mut fields: Properties, path: &str, uid: u32) -> ReadResult<(Binding, State)> {
    let (owner, _): (u32, OwnedObjectPath) = take(&mut fields, "User")?;
    let id: String = take(&mut fields, "Id")?;
    let created: u64 = take(&mut fields, "TimestampMonotonic")?;
    let active: bool = take(&mut fields, "Active")?;
    let locked: bool = take(&mut fields, "LockedHint")?;
    let remote: bool = take(&mut fields, "Remote")?;
    let kind: String = take(&mut fields, "Type")?;
    let class: String = take(&mut fields, "Class")?;
    let state: String = take(&mut fields, "State")?;
    if owner != uid
        || id.is_empty()
        || id.len() > 128
        || created == 0
        || path.len() > 512
        || !path.starts_with("/org/freedesktop/login1/session/")
        || !matches!(state.as_str(), "active" | "online" | "closing")
    {
        return Err(());
    }
    let current = if remote
        || class != "user"
        || !matches!(kind.as_str(), "x11" | "wayland")
        || !active
        || state != "active"
    {
        State::Disconnected
    } else if locked {
        State::Locked
    } else {
        State::Active
    };
    Ok((
        Binding {
            path: path.into(),
            id,
            uid,
            created,
        },
        current,
    ))
}
async fn resolve_session(
    connection: &Connection,
    manager: &Proxy<'_>,
    owner: &str,
    uid: u32,
) -> ReadResult<String> {
    let result: zbus::Result<OwnedObjectPath> = manager
        .call("GetSessionByPID", &(std::process::id(),))
        .await;
    match result {
        Ok(path) => return Ok(path.to_string()),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.login1.NoSessionForPID" => {}
        _ => return Err(()),
    }
    // A user-manager service may have no session cgroup. Select one eligible GUI
    // session, never a remote/TTY/greeter session or an environment hint.
    let rows: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &()).await.map_err(|_| ())?;
    if rows.len() > 128 {
        return Err(());
    }
    let mut selected = None;
    for (id, owner_uid, _, _, path) in rows {
        if owner_uid != uid {
            continue;
        }
        let fields = proxy(connection, owner, path.as_str(), PROPERTIES).await?;
        let (binding, state) = session_snapshot(all(&fields, SESSION).await?, path.as_str(), uid)?;
        if binding.id != id {
            return Err(());
        }
        if matches!(state, State::Active | State::Locked) {
            if selected.is_some() {
                return Err(());
            }
            selected = Some(path.to_string());
        }
    }
    selected.ok_or(())
}
async fn snapshot(
    bus: &Proxy<'_>,
    owner: &str,
    manager: &Proxy<'_>,
    session: &Proxy<'_>,
    expected: &Binding,
) -> ReadResult<State> {
    let actual_owner: String = bus
        .call("GetNameOwner", &(SERVICE,))
        .await
        .map_err(|_| ())?;
    if actual_owner != owner {
        return Err(());
    }
    let mut manager_fields = all(manager, MANAGER).await?;
    let sleeping: bool = take(&mut manager_fields, "PreparingForSleep")?;
    let shutdown: bool = take(&mut manager_fields, "PreparingForShutdown")?;
    let (actual, state) =
        session_snapshot(all(session, SESSION).await?, &expected.path, expected.uid)?;
    if actual != *expected {
        return Err(());
    }
    if shutdown {
        Ok(State::Disconnected)
    } else if sleeping {
        Ok(State::Suspended)
    } else {
        Ok(state)
    }
}
async fn attempt(
    vault: &Weak<VaultController>,
    frozen: &mut Option<Binding>,
    restrictions: &mut Restrictions,
) -> ReadResult<()> {
    // Fixed root-managed system bus endpoint; DBUS_SYSTEM_BUS_ADDRESS cannot
    // redirect credential authorization to an arbitrary user-controlled bus.
    let connection = bounded(async {
        zbus::connection::Builder::address("unix:path=/run/dbus/system_bus_socket")
            .map_err(|_| ())?
            .max_queued(16)
            .build()
            .await
            .map_err(|_| ())
    })
    .await?;
    let bus = proxy(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let owner: String =
        bounded(async { bus.call("GetNameOwner", &(SERVICE,)).await.map_err(|_| ()) }).await?;
    if !owner.starts_with(':') || owner.len() > 128 {
        return Err(());
    }
    let owner_uid: u32 = bounded(async {
        bus.call("GetConnectionUnixUser", &(owner.as_str(),))
            .await
            .map_err(|_| ())
    })
    .await?;
    if owner_uid != 0 {
        return Err(());
    }
    let manager = proxy(&connection, &owner, MANAGER_PATH, MANAGER).await?;
    // SAFETY: geteuid has no pointers or side effects and returns this process's UID.
    let uid = unsafe { libc::geteuid() };
    let path = match frozen.as_ref() {
        Some(binding) => binding.path.clone(),
        None => bounded(resolve_session(&connection, &manager, &owner, uid)).await?,
    };
    let session = proxy(&connection, &owner, &path, SESSION).await?;
    let session_properties = proxy(&connection, &owner, &path, PROPERTIES).await?;
    let manager_properties = proxy(&connection, &owner, MANAGER_PATH, PROPERTIES).await?;
    let subscriptions = bounded(async {
        let lock = session.receive_signal("Lock").await.map_err(|_| ())?;
        let unlock = session.receive_signal("Unlock").await.map_err(|_| ())?;
        let changed = session_properties
            .receive_signal("PropertiesChanged")
            .await
            .map_err(|_| ())?;
        let sleep = manager
            .receive_signal("PrepareForSleep")
            .await
            .map_err(|_| ())?;
        let shutdown = manager
            .receive_signal("PrepareForShutdown")
            .await
            .map_err(|_| ())?;
        let owner_changed = bus
            .receive_signal_with_args("NameOwnerChanged", &[(0, SERVICE)])
            .await
            .map_err(|_| ())?;
        let streams: Vec<LocalBoxStream<'_, Event>> = vec![
            lock.map(|message| {
                if message.body().deserialize::<()>().is_ok() {
                    Event::Lock
                } else {
                    Event::Failed
                }
            })
            .chain(futures::stream::once(async { Event::Failed }))
            .boxed_local(),
            unlock
                .map(|message| {
                    if message.body().deserialize::<()>().is_ok() {
                        Event::Refresh
                    } else {
                        Event::Failed
                    }
                })
                .chain(futures::stream::once(async { Event::Failed }))
                .boxed_local(),
            changed
                .map(|message| properties_event(message.body().deserialize()))
                .chain(futures::stream::once(async { Event::Failed }))
                .boxed_local(),
            sleep
                .map(|message| match message.body().deserialize::<bool>() {
                    Ok(value) => Event::Sleep(value),
                    Err(_) => Event::Failed,
                })
                .chain(futures::stream::once(async { Event::Failed }))
                .boxed_local(),
            shutdown
                .map(|message| match message.body().deserialize::<bool>() {
                    Ok(value) => Event::Shutdown(value),
                    Err(_) => Event::Failed,
                })
                .chain(futures::stream::once(async { Event::Failed }))
                .boxed_local(),
            owner_changed
                .map(|_| Event::Failed)
                .chain(futures::stream::once(async { Event::Failed }))
                .boxed_local(),
        ];
        Ok(select_all(streams))
    })
    .await?;
    let (binding, _) =
        bounded(async { session_snapshot(all(&session_properties, SESSION).await?, &path, uid) })
            .await?;
    if frozen.as_ref().is_some_and(|old| *old != binding) {
        return Err(());
    }
    *frozen = Some(binding.clone());
    let mut events = subscriptions;
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut refresh = true;
    let mut sleeping = false;
    let mut shutting_down = false;
    let mut last_tick = boot_time_ns()?;
    loop {
        if vault.strong_count() == 0 {
            return Ok(());
        }
        // Signals remain polled while a property read is pending. A restriction
        // cancels that read; it cannot publish an old Active reply afterwards.
        let should_refresh = refresh;
        let query = async {
            if should_refresh {
                bounded(snapshot(
                    &bus,
                    &owner,
                    &manager_properties,
                    &session_properties,
                    &binding,
                ))
                .await
            } else {
                std::future::pending().await
            }
        };
        tokio::select! {
            biased;
            event = events.next() => match event.ok_or(())? {
                Event::Failed => return Err(()),
                Event::Lock => { restrictions.lock_requested = true; observe(vault, State::Locked); refresh = true; }
                Event::Locked => { restrictions.lock_requested = true; restrictions.locked_observed = true; observe(vault, State::Locked); refresh = true; }
                Event::Refresh => { observe(vault, State::Unavailable); refresh = true; }
                Event::Sleep(value) => { sleeping = value; observe(vault, if value { State::Suspended } else { State::Unavailable }); refresh = true; }
                Event::Shutdown(value) => { shutting_down = value; observe(vault, State::Disconnected); refresh = true; }
                Event::Poll => { refresh = true; }
            },
            result = query => {
                let state = result?;
                let now = boot_time_ns()?;
                if now.checked_sub(last_tick).is_none_or(|gap| gap >= 3_000_000_000) {
                    observe(vault, State::Unavailable);
                }
                last_tick = now;
                // A missing resume signal reconnects for a fresh subscription and
                // snapshot. Availability never clears the previously locked Vault.
                if sleeping && state != State::Suspended { return Err(()); }
                if shutting_down && state != State::Disconnected { return Err(()); }
                if state == State::Locked { restrictions.locked_observed = true; }
                let state = if state == State::Active && restrictions.lock_requested {
                    if restrictions.locked_observed {
                        restrictions.lock_requested = false;
                        restrictions.locked_observed = false;
                        state
                    } else { State::Unavailable }
                } else { state };
                if state == State::Active { restrictions.locked_observed = false; }
                observe(vault, state);
                refresh = false;
            },
            _ = poll.tick(), if !should_refresh => { refresh = true; }
        }
    }
}
fn properties_event(result: zbus::Result<(String, Properties, Vec<String>)>) -> Event {
    let Ok((interface, changed, invalidated)) = result else {
        return Event::Failed;
    };
    if interface != SESSION || changed.len() > 128 || invalidated.len() > 128 {
        return Event::Failed;
    }
    let security = [
        "Active",
        "State",
        "LockedHint",
        "User",
        "Id",
        "TimestampMonotonic",
        "Remote",
        "Type",
        "Class",
    ];
    if changed
        .get("LockedHint")
        .and_then(|value| bool::try_from(value).ok())
        == Some(true)
    {
        Event::Locked
    } else if security.iter().any(|key| {
        changed.contains_key(*key) || invalidated.iter().any(|value| value.as_str() == *key)
    }) {
        Event::Refresh
    } else {
        Event::Poll
    }
}
fn boot_time_ns() -> ReadResult<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one valid timespec; CLOCK_BOOTTIME includes suspend.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(());
    }
    let seconds = u64::try_from(value.tv_sec).map_err(|_| ())?;
    let nanos = u64::try_from(value.tv_nsec).map_err(|_| ())?;
    if nanos >= 1_000_000_000 {
        return Err(());
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or(())
}
