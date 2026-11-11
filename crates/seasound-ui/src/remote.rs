//! Hand files from a later launch to the window that is already listening.
//!
//! A launch with no paths always opens a window. The first one binds
//! `instance.json` in the config directory. Another launch that has paths
//! sends them there and exits, unless `--new-instance` says to keep them.
//! A second window does not take the listener, so a test copy can run beside
//! one that is playing. Windows Explorer often starts one process per selected
//! file. The listener waits briefly so those processes become one playlist.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const ACCEPT_POLL: Duration = Duration::from_millis(15);
const COALESCE_CAP: Duration = Duration::from_secs(2);
const DELIVER_WAIT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Open,
    Enqueue,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Launch {
    pub(crate) help: bool,
    pub(crate) new_instance: bool,
    pub(crate) mode: Mode,
    pub(crate) paths: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Incoming {
    pub(crate) mode: Mode,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) raise: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Batch {
    pub(crate) mode: Mode,
    pub(crate) paths: Vec<PathBuf>,
    raise: bool,
}

impl Batch {
    pub(crate) fn raise(&self) -> bool {
        self.raise
    }
}

#[derive(Serialize, Deserialize)]
struct Wire {
    mode: Mode,
    paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Endpoint {
    pid: u32,
    port: u16,
}

struct Pending {
    paths: Vec<PathBuf>,
    raise: bool,
    first: Instant,
    due: Instant,
}

#[derive(Default)]
struct Inbox {
    open: Option<Pending>,
    enqueue: Option<Pending>,
}

impl Inbox {
    fn waiting(&self) -> bool {
        self.open.is_some() || self.enqueue.is_some()
    }

    fn push(&mut self, incoming: Incoming, now: Instant, wait: Duration) {
        if incoming.paths.is_empty() {
            return;
        }
        let slot = match incoming.mode {
            Mode::Open => &mut self.open,
            Mode::Enqueue => &mut self.enqueue,
        };
        match slot {
            Some(pending) => {
                pending.paths.extend(incoming.paths);
                pending.raise |= incoming.raise;
                let cap = pending.first + COALESCE_CAP;
                pending.due = (now + wait).min(cap);
            }
            None => {
                *slot = Some(Pending {
                    paths: incoming.paths,
                    raise: incoming.raise,
                    first: now,
                    due: now + wait,
                });
            }
        }
    }

    /// Open is taken even while a load is in progress, because it replaces that load.
    /// Enqueue waits, and it also waits while an open is still being collected.
    fn poll(&mut self, now: Instant, busy: bool) -> Option<Batch> {
        if let Some(pending) = take_due(&mut self.open, now) {
            return Some(batch(Mode::Open, pending));
        }
        if busy || self.open.is_some() {
            return None;
        }
        take_due(&mut self.enqueue, now).map(|pending| batch(Mode::Enqueue, pending))
    }
}

fn batch(mode: Mode, pending: Pending) -> Batch {
    Batch {
        mode,
        paths: pending.paths,
        raise: pending.raise,
    }
}

fn take_due(slot: &mut Option<Pending>, now: Instant) -> Option<Pending> {
    let due = slot.as_ref()?.due;
    if now >= due {
        slot.take()
    } else {
        None
    }
}

struct Listener {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    ctx: Arc<OnceLock<egui::Context>>,
}

impl Listener {
    fn install_context(&self, ctx: egui::Context) {
        let _ = self.ctx.set(ctx);
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) struct RemoteHub {
    listener: Option<Listener>,
    inbox: Inbox,
    rx: Receiver<Incoming>,
    startup: Option<Incoming>,
    coalesce: bool,
}

impl RemoteHub {
    pub(crate) fn is_listening(&self) -> bool {
        self.listener.is_some()
    }

    /// The process was started to replace the playlist, so the saved session stays out.
    pub(crate) fn replaces_playlist(&self) -> bool {
        matches!(
            self.startup,
            Some(Incoming {
                mode: Mode::Open,
                ..
            })
        )
    }

    pub(crate) fn install_context(&self, ctx: &egui::Context) {
        if let Some(listener) = &self.listener {
            listener.install_context(ctx.clone());
        }
    }

    pub(crate) fn waiting(&self) -> bool {
        self.startup.is_some() || self.inbox.waiting()
    }

    pub(crate) fn poll(&mut self, now: Instant, busy: bool) -> Option<Batch> {
        let wait = self.wait();
        if let Some(startup) = self.startup.take() {
            self.inbox.push(startup, now, wait);
        }
        while let Ok(incoming) = self.rx.try_recv() {
            self.inbox.push(incoming, now, wait);
        }
        self.inbox.poll(now, busy)
    }

    fn wait(&self) -> Duration {
        if self.coalesce {
            sibling_wait()
        } else {
            Duration::ZERO
        }
    }
}

pub(crate) fn help_text() -> &'static str {
    "\
SeaSound plays files in one playlist.

Usage:
  seasound [FILE...]
  seasound --enqueue [FILE...]
  seasound --new-instance [FILE...]

With no files, SeaSound opens a window. Another launch with no files opens
another window, so a second copy can run beside one that is already playing.

When files are passed and a SeaSound window is already listening, they are
sent to that window and this process exits. The first window is the one that
listens. A later window does not take its place.

  FILE...           Replace the playlist and play the files, folders, cue
                    sheets, or m3u playlists
  --open            Same as passing files
  --enqueue         Add the files to the current playlist
  --new-instance    Keep the files in this window
  --help            Show this help

The listening window installs the file-manager commands. On Linux that is a
desktop entry, including Enqueue in SeaSound. On Windows it is Open with
SeaSound and Enqueue in SeaSound. macOS accepts the same arguments.
"
}

pub(crate) fn parse_args<I, S>(args: I) -> Result<Launch, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    let mut help = false;
    let mut new_instance = false;
    let mut chosen: Option<Mode> = None;
    let mut paths = Vec::new();
    let mut ended = false;
    for arg in args {
        if !ended {
            if arg == "--" {
                ended = true;
                continue;
            }
            if let Some(text) = arg.to_str() {
                match text {
                    "--help" | "-h" => {
                        help = true;
                        continue;
                    }
                    "--new-instance" => {
                        new_instance = true;
                        continue;
                    }
                    "--enqueue" => {
                        choose(&mut chosen, Mode::Enqueue)?;
                        continue;
                    }
                    "--open" => {
                        choose(&mut chosen, Mode::Open)?;
                        continue;
                    }
                    flag if flag.starts_with('-') && flag != "-" => {
                        return Err(format!(
                            "Unknown argument {flag}. Run seasound --help for usage."
                        ));
                    }
                    _ => {}
                }
            }
        }
        paths.push(PathBuf::from(arg));
    }
    Ok(Launch {
        help,
        new_instance,
        mode: chosen.unwrap_or(Mode::Open),
        paths,
    })
}

fn choose(chosen: &mut Option<Mode>, mode: Mode) -> Result<(), String> {
    if let Some(current) = *chosen {
        if current != mode {
            return Err("Use either --open or --enqueue, not both.".into());
        }
    }
    *chosen = Some(mode);
    Ok(())
}

/// `Ok(None)` means the paths were delivered and this process should exit.
pub(crate) fn attach(config: Option<&Path>, launch: &Launch) -> Result<Option<RemoteHub>, String> {
    let startup = prepare(launch)?;
    if let Some(dir) = config {
        if !launch.new_instance {
            if let Some(incoming) = &startup {
                match deliver(dir, incoming) {
                    Ok(()) => return Ok(None),
                    Err(DeliverError::Absent) => {}
                    Err(DeliverError::Unreachable) => {
                        eprintln!(
                            "SeaSound is already open and did not take the files. Opening another window."
                        );
                    }
                }
            }
        }
    }

    let (tx, rx) = mpsc::channel();
    let listener = match config {
        Some(dir) => match claim(dir) {
            Ok(claimed) => match spawn_listener(claimed, tx) {
                Ok(listener) => Some(listener),
                Err(err) => {
                    eprintln!("SeaSound could not listen for files from a later launch: {err}");
                    None
                }
            },
            Err(ClaimError::Busy) => {
                drop(tx);
                if !launch.new_instance {
                    if let Some(incoming) = &startup {
                        if deliver(dir, incoming).is_ok() {
                            return Ok(None);
                        }
                    }
                }
                None
            }
            Err(ClaimError::Io(err)) => {
                drop(tx);
                eprintln!("SeaSound could not listen for files from a later launch: {err}");
                None
            }
        },
        None => {
            drop(tx);
            None
        }
    };
    let coalesce = listener.is_some();
    Ok(Some(RemoteHub {
        listener,
        inbox: Inbox::default(),
        rx,
        startup,
        coalesce,
    }))
}

fn prepare(launch: &Launch) -> Result<Option<Incoming>, String> {
    if launch.paths.is_empty() {
        return Ok(None);
    }
    let mut paths = Vec::with_capacity(launch.paths.len());
    for path in &launch.paths {
        let absolute = std::path::absolute(path)
            .map_err(|err| format!("Could not use {}: {err}", path.display()))?;
        if absolute.to_str().is_none() {
            return Err(format!(
                "Could not use {}: the path is not valid text",
                absolute.display()
            ));
        }
        paths.push(absolute);
    }
    Ok(Some(Incoming {
        mode: launch.mode,
        paths,
        raise: false,
    }))
}

enum DeliverError {
    Absent,
    Unreachable,
}

enum ClaimError {
    Busy,
    Io(io::Error),
}

fn deliver(dir: &Path, incoming: &Incoming) -> Result<(), DeliverError> {
    let path = instance_path(dir);
    if !path.is_file() {
        return Err(DeliverError::Absent);
    }
    let deadline = Instant::now() + DELIVER_WAIT;
    loop {
        if !path.is_file() {
            return Err(DeliverError::Absent);
        }
        match read_endpoint(&path) {
            Ok(endpoint) if endpoint.port != 0 && process_alive(endpoint.pid) => {
                match send(endpoint.port, incoming) {
                    Ok(()) => return Ok(()),
                    Err(_) if !process_alive(endpoint.pid) => return Err(DeliverError::Absent),
                    Err(_) => {
                        if Instant::now() >= deadline {
                            return Err(DeliverError::Unreachable);
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
            Ok(endpoint) if process_alive(endpoint.pid) => {
                if Instant::now() >= deadline {
                    return Err(DeliverError::Unreachable);
                }
                thread::sleep(Duration::from_millis(20));
            }
            _ => return Err(DeliverError::Absent),
        }
    }
}

fn claim(dir: &Path) -> Result<Claimed, ClaimError> {
    fs::create_dir_all(dir).map_err(ClaimError::Io)?;
    let path = instance_path(dir);
    for _ in 0..30 {
        match create_private(&path) {
            Ok(file) => {
                drop(file);
                return finish_claim(path);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => match read_endpoint(&path) {
                Ok(endpoint) if process_alive(endpoint.pid) => return Err(ClaimError::Busy),
                Ok(endpoint) => {
                    let _ = remove_if_same_pid(&path, endpoint.pid);
                }
                Err(_) if modified_recently(&path) => thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    let _ = fs::remove_file(&path);
                }
            },
            Err(err) => return Err(ClaimError::Io(err)),
        }
    }
    Err(ClaimError::Busy)
}

struct Claimed {
    listener: TcpListener,
    path: PathBuf,
}

fn finish_claim(path: PathBuf) -> Result<Claimed, ClaimError> {
    let pid = std::process::id();
    if let Err(err) = publish(&path, &Endpoint { pid, port: 0 }) {
        let _ = fs::remove_file(&path);
        return Err(ClaimError::Io(err));
    }
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(err) => {
            let _ = fs::remove_file(&path);
            return Err(ClaimError::Io(err));
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(err) => {
            let _ = fs::remove_file(&path);
            return Err(ClaimError::Io(err));
        }
    };
    if let Err(err) = publish(&path, &Endpoint { pid, port }) {
        let _ = fs::remove_file(&path);
        return Err(ClaimError::Io(err));
    }
    Ok(Claimed { listener, path })
}

fn spawn_listener(claimed: Claimed, tx: Sender<Incoming>) -> io::Result<Listener> {
    let path = claimed.path.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let ctx = Arc::new(OnceLock::new());
    let stop_thread = Arc::clone(&stop);
    let ctx_thread = Arc::clone(&ctx);
    let join = thread::Builder::new()
        .name("seasound-remote".into())
        .spawn(move || serve(claimed.listener, tx, stop_thread, ctx_thread))
        .map_err(|err| {
            let _ = fs::remove_file(&path);
            io::Error::new(ErrorKind::Other, err)
        })?;
    Ok(Listener {
        path: claimed.path,
        stop,
        join: Some(join),
        ctx,
    })
}

fn serve(
    listener: TcpListener,
    tx: Sender<Incoming>,
    stop: Arc<AtomicBool>,
    ctx: Arc<OnceLock<egui::Context>>,
) {
    if let Err(err) = listener.set_nonblocking(true) {
        eprintln!("SeaSound could not listen for files from a later launch: {err}");
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle_client(stream, &tx, &ctx),
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted =>
            {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

fn handle_client(mut stream: TcpStream, tx: &Sender<Incoming>, ctx: &OnceLock<egui::Context>) {
    let Ok(peer) = stream.peer_addr() else {
        return;
    };
    if !peer.ip().is_loopback() {
        return;
    }
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Some(bytes) = read_json_line(&mut stream) else {
        return;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return;
    };
    let Ok(wire) = serde_json::from_str::<Wire>(text.trim()) else {
        return;
    };
    let Some(incoming) = wire_to_incoming(wire) else {
        return;
    };
    if tx.send(incoming).is_err() {
        return;
    }
    let _ = stream.write_all(b"ok\n");
    let _ = stream.flush();
    if let Some(ctx) = ctx.get() {
        ctx.request_repaint();
    }
}

fn send(port: u16, incoming: &Incoming) -> io::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(400))?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let paths =
        path_strings(&incoming.paths).map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
    let wire = Wire {
        mode: incoming.mode,
        paths,
    };
    let mut payload =
        serde_json::to_vec(&wire).map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let response = read_response(&mut stream)?;
    if response.trim() == "ok" {
        Ok(())
    } else {
        Err(io::Error::new(ErrorKind::InvalidData, "listener declined"))
    }
}

fn wire_to_incoming(wire: Wire) -> Option<Incoming> {
    let mut paths = Vec::new();
    for path in wire.paths {
        if path.is_empty() {
            continue;
        }
        match std::path::absolute(path) {
            Ok(path) => paths.push(path),
            Err(_) => continue,
        }
    }
    if paths.is_empty() {
        return None;
    }
    Some(Incoming {
        mode: wire.mode,
        paths,
        raise: wire.mode == Mode::Open,
    })
}

fn read_json_line(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > MAX_MESSAGE {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    if let Some(end) = buf.iter().position(|byte| *byte == b'\n') {
        buf.truncate(end);
    }
    Some(buf)
}

fn read_response(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > 32 {
            return Err(io::Error::new(ErrorKind::InvalidData, "bad response"));
        }
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => buf.push(byte[0]),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    String::from_utf8(buf).map_err(|err| io::Error::new(ErrorKind::InvalidData, err))
}

fn path_strings(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        match path.to_str() {
            Some(text) => out.push(text.to_string()),
            None => {
                return Err(format!(
                    "Could not use {}: the path is not valid text",
                    path.display()
                ));
            }
        }
    }
    Ok(out)
}

fn instance_path(dir: &Path) -> PathBuf {
    dir.join("instance.json")
}

fn publish(path: &Path, endpoint: &Endpoint) -> io::Result<()> {
    let text = serde_json::to_string(endpoint)
        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn read_endpoint(path: &Path) -> io::Result<Endpoint> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(text.trim()).map_err(|err| io::Error::new(ErrorKind::InvalidData, err))
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn remove_if_same_pid(path: &Path, pid: u32) -> io::Result<()> {
    match read_endpoint(path) {
        Ok(endpoint) if endpoint.pid == pid => fs::remove_file(path),
        Ok(_) => Ok(()),
        Err(_) if modified_recently(path) => Ok(()),
        Err(_) => fs::remove_file(path),
    }
}

fn modified_recently(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    match modified.elapsed() {
        Ok(age) => age < Duration::from_secs(2),
        Err(_) => true,
    }
}

/// Windows starts one process per selected file. Elsewhere one launch already
/// has every path, so the playlist starts on the first frame.
fn sibling_wait() -> Duration {
    #[cfg(windows)]
    {
        Duration::from_millis(500)
    }
    #[cfg(not(windows))]
    {
        Duration::ZERO
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        let rc = unsafe { libc::kill(pid as i32, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        windows_process_alive(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(windows)]
fn windows_process_alive(pid: u32) -> bool {
    use std::ffi::c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: u32 = 5;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "seasound-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_one(path: &str) -> Incoming {
        Incoming {
            mode: Mode::Open,
            paths: vec![PathBuf::from(path)],
            raise: true,
        }
    }

    #[test]
    fn arguments_select_open_enqueue_or_another_window() {
        let open = parse_args(["seasound", "a.mp3", "b.flac"]).unwrap();
        assert_eq!(open.mode, Mode::Open);
        assert_eq!(open.paths.len(), 2);
        assert!(!open.new_instance);
        assert!(!open.help);

        let enqueue = parse_args(["seasound", "--enqueue", "--new-instance", "a.mp3"]).unwrap();
        assert_eq!(enqueue.mode, Mode::Enqueue);
        assert!(enqueue.new_instance);
        assert_eq!(enqueue.paths, vec![PathBuf::from("a.mp3")]);

        let explicit = parse_args(["seasound", "a.mp3", "--open"]).unwrap();
        assert_eq!(explicit.mode, Mode::Open);

        let literal = parse_args(["seasound", "--", "--enqueue"]).unwrap();
        assert_eq!(literal.paths, vec![PathBuf::from("--enqueue")]);
        assert_eq!(literal.mode, Mode::Open);

        let help = parse_args(["seasound", "--help", "a.mp3"]).unwrap();
        assert!(help.help);
        assert!(help_text().contains("Enqueue"));

        let err = parse_args(["seasound", "--enqueue", "--open", "a.mp3"]).unwrap_err();
        assert!(err.contains("--open"));
        assert!(parse_args(["seasound", "--nope"]).is_err());
    }

    #[test]
    fn a_relative_path_is_made_absolute_before_it_is_sent() {
        let launch = parse_args(["seasound", "track.mp3"]).unwrap();
        let incoming = prepare(&launch).unwrap().unwrap();
        assert!(incoming.paths[0].is_absolute());
        assert!(incoming.paths[0].ends_with("track.mp3"));
        assert!(!incoming.raise);
    }

    #[test]
    fn opens_from_several_processes_become_one_playlist() {
        let t0 = Instant::now();
        let mut inbox = Inbox::default();
        inbox.push(open_one("a.mp3"), t0, Duration::from_millis(500));
        inbox.push(
            open_one("b.mp3"),
            t0 + Duration::from_millis(100),
            Duration::from_millis(500),
        );
        assert!(inbox.poll(t0 + Duration::from_millis(500), false).is_none());
        let batch = inbox
            .poll(t0 + Duration::from_millis(600), false)
            .expect("due");
        assert_eq!(batch.mode, Mode::Open);
        assert_eq!(
            batch.paths,
            vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")]
        );
        assert!(batch.raise());
    }

    #[test]
    fn a_long_burst_is_released_at_the_cap() {
        let t0 = Instant::now();
        let mut inbox = Inbox::default();
        inbox.push(open_one("a.mp3"), t0, Duration::from_millis(500));
        inbox.push(
            open_one("b.mp3"),
            t0 + Duration::from_millis(1900),
            Duration::from_millis(500),
        );
        assert!(inbox
            .poll(t0 + Duration::from_millis(1999), false)
            .is_none());
        let batch = inbox.poll(t0 + COALESCE_CAP, false).expect("cap");
        assert_eq!(batch.paths.len(), 2);
    }

    #[test]
    fn enqueue_waits_for_an_open_and_for_a_load() {
        let t0 = Instant::now();
        let mut inbox = Inbox::default();
        inbox.push(open_one("a.mp3"), t0, Duration::from_millis(500));
        inbox.push(
            Incoming {
                mode: Mode::Enqueue,
                paths: vec![PathBuf::from("b.mp3")],
                raise: false,
            },
            t0,
            Duration::ZERO,
        );
        assert!(inbox.poll(t0, false).is_none());
        let open = inbox
            .poll(t0 + Duration::from_millis(500), true)
            .expect("open replaces a load");
        assert_eq!(open.mode, Mode::Open);
        assert!(inbox.poll(t0 + Duration::from_millis(500), true).is_none());
        let enqueue = inbox
            .poll(t0 + Duration::from_millis(500), false)
            .expect("enqueue");
        assert_eq!(enqueue.mode, Mode::Enqueue);
        assert!(!enqueue.raise());
    }

    #[test]
    fn sibling_wait_is_only_for_one_process_per_file() {
        #[cfg(windows)]
        assert_eq!(sibling_wait(), Duration::from_millis(500));
        #[cfg(not(windows))]
        assert_eq!(sibling_wait(), Duration::ZERO);
    }

    #[test]
    fn this_process_is_alive_and_a_huge_pid_is_not() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn a_later_launch_hands_files_to_the_listener_and_a_new_instance_does_not() {
        let dir = temp_config("remote");
        let track = dir.join("track.mp3");
        let first = parse_args(["seasound", track.to_str().unwrap()]).unwrap();
        let hub = attach(Some(&dir), &first).unwrap().expect("listener");
        assert!(hub.is_listening());
        assert!(hub.replaces_playlist());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(instance_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }

        let enqueue = parse_args(["seasound", "--enqueue", track.to_str().unwrap()]).unwrap();
        assert!(attach(Some(&dir), &enqueue).unwrap().is_none());
        let incoming = hub.rx.try_recv().expect("queued");
        assert_eq!(incoming.mode, Mode::Enqueue);
        assert_eq!(incoming.paths, vec![track.clone()]);
        assert!(!incoming.raise);

        let open = parse_args(["seasound", "--open", track.to_str().unwrap()]).unwrap();
        assert!(attach(Some(&dir), &open).unwrap().is_none());
        let incoming = hub.rx.try_recv().expect("open queued");
        assert_eq!(incoming.mode, Mode::Open);
        assert!(incoming.raise);

        let again = parse_args(["seasound", "--new-instance", track.to_str().unwrap()]).unwrap();
        let mut second = attach(Some(&dir), &again).unwrap().expect("second window");
        assert!(!second.is_listening());
        assert_eq!(second.startup.take().unwrap().paths, vec![track.clone()]);

        drop(second);
        drop(hub);
        assert!(!instance_path(&dir).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_listener_file_is_replaced() {
        let dir = temp_config("stale");
        fs::write(instance_path(&dir), r#"{"pid":4294967295,"port":9}"#).unwrap();
        let launch = parse_args(["seasound"]).unwrap();
        let hub = attach(Some(&dir), &launch).unwrap().expect("listener");
        assert!(hub.is_listening());
        let text = fs::read_to_string(instance_path(&dir)).unwrap();
        assert!(text.contains(&std::process::id().to_string()));
        assert!(!text.contains("4294967295"));
        drop(hub);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_config_directory_still_keeps_the_paths() {
        let launch = parse_args(["seasound", "--enqueue", "track.mp3"]).unwrap();
        let hub = attach(None, &launch).unwrap().expect("window");
        assert!(!hub.is_listening());
        assert!(!hub.replaces_playlist());
        assert_eq!(hub.startup.unwrap().mode, Mode::Enqueue);
    }
}
