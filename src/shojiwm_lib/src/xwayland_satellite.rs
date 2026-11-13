use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::rustix::{
    self,
    fs::{FileType, FlockOperation, Mode, OFlags, flock, fstat, lstat, mkdir, open, unlink},
    io::{Errno, FdFlags, fcntl_setfd},
    net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType},
    process::{Pid, getpid, getuid, test_kill_process},
};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::{
            net::{SocketAddr, UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{info, warn};

const TMP_UNIX_DIR: &str = "/tmp";
const X11_TMP_UNIX_DIR: &str = "/tmp/.X11-unix";
const SATELLITE_LOG_FILE: &str = "xwayland-satellite.log";

pub struct SatelliteInstance {
    pub display_name: String,
    pub display_number: u32,
    /// Present when satellite runs in-process; it keeps what a restart needs.
    pub embedded: Option<EmbeddedSatellite>,
    _unix_guard: UnlinkGuard,
    _lock_guard: UnlinkGuard,
}

struct UnlinkGuard(PathBuf);

impl Drop for UnlinkGuard {
    fn drop(&mut self) {
        let _ = unlink(&self.0);
    }
}

/// How xwayland-satellite is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SatelliteMode {
    /// In-process, on its own thread (the default).
    Embedded,
    /// As a separate process, from this executable path or `PATH` lookup.
    External(PathBuf),
}

/// The satellite mode chosen by the environment, or `None` when satellite is
/// disabled and the built-in Xwayland support is used instead.
///
/// - `SHOJI_XWAYLAND_SATELLITE=0|off`: disabled.
/// - `SHOJI_XWAYLAND_SATELLITE_PATH=<path>` (or `--xwayland-satellite-path`):
///   external, running that binary.
/// - `SHOJI_XWAYLAND_SATELLITE=external`: external, running the
///   `xwayland-satellite` found in `PATH`.
/// - otherwise: embedded.
pub fn satellite_mode() -> Option<SatelliteMode> {
    satellite_mode_from(
        std::env::var_os("SHOJI_XWAYLAND_SATELLITE"),
        std::env::var_os("SHOJI_XWAYLAND_SATELLITE_PATH"),
    )
}

fn satellite_mode_from(setting: Option<OsString>, path: Option<OsString>) -> Option<SatelliteMode> {
    if setting
        .as_ref()
        .is_some_and(|value| value == "0" || value == "off")
    {
        return None;
    }
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        return Some(SatelliteMode::External(expand_tilde_in_path(path)));
    }
    if setting.is_some_and(|value| value == "external") {
        return Some(SatelliteMode::External(PathBuf::from("xwayland-satellite")));
    }
    Some(SatelliteMode::Embedded)
}

pub fn satellite_requested() -> bool {
    satellite_mode().is_some()
}

/// Start xwayland-satellite as a separate process running `path`.
pub fn spawn_external_satellite(
    path: &Path,
) -> Result<SatelliteInstance, Box<dyn std::error::Error>> {
    if !test_listenfd_support(path) {
        return Err(format!(
            "{} does not support --test-listenfd-support / -listenfd integration",
            path.display()
        )
        .into());
    }

    ensure_x11_unix_dir()?;
    let (display_number, _lock_fd, lock_guard, abstract_listener, unix_listener, unix_guard) =
        reserve_x11_display(0)?;
    let display_name = format!(":{display_number}");

    let child = spawn_satellite_process(
        path,
        &display_name,
        abstract_listener.as_ref(),
        &unix_listener,
    )?;

    spawn_waiter_thread(path.to_path_buf(), child);

    Ok(SatelliteInstance {
        display_name,
        display_number,
        embedded: None,
        _unix_guard: unix_guard,
        _lock_guard: lock_guard,
    })
}

/// Reserve an X11 display for an in-process satellite. Nothing runs yet;
/// [`EmbeddedSatellite::start`] starts it.
pub fn reserve_embedded_satellite(
    events: Sender<SatelliteEvent>,
) -> Result<SatelliteInstance, Box<dyn std::error::Error>> {
    // Satellite runs `Xwayland` from `PATH`. Without it no run can ever serve
    // the display, and an exported `DISPLAY` whose socket nobody answers makes
    // every X11 client (fcitx5, Chromium, ...) hang in its connection setup.
    if find_in_path(XWAYLAND_BINARY).is_none() {
        return Err(format!("{XWAYLAND_BINARY} not found in PATH").into());
    }
    ensure_x11_unix_dir()?;
    let (display_number, _lock_fd, lock_guard, abstract_listener, unix_listener, unix_guard) =
        reserve_x11_display(0)?;
    let display_name = format!(":{display_number}");

    let listeners = abstract_listener
        .into_iter()
        .chain(std::iter::once(unix_listener))
        .map(OwnedFd::from)
        .collect();

    Ok(SatelliteInstance {
        embedded: Some(EmbeddedSatellite {
            display_name: display_name.clone(),
            listeners,
            flags: glamor_flags(),
            events,
            started_at: None,
            restart_delay: INITIAL_RESTART_DELAY,
            generation: 0,
            ready: false,
            failed_starts: 0,
        }),
        display_name,
        display_number,
        _unix_guard: unix_guard,
        _lock_guard: lock_guard,
    })
}

/// What the embedded satellite thread reports to the compositor's event loop.
#[derive(Debug)]
pub enum SatelliteEvent {
    /// Xwayland is up and satellite manages it.
    Ready {
        generation: u64,
        display: String,
        xwayland_pid: u32,
    },
    /// The satellite thread ended: Xwayland exited, or satellite panicked.
    Exited { generation: u64, panicked: bool },
}

/// Whether each xdg_toplevel of one satellite run, by its protocol id on that
/// run's connection, suggests floating by its X11 window's traits (window
/// type, modal state, `WM_TRANSIENT_FOR`, `WM_WINDOW_ROLE`), which the
/// toplevel itself cannot carry. Satellite fills it in before the toplevel's
/// requests reach the compositor.
#[derive(Clone, Default)]
pub struct FloatHints(Arc<Mutex<HashMap<u32, bool>>>);

impl FloatHints {
    pub fn suggests_float(&self, toplevel: u32) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&toplevel)
            .copied()
            .unwrap_or(false)
    }
}

/// Delay before the first restart after a quick failure.
const INITIAL_RESTART_DELAY: Duration = Duration::from_millis(500);
/// Longest delay between restarts while satellite keeps failing.
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
/// A run this long counts as healthy and resets the restart delay.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// Consecutive runs that end before Xwayland is ready, after which satellite
/// is given up on.
const MAX_FAILED_STARTS: u32 = 3;
/// The Xwayland executable satellite runs.
const XWAYLAND_BINARY: &str = "Xwayland";

/// xwayland-satellite running in-process.
///
/// Satellite still talks to the compositor over the Wayland protocol, through
/// one end of a socket pair that the compositor inserts as a client; only the
/// process boundary is gone. The X11 display (lock file and listening sockets)
/// is reserved once and handed to every run, so a restarted satellite serves the
/// same `DISPLAY`.
pub struct EmbeddedSatellite {
    display_name: String,
    /// The display's listening sockets, kept for restarts; each run gets
    /// duplicates, which it passes on to Xwayland.
    listeners: Vec<OwnedFd>,
    flags: Vec<String>,
    events: Sender<SatelliteEvent>,
    started_at: Option<Instant>,
    restart_delay: Duration,
    /// Identifies the current run, so a late event from an older one is ignored.
    generation: u64,
    /// Whether the current run reported Xwayland ready.
    ready: bool,
    /// Consecutive runs that ended before Xwayland was ready.
    failed_starts: u32,
}

impl EmbeddedSatellite {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Start satellite on a new thread. Returns the compositor's end of its
    /// Wayland connection, to be inserted as a client, and that run's float
    /// hints.
    pub fn start(&mut self) -> io::Result<(UnixStream, FloatHints)> {
        let (compositor_end, satellite_end) = UnixStream::pair()?;
        let listenfds = self
            .listeners
            .iter()
            .map(OwnedFd::try_clone)
            .collect::<io::Result<Vec<_>>>()?;
        self.generation += 1;
        self.ready = false;
        let generation = self.generation;
        let float_hints = FloatHints::default();
        let data = EmbeddedRunData {
            display: self.display_name.clone(),
            listenfds: Mutex::new(listenfds),
            flags: self.flags.clone(),
            server: Mutex::new(Some(satellite_end)),
            events: self.events.clone(),
            generation,
            float_hints: float_hints.clone(),
        };
        let events = self.events.clone();
        std::thread::Builder::new()
            .name("xwayland-satellite".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    satellite::main(data);
                }));
                let panicked = result.is_err();
                let _ = events.send(SatelliteEvent::Exited {
                    generation,
                    panicked,
                });
            })?;
        self.started_at = Some(Instant::now());
        info!(
            display = %self.display_name,
            generation,
            "started embedded xwayland-satellite"
        );
        Ok((compositor_end, float_hints))
    }

    /// The current run reported Xwayland ready.
    pub fn mark_ready(&mut self) {
        self.ready = true;
        self.failed_starts = 0;
    }

    /// The current run ended. Returns `false` when satellite should be given
    /// up on: Xwayland never came up in several runs in a row, so the display
    /// would only keep X11 clients waiting.
    pub fn run_ended(&mut self) -> bool {
        if !self.ready {
            self.failed_starts += 1;
        }
        self.failed_starts < MAX_FAILED_STARTS
    }

    /// How long to wait before restarting after the current run ended: short
    /// after a long healthy run, growing while satellite keeps failing fast.
    pub fn next_restart_delay(&mut self) -> Duration {
        let ran = self
            .started_at
            .map_or(Duration::ZERO, |start| start.elapsed());
        self.restart_delay = if ran >= HEALTHY_RUN {
            INITIAL_RESTART_DELAY
        } else {
            (self.restart_delay * 2).min(MAX_RESTART_DELAY)
        };
        self.restart_delay
    }
}

/// The first executable file named `name` in `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// `-glamor` for Xwayland, from `SHOJI_XWAYLAND_SATELLITE_GLAMOR`.
fn glamor_flags() -> Vec<String> {
    std::env::var("SHOJI_XWAYLAND_SATELLITE_GLAMOR")
        .ok()
        .filter(|glamor| matches!(glamor.as_str(), "gl" | "es" | "none"))
        .map(|glamor| vec!["-glamor".to_owned(), glamor])
        .unwrap_or_default()
}

/// What satellite needs from its embedder, for one run.
struct EmbeddedRunData {
    display: String,
    listenfds: Mutex<Vec<OwnedFd>>,
    flags: Vec<String>,
    server: Mutex<Option<UnixStream>>,
    events: Sender<SatelliteEvent>,
    generation: u64,
    float_hints: FloatHints,
}

impl satellite::RunData for EmbeddedRunData {
    fn display(&self) -> Option<&str> {
        Some(&self.display)
    }

    fn listenfds(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut *self.listenfds.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn flags(&self) -> &[String] {
        &self.flags
    }

    fn server(&self) -> Option<UnixStream> {
        self.server.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn float_hints(&self) -> Option<satellite::FloatHints> {
        let hints = self.float_hints.0.clone();
        Some(Box::new(move |toplevel, floats| {
            hints
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(toplevel, floats);
        }))
    }

    fn xwayland_ready(&self, display: String, pid: u32) {
        let _ = self.events.send(SatelliteEvent::Ready {
            generation: self.generation,
            display,
            xwayland_pid: pid,
        });
    }
}

fn expand_tilde_in_path(path: OsString) -> PathBuf {
    let path = PathBuf::from(path);
    let Some(raw) = path.to_str() else {
        return path;
    };
    if raw == "~" {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))
            .unwrap_or(path);
    }
    path
}

fn test_listenfd_support(path: &Path) -> bool {
    let mut child = match Command::new(path)
        .args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            warn!(?error, path = ?path, "failed to spawn xwayland-satellite probe");
            return false;
        }
    };

    match child.wait() {
        Ok(status) => status.success(),
        Err(error) => {
            warn!(?error, path = ?path, "failed to wait for xwayland-satellite probe");
            false
        }
    }
}

fn ensure_x11_unix_dir() -> Result<(), Box<dyn std::error::Error>> {
    match mkdir(X11_TMP_UNIX_DIR, 0o1777.into()) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => ensure_x11_unix_perms(),
        Err(error) => {
            Err(io::Error::other(format!("failed to create {X11_TMP_UNIX_DIR}: {error}")).into())
        }
    }
}

fn ensure_x11_unix_perms() -> Result<(), Box<dyn std::error::Error>> {
    let x11_tmp = lstat(X11_TMP_UNIX_DIR)?;
    let tmp = lstat(TMP_UNIX_DIR)?;

    if !(x11_tmp.st_uid == tmp.st_uid || x11_tmp.st_uid == getuid().as_raw()) {
        return Err(io::Error::other("wrong ownership for /tmp/.X11-unix").into());
    }
    if (x11_tmp.st_mode & 0o022) != 0o022 {
        return Err(io::Error::other("/tmp/.X11-unix is not writable").into());
    }
    if (x11_tmp.st_mode & 0o1000) != 0o1000 {
        return Err(io::Error::other("/tmp/.X11-unix is missing the sticky bit").into());
    }

    Ok(())
}

type ReservedX11Display = (
    u32,
    OwnedFd,
    UnlinkGuard,
    Option<UnixListener>,
    UnixListener,
    UnlinkGuard,
);

fn reserve_x11_display(start: u32) -> Result<ReservedX11Display, Box<dyn std::error::Error>> {
    // Held for the whole scan, so a stale lock is judged and replaced by one
    // of this user's compositors at a time.
    let reclaim_turn = take_reclaim_turn();
    for display_number in start..start + 50 {
        let lock_path = PathBuf::from(format!("/tmp/.X{display_number}-lock"));
        let flags = OFlags::WRONLY | OFlags::CLOEXEC | OFlags::CREATE | OFlags::EXCL;
        let lock_fd = match open(&lock_path, flags, 0o444.into()) {
            Ok(lock_fd) => lock_fd,
            Err(_) if reclaim_turn.is_some() && reclaim_stale_lock(&lock_path) => {
                let Ok(lock_fd) = open(&lock_path, flags, 0o444.into()) else {
                    continue;
                };
                lock_fd
            }
            Err(_) => continue,
        };
        let lock_guard = UnlinkGuard(lock_path);

        let pid_string = format!("{:>10}\n", getpid().as_raw_nonzero());
        rustix::io::write(&lock_fd, pid_string.as_bytes())?;

        match open_display_sockets(display_number) {
            Ok((abstract_listener, unix_listener, unix_guard)) => {
                return Ok((
                    display_number,
                    lock_fd,
                    lock_guard,
                    abstract_listener,
                    unix_listener,
                    unix_guard,
                ));
            }
            Err(error) if x11_socket_is_in_use(error.as_ref()) => {
                info!(
                    display = %format_args!(":{display_number}"),
                    ?error,
                    "X11 display socket is already in use; trying the next display"
                );
            }
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::other("no free X11 display found").into())
}

/// This user's compositors take turns at stale locks: one that read a dead
/// pid could otherwise unlink the lock another made a moment later, and the
/// loser's guard could remove the winner's. Without the turn, because the
/// path is not ours or flock fails, stale locks are left alone.
fn take_reclaim_turn() -> Option<OwnedFd> {
    let uid = getuid();
    let path = format!("/tmp/.shojiwm-x11-reclaim-{}", uid.as_raw());
    let flags =
        OFlags::RDONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = open(&path, flags, Mode::RUSR | Mode::WUSR).ok()?;
    let stat = fstat(&fd).ok()?;
    if stat.st_uid != uid.as_raw() || FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
    {
        return None;
    }
    flock(&fd, FlockOperation::LockExclusive).ok()?;
    Some(fd)
}

/// Removes a display lock left by a server that is gone: its pid, as the X
/// lock format writes it, names no process. Only this user's regular files
/// are read, another user's cannot be unlinked from sticky /tmp anyway.
/// Xorg and wlroots do the same.
fn reclaim_stale_lock(lock_path: &Path) -> bool {
    let Some(pid) = lock_pid(lock_path) else {
        return false;
    };
    if test_kill_process(pid) != Err(Errno::SRCH) {
        return false;
    }
    match unlink(lock_path) {
        Ok(()) => {
            info!(lock = ?lock_path, pid = pid.as_raw_nonzero(), "reclaimed the X11 lock of a server that is gone");
            true
        }
        Err(_) => false,
    }
}

/// The pid in a lock of this user's, opened without following a link or
/// waiting on a FIFO, and read no further than the 11 bytes of `%10d\n`.
fn lock_pid(lock_path: &Path) -> Option<Pid> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = open(lock_path, flags, Mode::empty()).ok()?;
    let stat = fstat(&fd).ok()?;
    if stat.st_uid != getuid().as_raw()
        || FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
    {
        return None;
    }
    let mut record = [0; 11];
    let read = rustix::io::read(&fd, &mut record).ok()?;
    let text = std::str::from_utf8(&record[..read]).ok()?;
    text.trim().parse().ok().and_then(Pid::from_raw)
}

fn x11_socket_is_in_use(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
        )
    })
}

fn bind_to_socket(addr: &SocketAddr) -> Result<UnixListener, Box<dyn std::error::Error>> {
    Ok(UnixListener::bind_addr(addr)?)
}

#[cfg(target_os = "linux")]
fn bind_to_abstract_socket(
    display_number: u32,
) -> Result<UnixListener, Box<dyn std::error::Error>> {
    use std::os::linux::net::SocketAddrExt;

    let name = format!("/tmp/.X11-unix/X{display_number}");
    let addr = SocketAddr::from_abstract_name(name)?;
    bind_to_socket(&addr)
}

#[cfg(not(target_os = "linux"))]
fn bind_to_abstract_socket(
    _display_number: u32,
) -> Result<UnixListener, Box<dyn std::error::Error>> {
    Err(io::Error::other("abstract X11 sockets are unsupported on this platform").into())
}

fn bind_to_unix_socket(
    display_number: u32,
) -> Result<(UnixListener, UnlinkGuard), Box<dyn std::error::Error>> {
    let path = PathBuf::from(format!("/tmp/.X11-unix/X{display_number}"));
    let addr = SocketAddr::from_pathname(&path)?;
    let listener = match UnixListener::bind_addr(&addr) {
        Ok(listener) => listener,
        // A socket file of ours nothing listens on is a dead server's and
        // goes. Anything else there, or one that will not go, keeps the
        // display taken and the caller moves on.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if !is_our_dead_socket(&path) || unlink(&path).is_err() {
                return Err(error.into());
            }
            bind_to_socket(&addr)?
        }
        Err(error) => return Err(error.into()),
    };
    Ok((listener, UnlinkGuard(path)))
}

/// A socket this user owns that refuses a connection. The probe never
/// waits: a listener whose backlog is full answers EAGAIN and counts as
/// alive, only a refusal says nobody listens.
fn is_our_dead_socket(path: &Path) -> bool {
    let Ok(stat) = lstat(path) else {
        return false;
    };
    if stat.st_uid != getuid().as_raw() || FileType::from_raw_mode(stat.st_mode) != FileType::Socket
    {
        return false;
    }
    let flags = SocketFlags::NONBLOCK | SocketFlags::CLOEXEC;
    let (Ok(probe), Ok(address)) = (
        rustix::net::socket_with(AddressFamily::UNIX, SocketType::STREAM, flags, None),
        SocketAddrUnix::new(path),
    ) else {
        return false;
    };
    rustix::net::connect(&probe, &address) == Err(Errno::CONNREFUSED)
}

fn open_display_sockets(
    display_number: u32,
) -> Result<(Option<UnixListener>, UnixListener, UnlinkGuard), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    let abstract_listener = Some(bind_to_abstract_socket(display_number)?);
    #[cfg(not(target_os = "linux"))]
    let abstract_listener = None;

    let (unix_listener, unix_guard) = bind_to_unix_socket(display_number)?;
    Ok((abstract_listener, unix_listener, unix_guard))
}

fn spawn_satellite_process(
    path: &Path,
    display_name: &str,
    abstract_listener: Option<&UnixListener>,
    unix_listener: &UnixListener,
) -> Result<Child, Box<dyn std::error::Error>> {
    let abstract_raw = abstract_listener.map(AsRawFd::as_raw_fd);
    let unix_raw = unix_listener.as_raw_fd();
    let glamor = std::env::var("SHOJI_XWAYLAND_SATELLITE_GLAMOR").ok();

    let mut command = Command::new(path);
    command
        .arg(display_name)
        .arg("-listenfd")
        .arg(unix_raw.to_string())
        .env_remove("DISPLAY")
        .stdin(Stdio::null());

    if satellite_logging_enabled() {
        let log_path = satellite_log_path();
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)?;
        let stderr_file = log_file.try_clone()?;
        command.stdout(Stdio::from(log_file));
        command.stderr(Stdio::from(stderr_file));
    } else {
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
    }

    if let Some(abstract_raw) = abstract_raw {
        command.arg("-listenfd").arg(abstract_raw.to_string());
    }
    if let Some(glamor) = glamor.as_deref()
        && matches!(glamor, "gl" | "es" | "none")
    {
        command.arg("-glamor").arg(glamor);
    }

    unsafe {
        command.pre_exec(move || {
            let unix_fd = BorrowedFd::borrow_raw(unix_raw);
            fcntl_setfd(unix_fd, FdFlags::empty()).map_err(|error| {
                io::Error::other(format!("failed to pass unix socket fd: {error}"))
            })?;

            if let Some(abstract_raw) = abstract_raw {
                let abstract_fd = BorrowedFd::borrow_raw(abstract_raw);
                fcntl_setfd(abstract_fd, FdFlags::empty()).map_err(|error| {
                    io::Error::other(format!("failed to pass abstract socket fd: {error}"))
                })?;
            }

            Ok(())
        });
    }

    Ok(command.spawn()?)
}

fn satellite_logging_enabled() -> bool {
    std::env::var_os("SHOJI_XWAYLAND_SATELLITE_LOG")
        .is_some_and(|value| value != "0" && value != "off")
}

fn satellite_log_path() -> PathBuf {
    std::env::var_os("SHOJI_XWAYLAND_SATELLITE_LOG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::shoji_log_dir().join(SATELLITE_LOG_FILE))
}

fn spawn_waiter_thread(path: PathBuf, mut child: Child) {
    let _ = std::thread::Builder::new()
        .name("xwayland-satellite-wait".to_string())
        .spawn(move || match child.wait() {
            Ok(status) => {
                warn!(path = ?path, ?status, "xwayland-satellite exited");
            }
            Err(error) => {
                warn!(path = ?path, ?error, "failed waiting for xwayland-satellite");
            }
        });
}

#[cfg(test)]
mod mode_tests {
    use super::{SatelliteMode, satellite_mode_from};
    use std::{ffi::OsString, path::PathBuf};

    fn mode(setting: Option<&str>, path: Option<&str>) -> Option<SatelliteMode> {
        satellite_mode_from(setting.map(OsString::from), path.map(OsString::from))
    }

    #[test]
    fn embedded_by_default() {
        assert_eq!(mode(None, None), Some(SatelliteMode::Embedded));
        assert_eq!(mode(Some("1"), None), Some(SatelliteMode::Embedded));
        assert_eq!(mode(None, Some("")), Some(SatelliteMode::Embedded));
    }

    #[test]
    fn external_from_path_lookup_or_an_explicit_binary() {
        assert_eq!(
            mode(Some("external"), None),
            Some(SatelliteMode::External(PathBuf::from("xwayland-satellite")))
        );
        assert_eq!(
            mode(None, Some("/opt/xwls/xwayland-satellite")),
            Some(SatelliteMode::External(PathBuf::from(
                "/opt/xwls/xwayland-satellite"
            )))
        );
        // An explicit binary wins over the PATH lookup.
        assert_eq!(
            mode(Some("external"), Some("/opt/xwls/xwayland-satellite")),
            Some(SatelliteMode::External(PathBuf::from(
                "/opt/xwls/xwayland-satellite"
            )))
        );
    }

    #[test]
    fn off_disables_satellite_even_with_a_path() {
        assert_eq!(mode(Some("off"), None), None);
        assert_eq!(mode(Some("0"), Some("/opt/xwls/xwayland-satellite")), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        X11_TMP_UNIX_DIR, bind_to_abstract_socket, bind_to_unix_socket, ensure_x11_unix_dir,
        reserve_x11_display, rustix,
    };
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU32, Ordering},
    };

    static NEXT_TEST_DISPLAY: AtomicU32 = AtomicU32::new(0);

    /// The display `reserve_x11_display(start)` gives within five seconds,
    /// kept until the reservation is dropped by the caller's scope.
    fn reserve_in_time(start: u32) -> Option<Result<super::ReservedX11Display, String>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(reserve_x11_display(start).map_err(|error| error.to_string()));
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .ok()
    }

    fn dead_pid() -> u32 {
        let mut gone = std::process::Command::new("true").spawn().expect("spawn");
        let pid = gone.id();
        gone.wait().expect("reap");
        pid
    }

    struct TestDisplayPaths {
        display_numbers: [u32; 4],
    }

    impl TestDisplayPaths {
        fn new() -> Self {
            let display_number = 100_000
                + (std::process::id() % 10_000) * 100
                + NEXT_TEST_DISPLAY.fetch_add(4, Ordering::Relaxed);
            let paths = Self {
                display_numbers: std::array::from_fn(|n| display_number + n as u32),
            };
            paths.remove();
            paths
        }

        fn start(&self) -> u32 {
            self.display_numbers[0]
        }

        fn remove(&self) {
            for display_number in self.display_numbers {
                let _ = fs::remove_file(format!("/tmp/.X{display_number}-lock"));
                let socket = format!("{X11_TMP_UNIX_DIR}/X{display_number}");
                let _ = fs::remove_file(&socket);
                let _ = fs::remove_dir(&socket);
            }
        }
    }

    impl Drop for TestDisplayPaths {
        fn drop(&mut self) {
            self.remove();
        }
    }

    #[test]
    fn reclaims_the_lock_and_socket_of_a_server_that_is_gone() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        let pid = dead_pid();
        let lock = format!("/tmp/.X{}-lock", paths.start());
        fs::write(&lock, format!("{pid:>10}\n")).expect("write a stale lock");
        // Its pathname socket outlives it too: bound, never unlinked.
        let socket = PathBuf::from(format!("{X11_TMP_UNIX_DIR}/X{}", paths.start()));
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("leave a socket file"));

        let reservation = reserve_x11_display(paths.start()).expect("reserve display");

        assert_eq!(reservation.0, paths.start());
        let owner: u32 = fs::read_to_string(&lock).unwrap().trim().parse().unwrap();
        assert_eq!(owner, std::process::id());
        drop(reservation);
    }

    #[test]
    fn keeps_the_lock_of_a_server_that_lives() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        let lock = format!("/tmp/.X{}-lock", paths.start());
        let alive = format!("{:>10}\n", std::process::id());
        fs::write(&lock, &alive).expect("write a live lock");

        let reservation = reserve_x11_display(paths.start()).expect("reserve next display");

        assert_eq!(reservation.0, paths.start() + 1);
        assert_eq!(fs::read_to_string(&lock).unwrap(), alive);
        drop(reservation);
    }

    #[test]
    fn contenders_for_one_stale_lock_leave_the_winner_its_lock() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        for _ in 0..50 {
            let paths = TestDisplayPaths::new();
            let lock = format!("/tmp/.X{}-lock", paths.start());
            fs::write(&lock, format!("{:>10}\n", dead_pid())).expect("write a stale lock");
            let gate = std::sync::Arc::new(std::sync::Barrier::new(4));
            let contenders: Vec<_> = (0..4)
                .map(|_| {
                    let gate = gate.clone();
                    let start = paths.start();
                    std::thread::spawn(move || {
                        gate.wait();
                        reserve_x11_display(start).map_err(|error| error.to_string())
                    })
                })
                .collect();
            let reservations: Vec<_> = contenders
                .into_iter()
                .map(|contender| contender.join().unwrap().expect("reserve display"))
                .collect();
            let winners = reservations.iter().filter(|r| r.0 == paths.start()).count();
            assert_eq!(winners, 1);
            let owner = fs::read_to_string(&lock).expect("the winner keeps its lock");
            assert_eq!(owner.trim().parse::<u32>().unwrap(), std::process::id());
            drop(reservations);
        }
    }

    #[test]
    fn a_lock_that_is_a_fifo_or_a_link_is_passed_over_without_waiting() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        let fifo = format!("/tmp/.X{}-lock", paths.start());
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .expect("make a FIFO lock");
        let link = format!("/tmp/.X{}-lock", paths.start() + 1);
        std::os::unix::fs::symlink(&fifo, &link).expect("link a lock to it");

        let reservation = reserve_in_time(paths.start()).expect("not held up by the FIFO");

        let reservation = reservation.expect("reserve the display after them");
        assert_eq!(reservation.0, paths.start() + 2);
        assert!(fs::symlink_metadata(&fifo).is_ok() && fs::symlink_metadata(&link).is_ok());
        drop(reservation);
    }

    #[test]
    fn a_listener_with_a_full_backlog_keeps_its_display_without_holding_us_up() {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        let socket = PathBuf::from(format!("{X11_TMP_UNIX_DIR}/X{}", paths.start()));
        let address = SocketAddrUnix::new(&socket).unwrap();
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        rustix::net::bind(&listener, &address).expect("bind a pathname socket");
        rustix::net::listen(&listener, 0).unwrap();
        let mut queued = Vec::new();
        for _ in 0..64 {
            let client = rustix::net::socket_with(
                AddressFamily::UNIX,
                SocketType::STREAM,
                SocketFlags::NONBLOCK,
                None,
            )
            .unwrap();
            if rustix::net::connect(&client, &address).is_err() {
                break;
            }
            queued.push(client);
        }

        let reservation = reserve_in_time(paths.start()).expect("not held up by the backlog");

        let reservation = reservation.expect("reserve the next display");
        assert_eq!(reservation.0, paths.start() + 1);
        assert!(socket.exists());
        drop(reservation);
        drop(queued);
    }

    #[test]
    fn a_socket_path_that_will_not_go_passes_over_its_display() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        // Refuses a connection and cannot be unlinked, as another user's
        // stale socket in sticky /tmp/.X11-unix cannot.
        let socket = PathBuf::from(format!("{X11_TMP_UNIX_DIR}/X{}", paths.start()));
        fs::create_dir(&socket).expect("put something there that is not ours to remove");

        let reservation = reserve_x11_display(paths.start()).expect("reserve the next display");

        assert_eq!(reservation.0, paths.start() + 1);
        assert!(socket.is_dir());
        drop(reservation);
    }

    #[test]
    fn skips_a_display_with_an_occupied_abstract_socket() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        let occupied = bind_to_abstract_socket(paths.start()).expect("occupy abstract socket");

        let reservation = reserve_x11_display(paths.start()).expect("reserve next display");

        assert_eq!(reservation.0, paths.start() + 1);
        drop(reservation);
        drop(occupied);
    }

    #[test]
    fn does_not_unlink_an_occupied_pathname_socket() {
        ensure_x11_unix_dir().expect("prepare X11 socket directory");
        let paths = TestDisplayPaths::new();
        let (occupied, occupied_guard) =
            bind_to_unix_socket(paths.start()).expect("occupy pathname socket");

        let reservation = reserve_x11_display(paths.start()).expect("reserve next display");

        assert_eq!(reservation.0, paths.start() + 1);
        assert!(PathBuf::from(format!("{X11_TMP_UNIX_DIR}/X{}", paths.start())).exists());
        drop(reservation);
        drop(occupied);
        drop(occupied_guard);
    }
}
