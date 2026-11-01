use futures::{
    StreamExt as _, select,
    stream::{FuturesUnordered, select_all},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    pin::Pin,
    process::{Command, Stdio},
    time::Duration,
};
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Variant, proxy};

const PREFIX: &str = "/org/freedesktop/UDisks2/block_devices/";
const FS_IFACE: &str = "org.freedesktop.UDisks2.Filesystem";
const MOUNT_TIMEOUT_MS: i32 = 30_000;
const REMOVED_TIMEOUT_MS: i32 = 6_000;
const MOUNT_RETRY_DELAY_MS: u64 = 5_000;
const MAX_MOUNT_RETRIES: u32 = 3;

type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;

/// Boxed future for a mount operation, so all mount futures share one concrete type.
type MountFuture =
    Pin<Box<dyn std::future::Future<Output = (OwnedObjectPath, Result<MountResult, zbus::Error>)>>>;

/// A mounted device we are tracking.
#[derive(Clone, Debug)]
struct MountedDevice {
    object_path: OwnedObjectPath,
    mount_point: String,
    notification_id: u32,
}

/// Tracks mount retry state for a device.
#[derive(Clone, Debug, Default)]
struct MountRetry {
    attempts: u32,
}

/// Result of mounting a device.
#[derive(Debug)]
enum MountResult {
    Mounted(String),
    AlreadyMounted(String),
    Skipped(&'static str),
}

/// Events from all three D-Bus sources, merged into one stream.
enum Ev {
    Added(OwnedObjectPath),
    Removed(OwnedObjectPath),
    Clicked(u32),
}

/// What the main loop woke up for.
enum Step {
    Event(Option<Ev>),
    MountDone(OwnedObjectPath, zbus::Result<MountResult>),
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
fn open_file_manager(dir: String) {
    let wayland = is_wayland();

    std::thread::spawn(move || {
        let openers: &[(&str, &[&str])] = if wayland {
            &[("gio", &["open"]), ("xdg-open", &[])]
        } else {
            &[("xdg-open", &[]), ("gio", &["open"])]
        };

        for (bin, args) in openers {
            match Command::new(bin)
                .args(*args)
                .arg(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(_) => return,
                Err(e) => debug!("{bin}: {e}"),
            }
        }

        eprintln!("udiskr: could not open {dir}");
    });
}

/// Send/replace a notification. Returns the server-assigned id, or 0 if it failed.
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

/// Send a "mounted" notification. Returns the server-assigned id.
async fn notify_mounted(
    n: &NotificationsProxy<'_>,
    path: &OwnedObjectPath,
    mount_point: &str,
) -> u32 {
    let msg = format!("Mounted /dev/{} at {mount_point}", dev(path));
    eprintln!("{msg}");
    send_notification(
        n,
        "block device mounted",
        &msg,
        &["default", "open"],
        0,
        MOUNT_TIMEOUT_MS,
    )
    .await
}

/// Send an "unmounted" notification. Returns the server-assigned id.
async fn notify_unmounted(n: &NotificationsProxy<'_>, device: &MountedDevice) -> u32 {
    let msg = format!(
        "/dev/{} unmounted from {}",
        dev(&device.object_path),
        device.mount_point
    );
    eprintln!("{msg}");
    let replaces = device.notification_id;
    send_notification(
        n,
        "block device unmounted",
        &msg,
        &[],
        replaces,
        REMOVED_TIMEOUT_MS,
    )
    .await
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build runtime");
    if let Err(e) = rt.block_on(run()) {
        eprintln!("udiskr: {e}");
        std::process::exit(1);
    }
}

async fn run() -> zbus::Result<()> {
    let system = Connection::system().await?;
    // Ping the udisksd manager to verify it's reachable. Non-fatal if it fails.
    let _ = (async {
        PeerProxy::new(
            &system,
            "org.freedesktop.UDisks2",
            "/org/freedesktop/UDisks2/Manager",
        )
        .await?
        .ping()
        .await
    })
    .await;

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
    let mut busy: HashSet<OwnedObjectPath> = HashSet::new();
    let mut gone: HashSet<OwnedObjectPath> = HashSet::new();
    let mut mount_retries: HashMap<OwnedObjectPath, MountRetry> = HashMap::new();
    let mut pending = FuturesUnordered::<MountFuture>::new();

    // Startup scan: queue devices that were already plugged in.
    match manager.get_managed_objects().await {
        Ok(objects) => {
            for (path, ifaces) in objects {
                if path.as_str().starts_with(PREFIX)
                    && ifaces.contains_key(FS_IFACE)
                    && busy.insert(path.clone())
                {
                    pending.push(Box::pin(try_mount(system.clone(), path)));
                }
            }
        }
        Err(e) => {
            eprintln!("udiskr: initial device scan failed: {e}, retrying in 5s...");
            tokio::time::sleep(Duration::from_secs(5)).await;
            match manager.get_managed_objects().await {
                Ok(objects) => {
                    for (path, ifaces) in objects {
                        if path.as_str().starts_with(PREFIX)
                            && ifaces.contains_key(FS_IFACE)
                            && busy.insert(path.clone())
                        {
                            pending.push(Box::pin(try_mount(system.clone(), path)));
                        }
                    }
                }
                Err(e2) => {
                    eprintln!("udiskr: initial device scan failed again: {e2}");
                    return Err(e2);
                }
            }
        }
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
                    gone.remove(&path);
                } else {
                    busy.insert(path.clone());
                    mount_retries.insert(path.clone(), MountRetry::default());
                    pending.push(Box::pin(try_mount(system.clone(), path)));
                }
            }
            Step::Event(Some(Ev::Removed(path))) => {
                if busy.contains(&path) {
                    gone.insert(path.clone());
                }
                if let Some(device) = mounted.remove(&path) {
                    let _ = notify_unmounted(&notifier, &device).await;
                }
                mount_retries.remove(&path);
            }
            Step::Event(Some(Ev::Clicked(id))) => {
                debug!("clicked notification {id}");
                if id != 0 {
                    let stale_path: Option<OwnedObjectPath> = mounted
                        .iter()
                        .find(|(_, d)| d.notification_id == id)
                        .map(|(path, _)| path.clone());
                    if let Some(path) = stale_path {
                        if let Some(device) = mounted.get(&path) {
                            if Path::new(&device.mount_point).is_dir() {
                                open_file_manager(device.mount_point.clone());
                            } else {
                                debug!("mount point {} no longer exists", device.mount_point);
                                mounted.remove(&path);
                            }
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
                    mount_retries.remove(&path);
                    continue;
                }
                match res {
                    Ok(MountResult::Mounted(mount_point)) => {
                        let id = notify_mounted(&notifier, &path, &mount_point).await;
                        mount_retries.remove(&path);
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
                        mount_retries.remove(&path);
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
                        mount_retries.remove(&path);
                    }
                    Err(zbus::Error::MethodError(name, _, _))
                        if name.starts_with("org.freedesktop.UDisks2.Error.AlreadyMounted") =>
                    {
                        debug!("mount {}: {name}", dev(&path));
                        mount_retries.remove(&path);
                    }
                    Err(e) => {
                        let retries = mount_retries.entry(path.clone()).or_default();
                        if retries.attempts < MAX_MOUNT_RETRIES {
                            retries.attempts += 1;
                            eprintln!(
                                "udiskr: mount {} failed ({e}), retry {}/{}",
                                dev(&path),
                                retries.attempts,
                                MAX_MOUNT_RETRIES
                            );
                            let path_clone = path.clone();
                            let sys_clone = system.clone();
                            pending.push(Box::pin(try_mount_retry(sys_clone, path_clone)));
                        } else {
                            eprintln!(
                                "udiskr: mount {} failed after {} retries: {e}",
                                dev(&path),
                                MAX_MOUNT_RETRIES
                            );
                            mount_retries.remove(&path);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Mount one device with a retry delay. Used for transient mount failures.
async fn try_mount_retry(
    system: Connection,
    object_path: OwnedObjectPath,
) -> (OwnedObjectPath, Result<MountResult, zbus::Error>) {
    tokio::time::sleep(Duration::from_millis(MOUNT_RETRY_DELAY_MS)).await;
    try_mount(system, object_path).await
}

/// Mount one device. Returns `Ok(mount_point)` or `Ok(Skipped(reason))`.
async fn try_mount(
    system: Connection,
    object_path: OwnedObjectPath,
) -> (OwnedObjectPath, Result<MountResult, zbus::Error>) {
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
            return Ok(MountResult::AlreadyMounted(mp.to_string()));
        }
        fs.mount(&HashMap::new()).await.map(MountResult::Mounted)
    }
    .await;
    (object_path, res)
}

/// UDisks2 mount points are NUL-terminated byte strings (`aay`).
///
/// Returns a borrowed `&str` with no allocation. Invalid UTF-8 yields `None`
/// (the caller decides whether to fall back to lossy conversion).
fn first_mount_point(points: &[Vec<u8>]) -> Option<&str> {
    let bytes = points.first()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok()
}

/// "/org/.../block_devices/sdb1" -> "sdb1" (never panics).
fn dev(object_path: &OwnedObjectPath) -> &str {
    object_path
        .as_str()
        .strip_prefix(PREFIX)
        .unwrap_or(object_path.as_str())
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
