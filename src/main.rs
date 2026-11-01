use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::{Command, Stdio},
};

use futures::{
    StreamExt as _,
    select,
    stream::{FuturesUnordered, select_all},
};
use zbus::{
    Connection,
    OwnedObjectPath,
    OwnedVariant,
    Variant,
    proxy,
};

const PREFIX: &str = "/org/freedesktop/UDisks2/block_devices/";
const FS_IFACE: &str = "org.freedesktop.UDisks2.Filesystem";
const MOUNT_TIMEOUT_MS: i32 = 30_000;
const REMOVED_TIMEOUT_MS: i32 = 6_000;

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;

/// Why a device was skipped during mount (debug only).
type SkipReason = &'static str;

/// A mounted device we are tracking.
#[derive(Clone, Debug)]
struct MountedDevice {
    object_path: OwnedObjectPath,
    mount_point: String,
    notification_id: u32,
}

/// What the main loop woke up for.
enum Step {
    Event(Option<Ev>),
    MountDone(OwnedObjectPath, zbus::Result<MountResult>),
}

/// Result of a mount attempt.
#[derive(Debug)]
enum MountResult {
    /// We mounted it, at this path.
    Mounted(String),
    /// Someone else already mounted it. We track it silently so we can still
    /// report its removal.
    AlreadyMounted(String),
    /// Deliberately skipped with a reason.
    Skipped(SkipReason),
}

/// Events from all three D-Bus sources, merged into one stream.
enum Ev {
    Added(OwnedObjectPath),
    Removed(OwnedObjectPath),
    Clicked(u32),
}

/// Log only when UDISKR_DEBUG is set. Does not allocate when disabled.
macro_rules! debug {
    ($($arg:tt)*) => {
        if std::env::var_os("UDISKR_DEBUG").is_some() {
            eprintln!("udiskr: {}", format_args!($($arg)*));
        }
    };
}

/// True when running under a Wayland session.
fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

/// Open the mount point with `xdg-open`, falling back to `gio open`
/// (order flipped on Wayland).
///
/// Runs in a helper thread: the thread waits for the opener, so no zombies pile
/// up, and the fallback also triggers when the first opener exists but fails
/// (not only when its binary is missing).
fn open_file_manager(dir: String) {
    std::thread::spawn(move || {
        let mut openers = [("xdg-open", None), ("gio", Some("open"))];
        if is_wayland() {
            openers.reverse();
        }
        for (bin, sub) in openers {
            let status = Command::new(bin)
                .args(sub)
                .arg(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match status {
                Ok(s) if s.success() => return,
                Ok(s) => debug!("{bin} exited with {s}"),
                Err(e) => debug!("{bin}: {e}"),
            }
        }
        eprintln!("udiskr: could not open {dir}");
    });
}

/// Send/replace a notification.
/// `replaces` must be 0 (new notification) or an id previously returned by the server.
/// Returns the server-assigned id, or 0 if the call failed.
async fn send_notification(
    n: &NotificationsProxy<'_>,
    summary: &str,
    body: &str,
    actions: &[&str],
    replaces: u32,
    timeout: i32,
) -> u32 {
    n.notify(
        "udiskr",
        replaces,
        "",
        summary,
        body,
        actions,
        &HashMap::new(),
        timeout,
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("udiskr: notify: {e}");
        0
    })
}

fn main() {
    // No timers are used, so only the I/O driver is needed.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("failed to build runtime");
    // Exit non-zero on any fatal error so a supervisor (systemd etc.) can restart us.
    if let Err(e) = rt.block_on(run()) {
        eprintln!("udiskr: {e}");
        std::process::exit(1);
    }
}

async fn run() -> zbus::Result<()> {
    let system = Connection::system().await?;
    PeerProxy::new(
        &system,
        "org.freedesktop.UDisks2",
        "/org/freedesktop/UDisks2/Manager",
    )
    .await?
    .ping()
    .await?;

    let manager = ManagerProxy::new(&system).await?;

    let session = Connection::session().await?;
    let notifier = NotificationsProxy::new(&session).await?;

    // Subscribe BEFORE scanning existing devices so nothing can slip through the gap.
    let added = manager
        .receive_interfaces_added()
        .await?
        .filter_map(|s| async move {
            let a = s.args().ok()?;
            (a.path.as_str().starts_with(PREFIX)
                && a.interfaces_and_properties.contains_key(FS_IFACE))
            .then(|| Ev::Added(a.path.clone()))
        })
        .boxed_local();
    let removed = manager
        .receive_interfaces_removed()
        .await?
        .filter_map(|s| async move {
            let a = s.args().ok()?;
            (a.path.as_str().starts_with(PREFIX)
                && a.interfaces.iter().any(|i| i.as_str() == FS_IFACE))
            .then(|| Ev::Removed(a.path.clone()))
        })
        .boxed_local();
    let clicked = notifier
        .receive_action_invoked()
        .await?
        .filter_map(|s| async move { s.args().ok().map(|a| Ev::Clicked(a.id)) })
        .boxed_local();
    let mut events = select_all([added, removed, clicked]);

    let mut mounted: HashMap<OwnedObjectPath, MountedDevice> = HashMap::new();
    let mut next_notify_id: u32 = 1;
    // Mounts currently in flight.
    let mut busy: HashSet<OwnedObjectPath> = HashSet::new();
    // In-flight mounts whose device was removed meanwhile: their result is discarded.
    let mut gone: HashSet<OwnedObjectPath> = HashSet::new();
    let mut pending = FuturesUnordered::new();

    // Startup scan: queue devices that were already plugged in.
    match manager.get_managed_objects().await {
        Ok(objects) => {
            for (path, ifaces) in objects {
                if path.as_str().starts_with(PREFIX)
                    && ifaces.contains_key(FS_IFACE)
                    && busy.insert(path.clone())
                {
                    pending.push(try_mount(system.clone(), path));
                }
            }
        }
        Err(e) => eprintln!("udiskr: initial device scan failed: {e}"),
    }

    loop {
        let step = if pending.is_empty() {
            Step::Event(events.next().await)
        } else {
            select! {
                ev = events.next() => Step::Event(ev),
                (path, res) = pending.select_next_some() => Step::MountDone(path, res),
            }
        };
        match step {
            Step::Event(None) => break,
            Step::Event(Some(Ev::Added(path))) => {
                if mounted.contains_key(&path) {
                    continue;
                }
                if busy.contains(&path) {
                    // Removed and re-added while a mount was in flight:
                    // the device is back, so keep the in-flight result.
                    gone.remove(&path);
                } else {
                    busy.insert(path.clone());
                    pending.push(try_mount(system.clone(), path));
                }
            }
            Step::Event(Some(Ev::Removed(path))) => {
                if busy.contains(&path) {
                    gone.insert(path.clone());
                }
                if let Some(device) = mounted.remove(&path) {
                    let msg = format!(
                        "/dev/{} unmounted from {}",
                        dev(&device.object_path),
                        device.mount_point
                    );
                    eprintln!("{msg}");
                    // Reuse the server-assigned id so this REPLACES the "mounted"
                    // notification (and drops its actions). With id 0 a new
                    // notification is created instead. Pass 0 here to keep both.
                    let replaces = if device.notification_id != 0 {
                        device.notification_id
                    } else {
                        0
                    };
                    let _ = send_notification(
                        &notifier,
                        "block device unmounted",
                        &msg,
                        &[],
                        replaces,
                        REMOVED_TIMEOUT_MS,
                    )
                    .await;
                }
            }
            Step::Event(Some(Ev::Clicked(id))) => {
                debug!("clicked notification {id}");
                if id != 0 {
                    if let Some((_, device)) = mounted.iter().find(|(_, d)| d.notification_id == id) {
                        if Path::new(&device.mount_point).is_dir() {
                            open_file_manager(device.mount_point.clone());
                        } else {
                            debug!("mount point {} no longer exists", device.mount_point);
                        }
                    } else {
                        debug!("no mounted device for notification id {id}");
                    }
                }
            }
            Step::MountDone(path, res) => {
                busy.remove(&path);
                if gone.remove(&path) {
                    debug!("{} vanished while mounting", dev(&path));
                    continue;
                }
                match res {
                    Ok(MountResult::Mounted(mount_point)) => {
                        let msg = format!("Mounted /dev/{} at {mount_point}", dev(&path));
                        eprintln!("{msg}");
                        // Action keys come in (key, label) pairs. "default" is the
                        // body click on most servers; the second pair gives a button
                        // on servers that don't render "default".
                        let id = send_notification(
                            &notifier,
                            "block device mounted",
                            &msg,
                            &["default", "Open", "open", "Open"],
                            next_notify_id,
                            MOUNT_TIMEOUT_MS,
                        )
                        .await;
                        next_notify_id = next_notify_id.wrapping_add(1);
                        mounted.insert(
                            path.clone(),
                            MountedDevice {
                                object_path: path,
                                mount_point,
                                notification_id: id,
                            },
                        );
                    }
                    Ok(MountResult::AlreadyMounted(mount_point)) => {
                        debug!("{} already mounted at {}", dev(&path), mount_point);
                        mounted.insert(
                            path.clone(),
                            MountedDevice {
                                object_path: path,
                                mount_point,
                                notification_id: 0,
                            },
                        );
                    }
                    Ok(MountResult::Skipped(reason)) => {
                        debug!("skipped {}: {reason}", dev(&path));
                    }
                    // Ordinary D-Bus noise (e.g. device not ready), or someone else
                    // mounted it first: ignore.
                    Err(zbus::Error::MethodError(name, _, _))
                        if name.starts_with("org.freedesktop.DBus")
                            || name.starts_with("org.freedesktop.UDisks2.Error.AlreadyMounted") =>
                    {
                        debug!("mount {}: {name}", dev(&path));
                    }
                    Err(e) => eprintln!("udiskr: mount {}: {e}", dev(&path)),
                }
            }
        }
    }
    Err(zbus::Error::Failure("D-Bus event stream ended".into()))
}

/// Mount one device. Returns `Ok(MountResult::Skipped(reason))` for deliberate skips.
async fn try_mount(
    system: Connection,
    object_path: OwnedObjectPath,
) -> (OwnedObjectPath, zbus::Result<MountResult>) {
    let res = async {
        let block = BlockProxy::new(&system, &object_path).await?;
        if block.hint_ignore().await? {
            return Ok(MountResult::Skipped("ignored"));
        }
        if block.hint_system().await? {
            return Ok(MountResult::Skipped("system"));
        }
        let fs = FilesystemProxy::new(&system, &object_path).await?;
        let points = fs.mount_points().await?;
        if let Some(mp) = first_mount_point(&points) {
            return Ok(MountResult::AlreadyMounted(mp));
        }
        fs.mount(&HashMap::new()).await.map(MountResult::Mounted)
    }
    .await;
    (object_path, res)
}

/// UDisks2 mount points are NUL-terminated byte strings (`aay`).
fn first_mount_point(points: &[Vec<u8>]) -> Option<String> {
    let bytes = points.first()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// "/org/.../block_devices/sdb1" -> "sdb1" (never panics).
fn dev(object_path: &OwnedObjectPath) -> &str {
    object_path.as_str().strip_prefix(PREFIX).unwrap_or(object_path.as_str())
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    default_path = "/org/freedesktop/UDisks2",
    interface = "org.freedesktop.DBus.ObjectManager"
)]
trait Manager {
    fn get_managed_objects(&self) -> zbus::Result<Objects>;
    #[zbus(signal)]
    fn interfaces_added(
        &self,
        path: OwnedObjectPath,
        interfaces_and_properties: HashMap<String, HashMap<String, OwnedVariant>>,
    );
    #[zbus(signal)]
    fn interfaces_removed(&self, path: OwnedObjectPath, interfaces: Vec<String>);
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    interface = "org.freedesktop.UDisks2.Block"
)]
trait Block {
    #[zbus(property)]
    fn hint_system(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn hint_ignore(&self) -> zbus::Result<bool>;
}

#[proxy(
    default_service = "org.freedesktop.UDisks2",
    interface = "org.freedesktop.UDisks2.Filesystem"
)]
trait Filesystem {
    fn mount(&self, options: &HashMap<&str, Variant<'_>>) -> zbus::Result<String>;
    #[zbus(property)]
    fn mount_points(&self) -> zbus::Result<Vec<Vec<u8>>>;
}

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: &HashMap<&str, Variant<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str);
}

#[proxy(interface = "org.freedesktop.DBus.Peer")]
trait Peer {
    fn ping(&self) -> zbus::Result<()>;
}
