//! Selected-file previews. Invoked as a one-shot process, independently of FFF.
//! Resource limits isolate renderer failures; this is not a security sandbox.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, TempDir};
use url::Url;

const MAX_FILE: u64 = 128 * 1024 * 1024;
const MAX_PNG: u64 = 16 * 1024 * 1024;
const MAX_FACETS: u32 = 2_000_000;
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const CANCELLED: &str = "Preview cancelled";
static CANCEL: AtomicBool = AtomicBool::new(false);
type Result<T> = std::result::Result<T, String>;

extern "C" fn cancelled(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

fn program(name: &str, override_var: &str) -> Option<PathBuf> {
    let value = env::var_os(override_var)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let config = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))?;
            let file = File::open(config.join("oma-cast/config.json")).ok()?;
            let value: Value = serde_json::from_reader(file.take(65536)).ok()?;
            let key = if name == "f3d" {
                "previewF3d"
            } else {
                "previewPdfRenderer"
            };
            value
                .get(key)?
                .as_str()
                .filter(|s| !s.is_empty())
                .map(std::ffi::OsString::from)
        })
        .unwrap_or_else(|| name.into());
    let candidate = PathBuf::from(value);
    let executable = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if candidate.is_absolute() {
        return executable(&candidate).then_some(candidate);
    }
    // Overrides are an executable name or an absolute path, never shell code.
    if candidate.components().count() != 1 {
        return None;
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .filter(|p| p.is_absolute())
        .map(|p| p.join(&candidate))
        .find(|p| executable(p))
}

/// Returns true when a preview-specific invocation has been handled.
pub fn dispatch() -> bool {
    let mut args = env::args_os().skip(1);
    let Some(mode) = args.next() else {
        return false;
    };
    if mode == "--preview-capabilities" {
        println!(
            "{}",
            json!({"previewProtocol":1,"images":true,"text":true,
            "f3d":program("f3d","OMA_CAST_F3D_BIN").is_some(),
            "pdf":program("pdftoppm","OMA_CAST_PDFTOPPM_BIN").is_some()})
        );
        return true;
    }
    if mode != "--preview" {
        return false;
    }
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = cancelled as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    let path = args.next().map(PathBuf::from);
    let answer = match path.as_deref() {
        Some(p) if args.next().is_none() => match preview(p) {
            Ok(v) => v,
            Err(e) => json!({"path":p.to_string_lossy(),"message":e,"error":true}),
        },
        _ => json!({"message":"Usage: oma-cast-files --preview <absolute-path>","error":true}),
    };
    if !CANCEL.load(Ordering::Relaxed) {
        println!("{answer}");
    }
    true
}

// Match desktop GIO/GLib file URIs, including semicolons and non-ASCII names.
// WHATWG URL path escaping differs here and would miss shared thumbnails.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.!~*'()$&+,:=@".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut uri, "%{byte:02X}").unwrap();
        }
    }
    uri
}

fn stamp(m: &fs::Metadata) -> String {
    format!("{}:{:09}", m.mtime(), m.mtime_nsec())
}
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    (a.dev(), a.ino(), a.len(), a.mtime(), a.mtime_nsec())
        == (b.dev(), b.ino(), b.len(), b.mtime(), b.mtime_nsec())
}

fn read_png(path: &Path) -> Result<Vec<u8>> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Invalid thumbnail file".into());
    }
    if meta.len() > MAX_PNG {
        return Err("Thumbnail too large".into());
    }
    let mut bytes = Vec::new();
    f.take(MAX_PNG + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_PNG {
        return Err("Thumbnail too large".into());
    }
    Ok(bytes)
}

fn chunks(bytes: &[u8]) -> Result<Vec<(&[u8], &[u8])>> {
    if !bytes.starts_with(PNG) {
        return Err("Invalid thumbnail".into());
    }
    let mut pos = PNG.len();
    let mut parts = Vec::new();
    while pos + 12 <= bytes.len() {
        let n = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        if n > MAX_PNG as usize || n + 12 > bytes.len() - pos {
            break;
        }
        let kind = &bytes[pos + 4..pos + 8];
        let body = &bytes[pos + 8..pos + 8 + n];
        let expected = u32::from_be_bytes(bytes[pos + 8 + n..pos + 12 + n].try_into().unwrap());
        if crc32fast::hash(&bytes[pos + 4..pos + 8 + n]) != expected {
            return Err("Invalid thumbnail checksum".into());
        }
        parts.push((kind, body));
        pos += n + 12;
        if kind == b"IEND" {
            return Ok(parts);
        }
    }
    Err("Truncated thumbnail".into())
}

fn valid_cache(path: &Path, uri: &str, meta: &fs::Metadata, own: bool) -> bool {
    let Ok(bytes) = read_png(path) else {
        return false;
    };
    let Ok(parts) = chunks(&bytes) else {
        return false;
    };
    let fields: HashMap<&[u8], &[u8]> = parts
        .into_iter()
        .filter_map(|(kind, data)| {
            if kind != b"tEXt" {
                return None;
            }
            data.iter()
                .position(|b| *b == 0)
                .map(|at| (&data[..at], &data[at + 1..]))
        })
        .collect();
    let matches = |key: &[u8], value: &str| fields.get(key).is_some_and(|v| *v == value.as_bytes());
    matches(b"Thumb::URI", uri)
        && matches(b"Thumb::MTime", &meta.mtime().to_string())
        && (!fields.contains_key(b"Thumb::Size".as_slice())
            || matches(b"Thumb::Size", &meta.len().to_string()))
        && (!own || matches(b"OmaCast::FileStamp", &stamp(meta)))
}

fn append_chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let at = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32fast::hash(&out[at..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn save_cache(source: &Path, target: &Path, uri: &str, meta: &fs::Metadata) -> Result<()> {
    let bytes = read_png(source)?;
    let parts = chunks(&bytes)?;
    let fields = [
        ("Thumb::URI", uri.to_string()),
        ("Thumb::MTime", meta.mtime().to_string()),
        ("Thumb::Size", meta.len().to_string()),
        ("Software", "oma-cast file previews".into()),
        ("OmaCast::FileStamp", stamp(meta)),
    ];
    let mut out = PNG.to_vec();
    for (kind, data) in parts {
        if kind == b"IEND" {
            for (key, value) in &fields {
                let mut field = key.as_bytes().to_vec();
                field.push(0);
                field.extend_from_slice(value.as_bytes());
                append_chunk(&mut out, b"tEXt", &field);
            }
        }
        append_chunk(&mut out, kind, data);
    }
    let parent = target.parent().ok_or("Invalid cache path")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let dir = fs::symlink_metadata(parent).map_err(|e| e.to_string())?;
    if !dir.is_dir() || dir.uid() != unsafe { libc::geteuid() } {
        return Err("Unsafe preview cache directory".into());
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    tmp.write_all(&out).map_err(|e| e.to_string())?;
    tmp.persist(target).map_err(|e| e.to_string())?;
    Ok(())
}

struct Renderer {
    child: Option<Child>,
    exited: bool,
}
impl Renderer {
    fn completed(&mut self) -> io::Result<Option<bool>> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // Keep the leader unreaped until group cleanup, pinning its PID/PGID
        // against reuse even when a wrapper exits before its descendants.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.as_ref().unwrap().id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        self.exited = true;
        Ok(Some(
            info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0,
        ))
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            if self.exited {
                // waitid observed exit, so this wait cannot block on I/O.
                let _ = child.wait();
            } else {
                // Never hold preview admission in wait() on kernel-blocked I/O.
                thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}

fn render(mut cmd: Command, output: &Path, start: Instant) -> Result<()> {
    let parent = unsafe { libc::getpid() };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent
            {
                return Err(io::Error::other("Preview worker exited"));
            }
            for (kind, limit) in [
                (libc::RLIMIT_CPU, 8),
                (libc::RLIMIT_AS, 3 * 1024 * 1024 * 1024),
                (libc::RLIMIT_FSIZE, MAX_PNG),
                (libc::RLIMIT_CORE, 0),
            ] {
                let value = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(kind, &value) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            libc::nice(10);
            Ok(())
        });
    }
    let mut child = Renderer {
        child: Some(
            cmd.spawn()
                .map_err(|e| format!("Could not start preview renderer: {e}"))?,
        ),
        exited: false,
    };
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        if start.elapsed() > Duration::from_secs(10) {
            return Err("Preview timed out".into());
        }
        if let Some(success) = child.completed().map_err(|e| e.to_string())? {
            if !success || !output.is_file() {
                return Err("Could not preview this file (invalid or password-protected)".into());
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn facets(path: &Path, size: u64) -> Result<u32> {
    let mut input = File::open(path).map_err(|e| e.to_string())?;
    let mut header = [0; 84];
    input.read_exact(&mut header).map_err(|_| "Invalid STL")?;
    let n = u32::from_le_bytes(header[80..84].try_into().unwrap());
    if 84 + 50 * u64::from(n) == size {
        if n > MAX_FACETS {
            return Err("Preview limit: 2 million triangles".into());
        }
        return Ok(n);
    }
    if !String::from_utf8_lossy(&header)
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("solid")
    {
        return Err("Invalid or truncated STL".into());
    }
    // Bound ASCII input as well as binary input; read_until cannot allocate an
    // entire attacker-controlled line (a BufReader supplies fixed-size chunks).
    let mut reader =
        BufReader::new(File::open(path).map_err(|e| e.to_string())?).take(MAX_FILE + 1);
    let mut data = String::new();
    let mut count = 0;
    let mut consumed = 0;
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        let mut line = Vec::new();
        let n = (&mut reader)
            .take(4096)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        consumed += n;
        if consumed as u64 > MAX_FILE {
            return Err("Preview limit: 128 MiB".into());
        }
        if line.len() == 4096 && !line.ends_with(b"\n") {
            return Err("Invalid STL line".into());
        }
        data.clear();
        data.push_str(&String::from_utf8_lossy(&line));
        if data
            .split_ascii_whitespace()
            .next()
            .is_some_and(|token| token.eq_ignore_ascii_case("facet"))
        {
            count += 1;
        }
        if count > MAX_FACETS {
            return Err("Preview limit: 2 million triangles".into());
        }
    }
    if count == 0 {
        return Err("Invalid ASCII STL".into());
    }
    Ok(count)
}

fn preview(path: &Path) -> Result<Value> {
    let start = Instant::now();
    if !path.is_absolute() {
        return Err("Preview path must be absolute".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NONBLOCK);
    let input = options.open(path).map_err(|e| e.to_string())?;
    let meta = input.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Not a regular file".into());
    }
    if meta.len() > MAX_FILE {
        return Err("Preview limit: 128 MiB".into());
    }
    let uri = file_uri(path);
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let mut result = json!({"path":path.to_string_lossy(),"name":path.file_name().unwrap_or_default().to_string_lossy(),
        "bytes":meta.len(),"type":ext.to_ascii_uppercase()});
    if ext == "pdf" {
        result["page"] = json!(1);
    }
    let cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
        .ok_or("HOME is not set")?;
    let key = format!("{:x}.png", md5::compute(uri.as_bytes()));
    let own = cache.join("oma-cast/previews-v2").join(&key);
    let mut candidates = vec![(own.clone(), true)];
    for size in ["xx-large", "x-large", "large", "normal"] {
        candidates.push((cache.join("thumbnails").join(size).join(&key), false));
    }
    for (candidate, is_own) in candidates {
        if valid_cache(&candidate, &uri, &meta, is_own) {
            result["image"] = json!(Url::from_file_path(candidate)
                .map_err(|_| "Invalid cache path")?
                .to_string());
            result["source"] = json!("cache");
            result["elapsedMs"] = json!(start.elapsed().as_millis());
            return Ok(result);
        }
    }
    if ["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&ext.as_str()) {
        result["image"] = json!(uri);
        result["source"] = json!("image");
        result["elapsedMs"] = json!(start.elapsed().as_millis());
        return Ok(result);
    }
    if [
        "txt", "md", "rs", "py", "js", "json", "yaml", "yml", "toml", "ini", "log",
    ]
    .contains(&ext.as_str())
    {
        let mut bytes = Vec::new();
        input
            .take(8192)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        result["text"] = json!(String::from_utf8_lossy(&bytes));
        result["source"] = json!("text");
        result["elapsedMs"] = json!(start.elapsed().as_millis());
        return Ok(result);
    }
    if !["pdf", "stl", "obj", "ply"].contains(&ext.as_str()) {
        result["message"] = json!("No preview for this file type");
        return Ok(result);
    }
    if ext == "stl" {
        result["triangles"] = json!(facets(path, meta.len())?);
    }
    let tmp = TempDir::with_prefix("oma-cast-preview-").map_err(|e| e.to_string())?;
    let output = tmp.path().join("render.png");
    let mut cmd = if ext == "pdf" {
        let executable = program("pdftoppm", "OMA_CAST_PDFTOPPM_BIN")
            .ok_or("PDF preview requires Poppler (pdftoppm)")?;
        let mut cmd = Command::new(executable);
        cmd.args([
            "-f",
            "1",
            "-l",
            "1",
            "-singlefile",
            "-scale-to",
            "768",
            "-png",
        ])
        .arg(path)
        .arg(tmp.path().join("render"));
        cmd
    } else {
        let executable = program("f3d", "OMA_CAST_F3D_BIN").ok_or("3D preview requires F3D")?;
        let mut cmd = Command::new(executable);
        cmd.args([
            "--no-config",
            "--rendering-backend=egl",
            "--load-plugins=native",
            "--verbose=quiet",
            "--no-background",
            "--anti-aliasing=fxaa",
            "--up=+Z",
            "--camera-direction=-1,1,-0.8",
            "--camera-orthographic",
            "--resolution=512,512",
        ])
        .arg(format!("--output={}", output.display()))
        .arg(path);
        cmd.env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("LP_NUM_THREADS", "2")
            .env("OMP_NUM_THREADS", "2");
        cmd
    };
    cmd.env("HOME", tmp.path())
        .env("XDG_CACHE_HOME", tmp.path());
    render(cmd, &output, start)?;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage);
    }
    result["renderCpuMs"] = json!(
        (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) * 1000
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) / 1000
    );
    result["renderPeakRssMiB"] = json!(usage.ru_maxrss as f64 / 1024.0);
    let after = fs::metadata(path).map_err(|e| e.to_string())?;
    if !same_file(&meta, &after) {
        return Err("File changed during rendering; select it again".into());
    }
    save_cache(&output, &own, &uri, &meta)?;
    result["image"] = json!(Url::from_file_path(own)
        .map_err(|_| "Invalid cache path")?
        .to_string());
    result["source"] = json!("render");
    result["elapsedMs"] = json!(start.elapsed().as_millis());
    Ok(result)
}
