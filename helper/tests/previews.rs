//! Each preview is a separate process. Tests require only Rust and /bin/sh;
//! fake renderers verify argv and lifecycle without F3D/Poppler installations.
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    input: PathBuf,
    fake: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let input = dir.path().join("сердце ; $(touch PWN) %25.stl");
        let mut stl = vec![0; 80];
        stl.extend_from_slice(&1_u32.to_le_bytes());
        stl.extend_from_slice(&[0; 50]);
        fs::write(&input, stl).unwrap();
        fs::write(
            dir.path().join("fixture.png"),
            include_bytes!("fixtures/preview.png"),
        )
        .unwrap();
        let fake = dir.path().join("renderer with spaces");
        let f = Self { dir, input, fake };
        f.script("output=\nlast=\nfor arg do\n case \"$arg\" in --output=*) output=${arg#--output=} ;; esac\n last=$arg\ndone\n[ \"$last\" = \"$EXPECTED_INPUT\" ] || exit 19\ncp \"$FIXTURE_PNG\" \"$output\"\n");
        f
    }
    fn script(&self, body: &str) {
        fs::write(&self.fake, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
        fs::set_permissions(&self.fake, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn command(&self, path: &Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oma-cast-files"));
        c.args(["--preview"])
            .arg(path)
            .env("HOME", self.dir.path())
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env("OMA_CAST_F3D_BIN", &self.fake)
            .env("OMA_CAST_PDFTOPPM_BIN", &self.fake)
            .env("EXPECTED_INPUT", path)
            .env("FIXTURE_PNG", self.dir.path().join("fixture.png"));
        c
    }
    fn run(&self, path: &Path) -> Value {
        let o = self.command(path).output().unwrap();
        assert!(o.status.success(), "{:?}", o);
        serde_json::from_slice(&o.stdout).unwrap()
    }
}

#[test]
fn model_render_cache_and_literal_filename() {
    let f = Fixture::new();
    let v = f.run(&f.input);
    assert_eq!(v["source"], "render");
    assert_eq!(v["triangles"], 1);
    assert_eq!(f.run(&f.input)["source"], "cache");
    assert!(!f.dir.path().join("PWN").exists());
    fs::write(&f.input, b"broken").unwrap();
    assert_eq!(f.run(&f.input)["error"], true);
}
#[test]
fn same_size_edit_invalidates_cache() {
    let f = Fixture::new();
    assert_eq!(f.run(&f.input)["source"], "render");
    thread::sleep(Duration::from_millis(10));
    let mut bytes = fs::read(&f.input).unwrap();
    bytes[0] = 1;
    fs::write(&f.input, bytes).unwrap();
    assert_eq!(f.run(&f.input)["source"], "render");
}
#[test]
fn corrupt_cache_is_ignored() {
    let f = Fixture::new();
    let v = f.run(&f.input);
    let path = url::Url::parse(v["image"].as_str().unwrap())
        .unwrap()
        .to_file_path()
        .unwrap();
    fs::write(path, b"bad PNG").unwrap();
    assert_eq!(f.run(&f.input)["source"], "render");
}
#[test]
fn shared_desktop_cache_is_read() {
    let f = Fixture::new();
    let v = f.run(&f.input);
    let own = url::Url::parse(v["image"].as_str().unwrap())
        .unwrap()
        .to_file_path()
        .unwrap();
    let desktop = f.dir.path().join("cache/thumbnails/large");
    fs::create_dir_all(&desktop).unwrap();
    let target = desktop.join(own.file_name().unwrap());
    fs::rename(own, &target).unwrap();
    let result = f.run(&f.input);
    assert_eq!(result["source"], "cache");
    assert_eq!(
        result["image"],
        url::Url::from_file_path(target).unwrap().to_string()
    );
}
#[test]
fn text_is_bounded_and_images_need_no_renderer() {
    let f = Fixture::new();
    let text = f.dir.path().join("readme.txt");
    fs::write(&text, "a".repeat(12000)).unwrap();
    assert_eq!(f.run(&text)["text"].as_str().unwrap().len(), 8192);
    for ext in ["png", "jpg", "jpeg", "gif", "webp", "bmp"] {
        let path = f.dir.path().join(format!("image.{ext}"));
        fs::copy(f.dir.path().join("fixture.png"), &path).unwrap();
        let mut c = f.command(&path);
        c.env("OMA_CAST_F3D_BIN", "/missing");
        let v: Value = serde_json::from_slice(&c.output().unwrap().stdout).unwrap();
        assert_eq!(v["source"], "image");
    }
}
#[test]
fn pdf_only_first_page_and_cached() {
    let f = Fixture::new();
    let pdf = f.dir.path().join("document ; $(touch PWN).pdf");
    fs::write(&pdf, b"%PDF-1.4\n").unwrap();
    f.script("[ \"$1\" = -f ] && [ \"$2\" = 1 ] && [ \"$3\" = -l ] && [ \"$4\" = 1 ] && [ \"$5\" = -singlefile ] && [ \"$6\" = -scale-to ] && [ \"$7\" = 768 ] && [ \"$8\" = -png ]\n[ \"$9\" = \"$EXPECTED_INPUT\" ]\nshift 9\ncp \"$FIXTURE_PNG\" \"$1.png\"\n");
    let v = f.run(&pdf);
    assert_eq!(v["page"], 1);
    assert_eq!(v["source"], "render");
    assert_eq!(f.run(&pdf)["source"], "cache");
}
#[test]
fn missing_optional_renderers_are_explained() {
    let f = Fixture::new();
    for (path, key, expected) in [
        (&f.input, "OMA_CAST_F3D_BIN", "F3D"),
        (
            &f.dir.path().join("missing.pdf"),
            "OMA_CAST_PDFTOPPM_BIN",
            "Poppler",
        ),
    ] {
        if path.extension().unwrap() == "pdf" {
            fs::write(path, b"%PDF-1.4\n").unwrap();
        }
        let mut c = f.command(path);
        c.env(key, "/no-such-program");
        let v: Value = serde_json::from_slice(&c.output().unwrap().stdout).unwrap();
        assert_eq!(v["error"], true);
        assert!(v["message"].as_str().unwrap().contains(expected));
    }
}
#[test]
fn failed_renderers_and_malformed_png_do_not_cache() {
    let f = Fixture::new();
    let pdf = f.dir.path().join("bad.pdf");
    fs::write(&pdf, b"not a PDF").unwrap();
    for path in [&f.input, &pdf] {
        f.script("exit 1\n");
        assert_eq!(f.run(path)["error"], true);
        // Successful exit with invalid output must not populate the cache.
        f.script("output=\nlast=\nfor arg do\n case \"$arg\" in --output=*) output=${arg#--output=} ;; esac\n last=$arg\ndone\n[ -n \"$output\" ] || output=$last.png\nprintf 'not a PNG' > \"$output\"\n");
        assert_eq!(f.run(path)["error"], true);
        assert!(!f.dir.path().join("cache/oma-cast/previews-v2").exists());
    }
}
#[test]
fn oversize_and_triangle_budgets() {
    let f = Fixture::new();
    let file = fs::File::create(&f.input).unwrap();
    file.set_len(128 * 1024 * 1024 + 1).unwrap();
    assert!(f.run(&f.input)["message"]
        .as_str()
        .unwrap()
        .contains("128 MiB"));
    let mut bytes = vec![0; 80];
    bytes.extend_from_slice(&2_000_001_u32.to_le_bytes());
    fs::write(&f.input, bytes).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&f.input)
        .unwrap()
        .set_len(84 + 50 * 2_000_001)
        .unwrap();
    assert!(f.run(&f.input)["message"]
        .as_str()
        .unwrap()
        .contains("2 million"));
}
#[test]
fn ascii_stl_and_non_regular_input() {
    let f = Fixture::new();
    fs::write(&f.input,b"solid test\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid test\n").unwrap();
    assert_eq!(f.run(&f.input)["triangles"], 1);
    assert_eq!(f.run(f.dir.path())["error"], true);
}

#[test]
fn ascii_stl_accepts_token_whitespace() {
    for separator in ["\t", "  ", " \t "] {
        let f = Fixture::new();
        fs::write(&f.input, format!("solid test\n\tfacet{separator}normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid test\n")).unwrap();
        let v = f.run(&f.input);
        assert_eq!(v["triangles"], 1, "{v}");
        assert_eq!(v["source"], "render");
    }
}

fn assert_stopped(pid: i32) {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z") {
            return;
        }
        if Instant::now() >= end {
            // Keep a failing regression test from leaking its own fixture.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            panic!("renderer descendant {pid} survived cleanup: {stat}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn descendants_are_killed_after_renderer_success_and_error() {
    for code in [0, 1] {
        let f = Fixture::new();
        let pidfile = f.dir.path().join("descendant");
        f.script(&format!("for arg do\n case \"$arg\" in --output=*) cp \"$FIXTURE_PNG\" \"${{arg#--output=}}\" ;; esac\ndone\n/usr/bin/sleep 30 &\necho $! > \"$PIDFILE\"\nexit {code}\n"));
        let output = f
            .command(&f.input)
            .env("PIDFILE", &pidfile)
            .output()
            .unwrap();
        let pid = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
        assert_stopped(pid);
        let v: Value = serde_json::from_slice(&output.stdout).unwrap();
        if code == 0 {
            assert_eq!(v["source"], "render");
        } else {
            assert_eq!(v["error"], true);
        }
    }
}

#[test]
fn descendants_are_killed_on_cancellation_and_timeout() {
    for cancel in [true, false] {
        let f = Fixture::new();
        let pidfile = f.dir.path().join("descendant");
        f.script("/usr/bin/sleep 30 &\necho $! > \"$PIDFILE\"\nwait\n");
        let mut child = f
            .command(&f.input)
            .env("PIDFILE", &pidfile)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(4);
        while !pidfile.exists() && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pidfile.exists());
        let pid = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
        if cancel {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
        }
        let end = Instant::now() + Duration::from_secs(if cancel { 2 } else { 12 });
        while child.try_wait().unwrap().is_none() && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        let finished = child.try_wait().unwrap().is_some();
        if !finished {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_stopped(pid);
        assert!(finished, "worker did not release preview admission");
    }
}
#[test]
fn cancellation_kills_active_renderer() {
    let f = Fixture::new();
    let pidfile = f.dir.path().join("pid");
    f.script("echo $$ > \"$PIDFILE\"\nexec /usr/bin/sleep 30\n");
    let mut cmd = f.command(&f.input);
    cmd.env("PIDFILE", &pidfile).stdout(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(4);
    while !pidfile.exists() && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pidfile.exists());
    let pid: i32 = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let end = Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        child.try_wait().unwrap().is_some(),
        "worker held cancellation"
    );
    thread::sleep(Duration::from_millis(100));
    let status = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    assert!(
        status.is_empty() || status.split_whitespace().nth(2) == Some("Z"),
        "renderer survived cancellation"
    );
}
#[test]
fn capabilities_without_python() {
    let f = Fixture::new();
    let mut cmd = f.command(&f.input);
    cmd.args(["--ignored"]); // Separate mode, no FFF initialization.
    let output = Command::new(env!("CARGO_BIN_EXE_oma-cast-files"))
        .arg("--preview-capabilities")
        .env("OMA_CAST_F3D_BIN", "/missing")
        .env("OMA_CAST_PDFTOPPM_BIN", "/missing")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["previewProtocol"], 1);
    assert_eq!(v["f3d"], false);
    assert_eq!(v["pdf"], false);
}

#[test]
fn forced_worker_exit_kills_renderer() {
    let f = Fixture::new();
    let pidfile = f.dir.path().join("pid");
    f.script("echo $$ > \"$PIDFILE\"\nexec /usr/bin/sleep 30\n");
    let mut child = f
        .command(&f.input)
        .env("PIDFILE", &pidfile)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(4);
    while !pidfile.exists() && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pidfile.exists());
    let pid: i32 = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    thread::sleep(Duration::from_millis(100));
    let status = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    assert!(
        status.is_empty() || status.split_whitespace().nth(2) == Some("Z"),
        "renderer survived worker SIGKILL"
    );
}

#[test]
fn wall_timeout_releases_worker() {
    let f = Fixture::new();
    f.script("exec /usr/bin/sleep 30\n");
    let started = Instant::now();
    let v = f.run(&f.input);
    assert_eq!(v["error"], true);
    assert!(v["message"].as_str().unwrap().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(12));
}

#[test]
fn cache_uri_matches_desktop_escaping() {
    let f = Fixture::new();
    let v = f.run(&f.input);
    let own = url::Url::parse(v["image"].as_str().unwrap())
        .unwrap()
        .to_file_path()
        .unwrap();
    let uri = format!(
        "file://{}/%D1%81%D0%B5%D1%80%D0%B4%D1%86%D0%B5%20%3B%20$(touch%20PWN)%20%2525.stl",
        f.dir.path().display()
    );
    assert_eq!(
        own.file_name().unwrap().to_string_lossy(),
        format!("{:x}.png", md5::compute(uri.as_bytes()))
    );
}
