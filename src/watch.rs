// `gocut watch`: rebuild and restart a Go program when a file it is built from changes.
//
// It replaces air for the hot-reload loop, and differs from it in three ways:
//
// - A change during a build interrupts that build's whole process group before the next one
//   starts, so builds never overlap. Stopping the watcher stops the build and the server too:
//   every child runs in its own process group, which the watcher signals on exit, and dies with
//   the watcher if the watcher is killed outright (PR_SET_PDEATHSIG).
// - With --pkg, only files the target is built from start a build. After each successful build,
//   `go list -deps` gives the target's packages, wherever they live (a `replace` to a sibling
//   directory included), and an edit to a package the target does not import is ignored.
// - A build whose binary is byte-identical to the running one does not restart the server.
// - With --restart, a change to a file the server reads at startup (its config) restarts the
//   server with the binary it has, without a build.
//
// Linux only: it uses inotify for file events and signalfd for signals and child exits.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub(crate) const USAGE: &str = "usage: gocut watch --bin <path> --build <cmd> [--run <cmd>] [--pkg <pattern>]... \
[--restart <path>]... [--root <dir>] [--exclude <dir>]... [--delay <ms>] [--kill-delay <ms>]";

macro_rules! say {
    ($($t:tt)*) => { eprintln!("{} gocut watch: {}", clock(), format_args!($($t)*)) };
}

#[derive(Debug, PartialEq)]
pub(crate) struct Options {
    // The binary the build writes. Its directory is not watched.
    bin: PathBuf,
    // Shell command that builds `bin`.
    build: String,
    // Shell command that runs the server; `bin` when unset.
    run: Option<String>,
    // Main packages whose dependencies decide which edits start a build. Every .go file when empty.
    pkgs: Vec<String>,
    // Files, or directories of files, the server reads at startup: a change restarts it without a build.
    restart: Vec<PathBuf>,
    root: PathBuf,
    // Directory names or root-relative paths not to watch.
    excludes: Vec<String>,
    // Quiet period after the last relevant change before a build starts.
    delay: Duration,
    // How long a stopped server or build has after SIGINT before it is killed.
    kill_delay: Duration,
}

pub(crate) fn parse_options(args: &[String]) -> Result<Options> {
    let mut o = Options {
        bin: PathBuf::new(),
        build: String::new(),
        run: None,
        pkgs: Vec::new(),
        restart: Vec::new(),
        root: PathBuf::from("."),
        excludes: Vec::new(),
        delay: Duration::from_millis(300),
        kill_delay: Duration::from_secs(2),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a.as_str(), None),
        };
        let mut value = || -> Result<String> {
            match inline {
                Some(v) => Ok(v.to_string()),
                None => it.next().cloned().with_context(|| format!("{flag} needs a value")),
            }
        };
        match flag {
            "--bin" => o.bin = value()?.into(),
            "--build" => o.build = value()?,
            "--run" => o.run = Some(value()?),
            "--pkg" => o.pkgs.push(value()?),
            "--restart" => o.restart.push(value()?.into()),
            "--root" => o.root = value()?.into(),
            "--exclude" => o.excludes.push(value()?),
            "--delay" => o.delay = millis(flag, &value()?)?,
            "--kill-delay" => o.kill_delay = millis(flag, &value()?)?,
            _ => bail!("unknown argument {a}\n{USAGE}"),
        }
    }
    if o.bin.as_os_str().is_empty() || o.build.is_empty() {
        bail!("--bin and --build are required\n{USAGE}");
    }
    Ok(o)
}

fn millis(flag: &str, v: &str) -> Result<Duration> {
    v.parse().map(Duration::from_millis).with_context(|| format!("{flag} takes milliseconds, got {v:?}"))
}

// The files a target is built from, as `go list -deps` reports them.
#[derive(Default, Debug)]
struct Closure {
    // Package directories outside the standard library: any non-test .go file in one counts.
    dirs: HashSet<PathBuf>,
    // Non-Go files the build reads: embedded files, assembly, C, syso.
    files: HashSet<PathBuf>,
    // Directories holding embedded files, so a new file beside them counts too.
    embed_dirs: HashSet<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedPkg {
    dir: Option<PathBuf>,
    #[serde(default)]
    standard: bool,
    #[serde(default)]
    embed_files: Vec<String>,
    #[serde(default)]
    s_files: Vec<String>,
    #[serde(default)]
    h_files: Vec<String>,
    #[serde(default)]
    c_files: Vec<String>,
    #[serde(default, rename = "CXXFiles")]
    cxx_files: Vec<String>,
    #[serde(default)]
    syso_files: Vec<String>,
}

const LIST_FIELDS: &str = "-json=Dir,Standard,EmbedFiles,SFiles,HFiles,CFiles,CXXFiles,SysoFiles";

impl Closure {
    // Parses the concatenated JSON objects `go list -json` prints.
    fn parse(json: &[u8]) -> Result<Closure> {
        let mut c = Closure::default();
        for p in serde_json::Deserializer::from_slice(json).into_iter::<ListedPkg>() {
            let p = p.context("parsing go list output")?;
            let Some(dir) = p.dir.filter(|_| !p.standard) else {
                continue;
            };
            let dir = canonical(&dir);
            let other = p.s_files.iter().chain(&p.h_files).chain(&p.c_files).chain(&p.cxx_files).chain(&p.syso_files);
            c.files.extend(other.map(|f| dir.join(f)));
            for f in &p.embed_files {
                let path = dir.join(f);
                if let Some(parent) = path.parent() {
                    c.embed_dirs.insert(parent.to_path_buf());
                }
                c.files.insert(path);
            }
            c.dirs.insert(dir);
        }
        Ok(c)
    }

    fn list(pkgs: &[String], root: &Path) -> Result<Closure> {
        let out = Command::new("go")
            .args(["list", "-e", "-deps", LIST_FIELDS])
            .args(pkgs)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .context("running go list")?;
        if !out.status.success() {
            bail!("go list failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        Closure::parse(&out.stdout)
    }
}

// Whether a change to `path` can change the build. With no closure (no --pkg, or go list has not
// succeeded yet) every non-test .go file counts.
#[expect(clippy::case_sensitive_file_extension_comparisons, reason = "the go command matches .go case-sensitively")]
fn relevant(closure: Option<&Closure>, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if matches!(name, "go.mod" | "go.sum" | "go.work" | "go.work.sum") {
        return true;
    }
    // A file go list names is read by the build whatever its name (`//go:embed all:dir` keeps
    // `_` and `.` files).
    if closure.is_some_and(|c| c.files.contains(path)) {
        return true;
    }
    // The go command ignores these, and editors write their swap and backup files under them.
    if name.starts_with('.') || name.starts_with('_') || name.ends_with('~') {
        return false;
    }
    let go = name.ends_with(".go") && !name.ends_with("_test.go");
    let Some(c) = closure else { return go };
    let dir = path.parent();
    (go && dir.is_some_and(|d| c.dirs.contains(d))) || dir.is_some_and(|d| c.embed_dirs.contains(d))
}

// Directories the tree walk does not enter: hidden and underscore directories and testdata, which
// the go command ignores, node_modules, the directory the binary is written to, and --exclude.
fn skip_dir(root: &Path, bin: &Path, excludes: &[String], dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(OsStr::to_str) else {
        return true;
    };
    if name.starts_with('.') || name.starts_with('_') || matches!(name, "node_modules" | "testdata") {
        return true;
    }
    if bin.parent().is_some_and(|b| b == dir && b != root) {
        return true;
    }
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    excludes.iter().any(|e| e == name || Path::new(e.trim_start_matches("./")) == rel)
}

// What a change asks for. A build restarts the server too, so it outranks a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Work {
    Restart,
    Build,
}

fn classify(closure: Option<&Closure>, restart: &[PathBuf], path: &Path) -> Option<Work> {
    if relevant(closure, path) {
        return Some(Work::Build);
    }
    let name = path.file_name().and_then(OsStr::to_str)?;
    // Editors' swap and backup files.
    if name.starts_with('.') || name.ends_with('~') {
        return None;
    }
    restart.iter().any(|r| path.starts_with(r)).then_some(Work::Restart)
}

#[expect(clippy::case_sensitive_file_extension_comparisons, reason = "the go command matches .go case-sensitively")]
fn is_go_source(path: &Path) -> bool {
    path.file_name().and_then(OsStr::to_str).is_some_and(|n| n.ends_with(".go") && !n.ends_with("_test.go"))
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// Joins `p` onto `root` and drops `.` components, so paths compare equal to the ones a directory
// walk produces.
fn absolute(root: &Path, p: &Path) -> PathBuf {
    root.join(p).components().filter(|c| *c != Component::CurDir).collect()
}

const WATCH_MASK: u32 =
    libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM | libc::IN_DELETE | libc::IN_CREATE;

#[derive(Debug, PartialEq)]
struct RawEvent {
    wd: i32,
    mask: u32,
    name: Vec<u8>,
}

// Splits a buffer read from an inotify descriptor into events. Each is a 16-byte header (wd,
// mask, cookie, len) followed by a NUL-padded name of `len` bytes.
fn parse_events(buf: &[u8]) -> Vec<RawEvent> {
    const HDR: usize = std::mem::size_of::<libc::inotify_event>();
    let word = |i: usize| u32::from_ne_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    let mut out = Vec::new();
    let mut i = 0;
    while i + HDR <= buf.len() {
        let len = usize::try_from(word(i + 12)).expect("a u32 fits in usize on Linux");
        let (wd, mask) = (word(i).cast_signed(), word(i + 4));
        let raw = buf.get(i + HDR..i + HDR + len).unwrap_or_default();
        let name = raw.split(|b| *b == 0).next().unwrap_or_default().to_vec();
        out.push(RawEvent { wd, mask, name });
        i += HDR + len;
    }
    out
}

struct Inotify {
    fd: OwnedFd,
    dirs: HashMap<i32, PathBuf>,
    watched: HashSet<PathBuf>,
}

impl Inotify {
    fn new() -> Result<Inotify> {
        // SAFETY: inotify_init1 takes no pointers; the descriptor it returns is owned below.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify_init1");
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        Ok(Inotify { fd: unsafe { OwnedFd::from_raw_fd(fd) }, dirs: HashMap::new(), watched: HashSet::new() })
    }

    fn add(&mut self, dir: &Path) -> Result<()> {
        if self.watched.contains(dir) {
            return Ok(());
        }
        let c = CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: c is a valid NUL-terminated path for the duration of the call.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), WATCH_MASK | libc::IN_ONLYDIR) };
        if wd < 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                // Removed, or replaced by a file, since the walk saw it.
                Some(libc::ENOENT | libc::ENOTDIR) => Ok(()),
                Some(libc::ENOSPC) => {
                    bail!("out of inotify watches at {}: raise fs.inotify.max_user_watches", dir.display())
                }
                _ => Err(e).with_context(|| format!("watching {}", dir.display())),
            };
        }
        // A renamed directory keeps its inode and so its watch descriptor: drop the old path, so a
        // directory created there later is watched again.
        if let Some(old) = self.dirs.insert(wd, dir.to_path_buf())
            && old != dir
        {
            self.watched.remove(&old);
        }
        self.watched.insert(dir.to_path_buf());
        Ok(())
    }

    fn add_tree(&mut self, top: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
        let mut stack = vec![top.to_path_buf()];
        while let Some(d) = stack.pop() {
            self.add(&d)?;
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            // file_type does not follow symlinks, so a symlinked directory is not walked.
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) && !skip(&e.path()) {
                    stack.push(e.path());
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<Vec<RawEvent>> {
        let mut buf = vec![0u8; 64 << 10];
        let mut out = Vec::new();
        loop {
            // SAFETY: buf is writable for buf.len() bytes.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                break;
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                match e.kind() {
                    std::io::ErrorKind::WouldBlock => break,
                    std::io::ErrorKind::Interrupted => continue,
                    _ => return Err(e).context("reading inotify events"),
                }
            }
            out.extend(parse_events(&buf[..n.cast_unsigned()]));
        }
        Ok(out)
    }
}

// SIGINT, SIGTERM and SIGHUP stop the watcher; SIGCHLD reports a build or server exit. They are
// blocked and read from a signalfd, so the main loop waits on files and signals in one poll.
struct Signals {
    fd: OwnedFd,
}

const SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD];

impl Signals {
    // Must run before any thread starts, so no thread is left with these signals unblocked.
    fn block() -> Result<Signals> {
        let set = signal_set(&SIGNALS);
        // SAFETY: set is an initialised sigset_t, and a null old-set pointer is allowed.
        let r = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r)).context("blocking signals");
        }
        // SAFETY: set is an initialised sigset_t; -1 asks for a new descriptor.
        let fd = unsafe { libc::signalfd(-1, &raw const set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("signalfd");
        }
        // SAFETY: fd is a fresh descriptor nothing else owns.
        Ok(Signals { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    fn drain(&self) -> Vec<libc::c_int> {
        let mut out = Vec::new();
        loop {
            // SAFETY: signalfd_siginfo is plain integers, for which all-zero bytes are valid.
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::signalfd_siginfo>();
            // SAFETY: info is writable for size bytes.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), (&raw mut info).cast(), size) };
            if usize::try_from(n) != Ok(size) {
                return out;
            }
            out.push(info.ssi_signo.cast_signed());
        }
    }
}

// A sigset_t holding exactly `signals`.
fn signal_set(signals: &[libc::c_int]) -> libc::sigset_t {
    // SAFETY: sigset_t is plain data; sigemptyset below gives it a defined value.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: set is a valid, writable sigset_t.
    unsafe { libc::sigemptyset(&raw mut set) };
    for &s in signals {
        // SAFETY: set is a valid, writable sigset_t, and s is a valid signal number.
        unsafe { libc::sigaddset(&raw mut set, s) };
    }
    set
}

// Waits until either descriptor is readable or the timeout passes, and reports which are readable.
fn poll2(a: RawFd, b: RawFd, timeout: Option<Duration>) -> Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd { fd: a, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: b, events: libc::POLLIN, revents: 0 },
    ];
    // Rounded up, so a wait shorter than a millisecond does not spin.
    let ms = timeout.map_or(-1, |t| i32::try_from(t.as_micros().div_ceil(1000)).unwrap_or(i32::MAX));
    // SAFETY: fds is a valid array of two pollfd.
    let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok((false, false));
        }
        return Err(e).context("poll");
    }
    Ok((fds[0].revents & libc::POLLIN != 0, fds[1].revents & libc::POLLIN != 0))
}

// Runs in the child between fork and exec, so it makes only async-signal-safe calls. `watcher` is
// the watcher's pid.
fn child_setup(watcher: libc::pid_t) {
    // The watcher blocks the signals it reads from its signalfd; the child must not inherit that.
    let set = signal_set(&[]);
    // SAFETY: set is an initialised sigset_t, and a null old-set pointer is allowed.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &raw const set, std::ptr::null_mut()) };
    // If the watcher is killed outright, its direct children die with it.
    let sigkill = libc::c_ulong::from(libc::SIGKILL.cast_unsigned());
    // SAFETY: PR_SET_PDEATHSIG takes a signal number and no pointers.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, sigkill) };
    // The watcher may have died before prctl ran, and then the signal never comes.
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != watcher {
        // SAFETY: _exit is async-signal-safe and ends the child before it runs the command.
        unsafe { libc::_exit(1) };
    }
}

// A shell command running in its own process group, whose id is the shell's pid.
struct Proc {
    child: Child,
    reaped: bool,
}

impl Proc {
    // Must be called from the main thread: PR_SET_PDEATHSIG fires when the spawning thread exits.
    fn spawn(cmd: &str, dir: &Path) -> Result<Proc> {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(cmd).current_dir(dir).stdin(Stdio::null()).process_group(0);
        let watcher = std::process::id().cast_signed();
        // SAFETY: child_setup only makes async-signal-safe calls.
        unsafe {
            c.pre_exec(move || {
                child_setup(watcher);
                Ok(())
            })
        };
        let child = c.spawn().with_context(|| format!("starting {cmd}"))?;
        Ok(Proc { child, reaped: false })
    }

    fn pgid(&self) -> libc::pid_t {
        self.child.id().cast_signed()
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: plain syscall. The leader is not reaped yet (reaped is set after the last
        // signal), so the group id cannot have been reused.
        unsafe { libc::kill(-self.pgid(), sig) };
    }

    // Whether the group's leader has exited. WNOWAIT leaves it a zombie, which keeps its pid and
    // process group id reserved until reap.
    fn exited(&self) -> bool {
        // SAFETY: siginfo_t is plain data, for which all-zero bytes are valid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        // SAFETY: info is a valid, writable siginfo_t.
        let r = unsafe { libc::waitid(libc::P_PID, self.child.id(), &raw mut info, flags) };
        // SAFETY: waitid succeeded, so info holds a SIGCHLD record (or zeroes under WNOHANG).
        r == 0 && unsafe { info.si_pid() } != 0
    }

    // Kills whatever is left of the group, then reaps the leader.
    fn reap(&mut self) -> Option<ExitStatus> {
        self.signal(libc::SIGKILL);
        self.reaped = true;
        self.child.wait().ok()
    }

    // Interrupts the group, gives the leader `grace` to exit, then kills the group.
    fn stop(&mut self, grace: Duration) -> Option<ExitStatus> {
        self.signal(libc::SIGINT);
        let deadline = Instant::now() + grace;
        while !self.exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.reap()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.reaped {
            self.reap();
        }
    }
}

// The last content hash seen for each changed file, so a save that leaves a file as it was does
// not start a build. A file seen for the first time counts as changed.
#[derive(Default)]
struct Hashes(HashMap<PathBuf, Option<blake3::Hash>>);

impl Hashes {
    fn changed(&mut self, path: &Path) -> bool {
        let now = hash_file(path);
        self.0.insert(path.to_path_buf(), now) != Some(now)
    }
}

// The content hash of a regular file. Anything else counts as absent: opening a FIFO for reading
// would block the watcher until something writes to it.
fn hash_file(path: &Path) -> Option<blake3::Hash> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let f = std::fs::File::open(path).ok()?;
    let mut h = blake3::Hasher::new();
    h.update_reader(f).ok()?;
    Some(h.finalize())
}

// Whether a --restart file changed since the server last started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Config {
    #[default]
    Unchanged,
    Changed,
}

// Whether a successful build needs the server (re)started. `running` is the hash of the binary
// the running server was started from, None when no server runs. It restarts when no server
// runs, a --restart file changed while the build was due, or the binary differs.
fn restart_needed(built: Option<blake3::Hash>, running: Option<blake3::Hash>, config: Config) -> bool {
    config == Config::Changed || built.is_none() || built != running
}

struct Watcher {
    o: Options,
    root: PathBuf,
    bin: PathBuf,
    run: String,
    modcache: Option<PathBuf>,
    gowork: Option<PathBuf>,
    ino: Inotify,
    closure: Option<Closure>,
    hashes: Hashes,
    build: Option<Proc>,
    build_started: Instant,
    server: Option<Proc>,
    // Hash of the binary the running server was started from.
    served: Option<blake3::Hash>,
    // Whether the last build succeeded, so `bin` is current.
    bin_current: bool,
    // A --restart file changed while a build was due or running; that build restarts the server.
    config: Config,
    // Source files ignored while a build was due or running, because the package list predates
    // it. The build may add their package, so they are classified again after the next relist.
    deferred: HashSet<PathBuf>,
    restart: Vec<PathBuf>,
    // The next piece of work: when it may start (the last change plus --delay), what it is and why.
    due: Option<(Instant, Work, String)>,
}

pub(crate) fn run(o: Options) -> Result<ExitCode> {
    let signals = Signals::block()?;
    let root = canonical(&o.root);
    if !root.is_dir() {
        bail!("--root {} is not a directory", o.root.display());
    }
    let bin = absolute(&root, &o.bin);
    let run = o.run.clone().unwrap_or_else(|| bin.display().to_string());
    let restart: Vec<PathBuf> = o.restart.iter().map(|p| absolute(&root, p)).collect();
    let (modcache, gowork) = go_env(&root);
    let mut w = Watcher {
        o,
        root,
        bin,
        run,
        modcache,
        gowork,
        ino: Inotify::new()?,
        closure: None,
        hashes: Hashes::default(),
        build: None,
        build_started: Instant::now(),
        server: None,
        served: None,
        bin_current: false,
        config: Config::Unchanged,
        deferred: HashSet::new(),
        restart,
        due: Some((Instant::now(), Work::Build, "starting".into())),
    };
    w.watch_all()?;
    w.relist();
    say!("watching {} directories", w.ino.watched.len());
    let result = w.serve(&signals);
    if result.is_err() {
        // Interrupt rather than leave Proc's drop to SIGKILL the server.
        w.shutdown();
    }
    result
}

// GOMODCACHE, whose packages never change, and GOWORK, whose go.work counts as a module file.
fn go_env(root: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let out = Command::new("go").args(["env", "GOMODCACHE", "GOWORK"]).current_dir(root).stdin(Stdio::null()).output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().map(|l| Some(l.trim()).filter(|l| !l.is_empty() && *l != "off").map(PathBuf::from));
    (lines.next().flatten(), lines.next().flatten())
}

impl Watcher {
    fn serve(&mut self, signals: &Signals) -> Result<ExitCode> {
        loop {
            if self.build.is_none()
                && self.due.as_ref().is_some_and(|(t, _, _)| *t <= Instant::now())
                && let Some((_, work, why)) = self.due.take()
            {
                match work {
                    Work::Build => self.start_build(&why),
                    Work::Restart => self.restart_server(&why),
                }
            }
            // While a build runs, its exit (SIGCHLD) or a file change is what moves things on.
            let timeout = match (&self.build, &self.due) {
                (None, Some((t, _, _))) => Some(t.saturating_duration_since(Instant::now())),
                _ => None,
            };
            let (files, sigs) = poll2(self.ino.fd.as_raw_fd(), signals.fd.as_raw_fd(), timeout)?;
            if sigs {
                for s in signals.drain() {
                    if s == libc::SIGCHLD {
                        self.reap_children();
                    } else {
                        self.shutdown();
                        return Ok(ExitCode::SUCCESS);
                    }
                }
            }
            if files {
                self.on_file_events()?;
            }
        }
    }

    fn rel<'a>(&self, p: &'a Path) -> std::borrow::Cow<'a, str> {
        p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy()
    }

    fn skip_dir(&self, dir: &Path) -> bool {
        skip_dir(&self.root, &self.bin, &self.o.excludes, dir)
    }

    // Watches the root, the --restart paths and go.work's directory. Adding a watch that exists is
    // a no-op, so this also repairs the watches after the event queue overflowed.
    fn watch_all(&mut self) -> Result<()> {
        let (root, bin, excludes) = (&self.root, &self.bin, &self.o.excludes);
        let skip = |d: &Path| skip_dir(root, bin, excludes, d);
        self.ino.add_tree(root, &skip)?;
        // A --restart path outside the root, or in a directory the walk skips, is watched on its own.
        for p in &self.restart {
            if p.is_dir() {
                self.ino.add_tree(p, &skip)?;
            } else if let Some(dir) = p.parent() {
                self.ino.add(dir)?;
            }
        }
        if let Some(dir) = self.gowork.as_deref().and_then(Path::parent) {
            self.ino.add(dir)?;
        }
        Ok(())
    }

    fn watch_tree(&mut self, top: &Path) -> Result<()> {
        let (root, bin, excludes) = (&self.root, &self.bin, &self.o.excludes);
        self.ino.add_tree(top, &|d| skip_dir(root, bin, excludes, d))
    }

    // Reruns go list for --pkg and watches every package directory it reports, including ones
    // outside the root (a `replace` to a sibling module) and ones the tree walk excluded.
    fn relist(&mut self) {
        if self.o.pkgs.is_empty() {
            return;
        }
        let t = Instant::now();
        let c = match Closure::list(&self.o.pkgs, &self.root) {
            Ok(c) => c,
            Err(e) => {
                let fallback = if self.closure.is_some() { "the previous package list" } else { "every .go file" };
                say!("{e:#}; using {fallback}");
                return;
            }
        };
        let fixed = |d: &&PathBuf| !self.modcache.as_ref().is_some_and(|m| d.starts_with(m));
        let dirs: Vec<PathBuf> = c.dirs.iter().chain(&c.embed_dirs).filter(fixed).cloned().collect();
        for d in &dirs {
            if let Err(e) = self.ino.add(d) {
                say!("{e:#}");
            }
        }
        if self.closure.is_none() {
            say!("{} packages from {} ({:.1}s)", c.dirs.len(), self.o.pkgs.join(" "), t.elapsed().as_secs_f64());
        }
        self.closure = Some(c);
    }

    fn on_file_events(&mut self) -> Result<()> {
        let mut changed: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut new_dirs = Vec::new();
        let mut overflow = false;
        for ev in self.ino.drain()? {
            if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                overflow = true;
                continue;
            }
            if ev.mask & libc::IN_IGNORED != 0 {
                if let Some(d) = self.ino.dirs.remove(&ev.wd) {
                    self.ino.watched.remove(&d);
                }
                continue;
            }
            let Some(dir) = self.ino.dirs.get(&ev.wd) else {
                continue;
            };
            let path = dir.join(OsStr::from_bytes(&ev.name));
            if ev.mask & libc::IN_ISDIR != 0 {
                let ours = path.starts_with(&self.root) || self.restart.iter().any(|r| path.starts_with(r));
                if ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 && ours {
                    new_dirs.push(path);
                }
                continue;
            }
            // A created file's contents arrive with IN_CLOSE_WRITE.
            if ev.mask & libc::IN_CREATE == 0 && seen.insert(path.clone()) {
                changed.push(path);
            }
        }
        // A watch that cannot be added (out of inotify watches, say) is reported, and the
        // watcher keeps serving with the ones it has. After an overflow the lost events may have
        // created directories, so everything is walked again.
        if overflow && let Err(e) = self.watch_all() {
            say!("{e:#}");
        }
        for d in new_dirs {
            if !self.skip_dir(&d)
                && let Err(e) = self.watch_tree(&d)
            {
                say!("{e:#}");
            }
        }
        self.consider(changed, overflow.then(|| "too many changes at once (inotify queue overflowed)".to_string()));
        Ok(())
    }

    // Classifies changed files and schedules what they ask for: a build, a restart, or nothing.
    // `forced` is a reason to build whatever the files are.
    fn consider(&mut self, paths: Vec<PathBuf>, forced: Option<String>) {
        let mut todo = forced.map(|why| (Work::Build, why));
        let mut config = false;
        let mut ignored = Vec::new();
        for path in paths {
            let Some(work) = classify(self.closure.as_ref(), &self.restart, &path) else {
                if is_go_source(&path) {
                    ignored.push(path);
                }
                continue;
            };
            if !self.hashes.changed(&path) {
                continue;
            }
            config |= work == Work::Restart;
            if todo.as_ref().is_none_or(|(w, _)| work > *w) {
                todo = Some((work, format!("{} changed", self.rel(&path))));
            }
        }

        let due_work = self.due.as_ref().map(|d| d.1);
        let build_ahead =
            self.build.is_some() || due_work == Some(Work::Build) || todo.as_ref().is_some_and(|t| t.0 == Work::Build);
        for p in ignored {
            say!("ignoring {}: not part of {}", self.rel(&p), self.o.pkgs.join(" "));
            if build_ahead {
                self.deferred.insert(p);
            }
        }

        let Some((work, reason)) = todo else { return };
        let at = Instant::now() + self.o.delay;
        match work {
            Work::Build => {
                // The build restarts the server; make it do so even when its binary is unchanged
                // if a --restart file changed in this batch or a restart for one was pending.
                if config || due_work == Some(Work::Restart) {
                    self.config = Config::Changed;
                }
                if let Some(mut b) = self.build.take() {
                    say!("{reason}; stopping the running build");
                    b.stop(self.o.kill_delay);
                }
                self.due = Some((at, Work::Build, reason));
            }
            Work::Restart if build_ahead => {
                say!("{reason}; the next build restarts the server");
                self.config = Config::Changed;
            }
            Work::Restart => self.due = Some((at, Work::Restart, reason)),
        }
    }

    // Restarts the server on the binary the last build wrote, which a failed build leaves stale.
    fn restart_server(&mut self, why: &str) {
        if !self.bin_current {
            say!("{why}; not restarting the server: the last build failed");
            return;
        }
        if let Some(mut s) = self.server.take() {
            s.stop(self.o.kill_delay);
        }
        self.start_server(why, hash_file(&self.bin));
    }

    fn start_server(&mut self, why: &str, hash: Option<blake3::Hash>) {
        match Proc::spawn(&self.run, &self.root) {
            Ok(p) => {
                say!("{why}; server started");
                self.server = Some(p);
                self.served = hash;
            }
            Err(e) => say!("{why}; {e:#}"),
        }
    }

    fn start_build(&mut self, why: &str) {
        say!("{why}; building");
        self.build_started = Instant::now();
        match Proc::spawn(&self.o.build, &self.root) {
            Ok(p) => self.build = Some(p),
            Err(e) => say!("{e:#}"),
        }
    }

    fn reap_children(&mut self) {
        if self.build.as_ref().is_some_and(Proc::exited)
            && let Some(mut b) = self.build.take()
        {
            let status = b.reap();
            self.finish_build(status);
        }
        if self.server.as_ref().is_some_and(Proc::exited)
            && let Some(mut s) = self.server.take()
        {
            let status = s.reap();
            say!("server exited ({})", status.map_or("unknown status".into(), |s| s.to_string()));
            self.served = None;
        }
    }

    fn finish_build(&mut self, status: Option<ExitStatus>) {
        let took = self.build_started.elapsed().as_secs_f64();
        let config = std::mem::take(&mut self.config);
        self.bin_current = status.is_some_and(|s| s.success());
        if !self.bin_current {
            let status = status.map_or("unknown status".into(), |s| s.to_string());
            if let Some(mut s) = self.server.take() {
                say!("build failed after {took:.1}s ({status}); stopping the server");
                s.stop(self.o.kill_delay);
                self.served = None;
            } else {
                say!("build failed after {took:.1}s ({status})");
            }
            return;
        }
        let built = hash_file(&self.bin);
        let running = self.server.as_ref().and(self.served);
        if restart_needed(built, running, config) {
            if let Some(mut s) = self.server.take() {
                s.stop(self.o.kill_delay);
            }
            self.start_server(&format!("built in {took:.1}s"), built);
        } else {
            say!("built in {took:.1}s; binary unchanged, server kept running");
        }
        // Imports may have changed; the package list goes stale only after a successful build.
        self.relist();
        // Files ignored while this build was due may belong to a package it added.
        let deferred: Vec<PathBuf> = std::mem::take(&mut self.deferred).into_iter().collect();
        if !deferred.is_empty() {
            self.consider(deferred, None);
        }
    }

    fn shutdown(&mut self) {
        if let Some(mut b) = self.build.take() {
            say!("stopping the build");
            b.stop(self.o.kill_delay);
        }
        if let Some(mut s) = self.server.take() {
            say!("stopping the server");
            s.stop(self.o.kill_delay);
        }
    }
}

fn clock() -> String {
    // SAFETY: a null pointer asks time for the return value only.
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: tm is plain data, for which all-zero bytes are valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: t and tm are valid for reading and writing respectively.
    unsafe { libc::localtime_r(&raw const t, &raw mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(ToString::to_string).collect()
    }

    // A fresh directory under the system temp dir, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let d = std::env::temp_dir().join(format!("gocut-watch-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn options_accept_both_flag_forms_and_default_the_rest() {
        let o = parse_options(&args(&[
            "--bin",
            "./tmp/app",
            "--build=go build -gcflags='example.com/...=-N -l' -o ./tmp/app ./cmd/app",
            "--pkg",
            "./cmd/app",
            "--pkg=./cmd/other",
            "--restart",
            "config",
            "--restart=extra.yaml",
            "--exclude",
            "ui",
            "--delay",
            "50",
        ]))
        .unwrap();
        assert_eq!(
            o,
            Options {
                bin: PathBuf::from("./tmp/app"),
                build: "go build -gcflags='example.com/...=-N -l' -o ./tmp/app ./cmd/app".into(),
                run: None,
                pkgs: vec!["./cmd/app".into(), "./cmd/other".into()],
                restart: vec![PathBuf::from("config"), PathBuf::from("extra.yaml")],
                root: PathBuf::from("."),
                excludes: vec!["ui".into()],
                delay: Duration::from_millis(50),
                kill_delay: Duration::from_secs(2),
            }
        );
    }

    #[test]
    fn options_reject_missing_required_flags_unknown_flags_and_bad_durations() {
        let err = |a: &[&str]| parse_options(&args(a)).unwrap_err().to_string();
        assert!(err(&["--bin", "x"]).starts_with("--bin and --build are required"));
        assert!(err(&["--bin", "x", "--build", "y", "--poll"]).starts_with("unknown argument --poll"));
        assert_eq!(err(&["--bin", "x", "--build", "y", "--delay", "2s"]), "--delay takes milliseconds, got \"2s\"");
        assert_eq!(err(&["--bin"]), "--bin needs a value");
    }

    // `go list -e -deps -json=<fields>` prints one object per package with only the requested
    // fields, omitting empty ones: a standard package, a package that failed to load (no Dir),
    // a module package with embedded and assembly files, and one reached through a `replace`.
    const LISTED: &str = r#"{
	"Dir": "/nonexistent-gocut/goroot/src/fmt",
	"Standard": true
}
{
}
{
	"Dir": "/nonexistent-gocut/mod/internal/web",
	"EmbedFiles": [
		"templates/page.tmpl",
		"static/app.css",
		"static/_redirects"
	],
	"SFiles": [
		"sum_amd64.s"
	]
}
{
	"Dir": "/nonexistent-gocut/shared/money"
}
"#;

    #[test]
    fn closure_counts_only_files_the_listed_packages_are_built_from() {
        let c = Closure::parse(LISTED.as_bytes()).unwrap();
        let r = |p: &str| relevant(Some(&c), Path::new(p));
        assert!(r("/nonexistent-gocut/mod/internal/web/handler.go"));
        assert!(r("/nonexistent-gocut/mod/internal/web/new_file.go"));
        assert!(r("/nonexistent-gocut/shared/money/money.go"));
        assert!(r("/nonexistent-gocut/mod/internal/web/sum_amd64.s"));
        assert!(r("/nonexistent-gocut/mod/internal/web/templates/page.tmpl"));
        assert!(r("/nonexistent-gocut/mod/internal/web/templates/added.tmpl"));
        assert!(r("/nonexistent-gocut/mod/internal/web/static/app.css"));
        // `//go:embed all:static` keeps `_` and `.` files; go list names them.
        assert!(r("/nonexistent-gocut/mod/internal/web/static/_redirects"));
        assert!(!r("/nonexistent-gocut/mod/internal/web/static/_unlisted"));
        assert!(r("/anywhere/go.mod"));
        assert!(r("/anywhere/go.work.sum"));

        assert!(!r("/nonexistent-gocut/mod/internal/web/handler_test.go"));
        assert!(!r("/nonexistent-gocut/mod/internal/web/README.md"));
        assert!(!r("/nonexistent-gocut/mod/internal/web/.handler.go.swp"));
        assert!(!r("/nonexistent-gocut/mod/internal/web/_scratch.go"));
        assert!(!r("/nonexistent-gocut/mod/internal/web/templates/.page.tmpl.swp"));
        assert!(!r("/nonexistent-gocut/mod/internal/web/templates/page.tmpl~"));
        assert!(!r("/nonexistent-gocut/mod/internal/worker/job.go"));
        assert!(!r("/nonexistent-gocut/goroot/src/fmt/print.go"));
        assert_eq!(c.dirs.len(), 2);
    }

    #[test]
    fn restart_paths_restart_and_build_inputs_build_even_inside_them() {
        let c = Closure::parse(LISTED.as_bytes()).unwrap();
        let restart =
            [PathBuf::from("/nonexistent-gocut/mod/config"), PathBuf::from("/nonexistent-gocut/mod/extra.yaml")];
        let k = |p: &str| classify(Some(&c), &restart, Path::new(p));
        assert_eq!(k("/nonexistent-gocut/mod/config/config.yaml"), Some(Work::Restart));
        assert_eq!(k("/nonexistent-gocut/mod/config/local/override.yaml"), Some(Work::Restart));
        assert_eq!(k("/nonexistent-gocut/mod/extra.yaml"), Some(Work::Restart));
        assert_eq!(k("/nonexistent-gocut/mod/internal/web/handler.go"), Some(Work::Build));
        assert_eq!(k("/nonexistent-gocut/mod/config/go.mod"), Some(Work::Build));

        assert_eq!(k("/nonexistent-gocut/mod/config/.config.yaml.swp"), None);
        assert_eq!(k("/nonexistent-gocut/mod/config/config.yaml~"), None);
        // Paths match by component, not by string prefix.
        assert_eq!(k("/nonexistent-gocut/mod/configs/config.yaml"), None);
        assert_eq!(k("/nonexistent-gocut/mod/extra.yaml.bak"), None);
        assert_eq!(k("/nonexistent-gocut/mod/README.md"), None);
        assert!(Work::Build > Work::Restart);
    }

    #[test]
    fn without_a_closure_every_non_test_go_file_counts() {
        assert!(relevant(None, Path::new("/m/a/x.go")));
        assert!(relevant(None, Path::new("/m/go.sum")));
        assert!(!relevant(None, Path::new("/m/a/x_test.go")));
        assert!(!relevant(None, Path::new("/m/a/page.tmpl")));
    }

    #[test]
    fn skipped_directories() {
        let root = Path::new("/r");
        let bin = Path::new("/r/tmp/app");
        let excludes = vec!["ui".to_string(), "./internal/gen".to_string()];
        let skip = |d: &str| skip_dir(root, bin, &excludes, Path::new(d));
        assert!(skip("/r/tmp"));
        assert!(skip("/r/.git"));
        assert!(skip("/r/_old"));
        assert!(skip("/r/pkg/testdata"));
        assert!(skip("/r/web/node_modules"));
        assert!(skip("/r/ui"));
        assert!(skip("/r/internal/gen"));
        assert!(!skip("/r/internal/gen2"));
        assert!(!skip("/r/internal/tmp"));
        assert!(!skip("/r/cmd/app"));
        // A binary written to the root does not exclude the root.
        assert!(!skip_dir(root, Path::new("/r/app"), &[], root));
    }

    fn raw(wd: i32, mask: u32, name: &[u8], len: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(wd.to_ne_bytes());
        b.extend(mask.to_ne_bytes());
        b.extend(0u32.to_ne_bytes());
        b.extend(u32::try_from(len).unwrap().to_ne_bytes());
        let mut n = name.to_vec();
        n.resize(len, 0);
        b.extend(n);
        b
    }

    #[test]
    fn events_are_split_at_their_declared_lengths_and_names_end_at_the_first_nul() {
        let mut buf = raw(1, libc::IN_CLOSE_WRITE, b"main.go", 16);
        buf.extend(raw(2, libc::IN_CREATE | libc::IN_ISDIR, b"newpkg", 32));
        buf.extend(raw(3, libc::IN_IGNORED, b"", 0));
        // A partial header at the end is not an event.
        buf.extend([7u8; 9]);
        assert_eq!(
            parse_events(&buf),
            vec![
                RawEvent { wd: 1, mask: libc::IN_CLOSE_WRITE, name: b"main.go".to_vec() },
                RawEvent { wd: 2, mask: libc::IN_CREATE | libc::IN_ISDIR, name: b"newpkg".to_vec() },
                RawEvent { wd: 3, mask: libc::IN_IGNORED, name: Vec::new() },
            ]
        );
    }

    #[test]
    fn a_file_counts_as_changed_only_when_its_contents_differ_from_the_last_seen() {
        let dir = TempDir::new("hashes");
        let f = dir.0.join("a.go");
        let mut h = Hashes::default();
        std::fs::write(&f, "package a").unwrap();
        assert!(h.changed(&f), "first sight");
        std::fs::write(&f, "package a").unwrap();
        assert!(!h.changed(&f), "same bytes saved again");
        std::fs::write(&f, "package a // x").unwrap();
        assert!(h.changed(&f), "new bytes");
        std::fs::remove_file(&f).unwrap();
        assert!(h.changed(&f), "deleted");
        assert!(!h.changed(&f), "still deleted");
        assert!(h.changed(&dir.0.join("never-seen.go")), "a missing file seen for the first time");
    }

    #[test]
    fn a_fifo_hashes_as_absent_without_blocking() {
        let dir = TempDir::new("fifo");
        let fifo = dir.0.join("pipe.yaml");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: c is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Opening the FIFO for reading would block here until a writer appeared.
        assert_eq!(hash_file(&fifo), None);
        assert!(
            Hashes::default().changed(&fifo),
            "a FIFO seen for the first time counts as changed, like a missing file"
        );
    }

    #[test]
    fn server_restarts_unless_it_runs_an_identical_binary() {
        let a = Some(blake3::hash(b"a"));
        let b = Some(blake3::hash(b"b"));
        assert!(!restart_needed(a, a, Config::Unchanged));
        assert!(restart_needed(b, a, Config::Unchanged));
        assert!(restart_needed(a, None, Config::Unchanged), "no server runs");
        assert!(restart_needed(None, a, Config::Unchanged), "the binary could not be hashed");
        // A config file changed while the build was due: the server must reread it.
        assert!(restart_needed(a, a, Config::Changed));
    }

    fn gone(pid: i32) -> bool {
        // A zombie has been killed; it is only waiting for init to reap it.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(s) => s.rsplit_once(") ").is_some_and(|(_, rest)| rest.starts_with('Z')),
        }
    }

    fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        ok()
    }

    #[test]
    fn stopping_a_proc_kills_its_whole_process_group() {
        let dir = TempDir::new("group");
        let pidfile = dir.0.join("pid");
        // The shell starts a grandchild that, like a compile under go build, is not the leader.
        let mut p = Proc::spawn(&format!("sleep 60 & echo $! > '{}'; wait", pidfile.display()), &dir.0).unwrap();
        let mut grandchild = 0;
        assert!(wait_until(Duration::from_secs(5), || {
            grandchild = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            grandchild > 0
        }));
        assert!(!gone(grandchild));

        let t = Instant::now();
        let status = p.stop(Duration::from_secs(5)).unwrap();
        assert!(
            t.elapsed() < Duration::from_secs(4),
            "the leader exits on SIGINT without waiting out the grace period"
        );
        assert!(!status.success());
        assert!(wait_until(Duration::from_secs(2), || gone(grandchild)), "grandchild {grandchild} outlived its group");
    }

    #[test]
    fn a_group_that_ignores_sigint_is_killed_after_the_grace_period() {
        let mut p = Proc::spawn("trap '' INT; sleep 60; true", Path::new("/")).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        let status = p.stop(Duration::from_millis(300)).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(300));
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn commands_run_in_the_directory_given() {
        let dir = TempDir::new("cwd");
        let mut p = Proc::spawn("pwd > where", &dir.0).unwrap();
        assert!(wait_until(Duration::from_secs(5), || p.exited()));
        assert!(p.reap().is_some_and(|s| s.success()));
        let got = std::fs::read_to_string(dir.0.join("where")).unwrap();
        assert_eq!(canonical(Path::new(got.trim())), canonical(&dir.0));
    }

    #[test]
    fn a_path_a_watched_directory_moved_away_from_can_be_watched_again() {
        let dir = TempDir::new("rename");
        let (a, b) = (dir.0.join("a"), dir.0.join("b"));
        std::fs::create_dir(&a).unwrap();
        let mut ino = Inotify::new().unwrap();
        ino.add(&a).unwrap();
        std::fs::rename(&a, &b).unwrap();
        // The tree walk after IN_MOVED_TO re-adds the directory under its new path.
        ino.add(&b).unwrap();
        std::fs::create_dir(&a).unwrap();
        ino.add(&a).unwrap();
        assert!(ino.watched.contains(&a) && ino.watched.contains(&b));
        assert_eq!(ino.dirs.values().filter(|d| **d == a).count(), 1, "the new {} has a watch", a.display());
    }

    // A watcher over `root` with no processes running, as `run` builds it.
    fn watcher(root: &Path, restart: &[&str], closure: Closure) -> Watcher {
        let mut args = vec![
            "--bin".to_string(),
            "tmp/app".into(),
            "--build".into(),
            "true".into(),
            "--pkg".into(),
            "./cmd/app".into(),
        ];
        for r in restart {
            args.extend(["--restart".to_string(), r.to_string()]);
        }
        let o = parse_options(&args).unwrap();
        Watcher {
            restart: o.restart.iter().map(|p| absolute(root, p)).collect(),
            bin: absolute(root, &o.bin),
            run: "true".into(),
            o,
            root: root.to_path_buf(),
            modcache: None,
            gowork: None,
            ino: Inotify::new().unwrap(),
            closure: Some(closure),
            hashes: Hashes::default(),
            build: None,
            build_started: Instant::now(),
            server: None,
            served: None,
            bin_current: true,
            config: Config::Unchanged,
            deferred: HashSet::new(),
            due: None,
        }
    }

    // A module root with config/config.yaml and an app package, and the closure go list gives it.
    fn module(name: &str) -> (TempDir, Closure) {
        let dir = TempDir::new(name);
        for d in ["config", "cmd/app", "internal/newpkg"] {
            std::fs::create_dir_all(dir.0.join(d)).unwrap();
        }
        std::fs::write(dir.0.join("config/config.yaml"), "port: 1").unwrap();
        std::fs::write(dir.0.join("cmd/app/main.go"), "package main").unwrap();
        std::fs::write(dir.0.join("internal/newpkg/x.go"), "package newpkg").unwrap();
        let closure = Closure { dirs: HashSet::from([dir.0.join("cmd/app")]), ..Closure::default() };
        (dir, closure)
    }

    #[test]
    fn a_build_that_replaces_a_pending_restart_still_restarts_for_the_config() {
        let (dir, closure) = module("supersede");
        let mut w = watcher(&dir.0, &["config"], closure);
        w.consider(vec![dir.0.join("config/config.yaml")], None);
        assert_eq!(w.due.as_ref().map(|d| d.1), Some(Work::Restart));
        assert_eq!(w.config, Config::Unchanged);

        w.consider(vec![dir.0.join("cmd/app/main.go")], None);
        assert_eq!(w.due.as_ref().map(|d| d.1), Some(Work::Build));
        assert_eq!(w.config, Config::Changed, "the build must restart the server even if its binary is unchanged");
    }

    #[test]
    fn a_config_change_batched_with_a_source_change_still_restarts_for_the_config() {
        let (dir, closure) = module("batch");
        let mut w = watcher(&dir.0, &["config"], closure);
        w.consider(vec![dir.0.join("config/config.yaml"), dir.0.join("cmd/app/main.go")], None);
        assert_eq!(w.due.as_ref().map(|d| d.1), Some(Work::Build));
        assert_eq!(w.config, Config::Changed);
    }

    #[test]
    fn a_source_change_alone_does_not_force_a_restart() {
        let (dir, closure) = module("plain");
        let mut w = watcher(&dir.0, &["config"], closure);
        w.consider(vec![dir.0.join("cmd/app/main.go")], None);
        assert_eq!(w.due.as_ref().map(|d| d.1), Some(Work::Build));
        assert_eq!(w.config, Config::Unchanged);
    }

    #[test]
    fn a_file_ignored_while_a_build_is_due_is_rebuilt_once_the_build_adds_its_package() {
        let (dir, closure) = module("deferred");
        let mut w = watcher(&dir.0, &[], closure);
        let newfile = dir.0.join("internal/newpkg/x.go");
        // main.go gains an import of newpkg, and newpkg is edited before the build lists it.
        w.consider(vec![dir.0.join("cmd/app/main.go"), newfile.clone()], None);
        assert_eq!(w.deferred, HashSet::from([newfile]));

        // The build runs, then go list reports newpkg; finish_build reconsiders the deferred file.
        w.due = None;
        w.closure.as_mut().unwrap().dirs.insert(dir.0.join("internal/newpkg"));
        let deferred: Vec<PathBuf> = std::mem::take(&mut w.deferred).into_iter().collect();
        w.consider(deferred, None);
        assert_eq!(w.due.as_ref().map(|d| (d.1, d.2.as_str())), Some((Work::Build, "internal/newpkg/x.go changed")));
    }

    #[test]
    fn a_file_outside_the_package_list_is_not_deferred_when_no_build_is_ahead() {
        let (dir, closure) = module("idle");
        let mut w = watcher(&dir.0, &[], closure);
        w.consider(vec![dir.0.join("internal/newpkg/x.go")], None);
        assert!(w.due.is_none());
        assert!(w.deferred.is_empty());
    }
}
