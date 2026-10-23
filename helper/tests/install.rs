//! CLI tests run in private HOMEs with a stub shell. PATH has Omarchy's jq,
//! but no Python, Cargo or optional preview renderers.
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    home: TempDir,
    bin: PathBuf,
    helper: PathBuf,
    cli: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let bin = home.path().join("tools");
        fs::create_dir(&bin).unwrap();
        for name in [
            "bash", "readlink", "dirname", "mkdir", "ln", "seq", "sleep", "install", "mv", "rm",
            "grep", "tail", "jq",
        ] {
            symlink(format!("/usr/bin/{name}"), bin.join(name)).unwrap();
        }
        for (name, body) in [
            ("omarchy-shell", "case \"$*\" in *files.status*) printf '%s\\n' \"$FILES_STATUS\" ;; *system.healthPing*) echo '{\"ok\":true}' ;; *) echo ok ;; esac\n"),
            ("omarchy", "case \"$*\" in 'plugin list --json') echo '[{\"id\":\"typecraft.oma-cast\",\"enabled\":true}]' ;; *) echo ok ;; esac\n"),
            ("hyprctl", "echo ok\n"), ("socat", "exit 0\n"),
        ] { script(&bin.join(name), body); }
        let helper = home.path().join(".local/lib/oma-cast/oma-cast-files");
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("bin/oma-cast");
        Self {
            home,
            bin,
            helper,
            cli,
        }
    }
    fn link_plugin(&self) {
        let plugins = self.home.path().join(".config/omarchy/plugins");
        fs::create_dir_all(&plugins).unwrap();
        symlink(
            self.cli.parent().unwrap().parent().unwrap().join("plugin"),
            plugins.join("typecraft.oma-cast"),
        )
        .unwrap();
    }
    fn output(&self, args: &[&str], status: &str) -> Output {
        Command::new("/usr/bin/bash")
            .arg(&self.cli)
            .args(args)
            .env("HOME", self.home.path())
            .env("PATH", &self.bin)
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_RUNTIME_DIR", self.home.path().join("run"))
            .env("OMA_CAST_F3D_BIN", "/missing")
            .env("OMA_CAST_PDFTOPPM_BIN", "/missing")
            .env_remove("OMA_CAST_FILES_BIN")
            .env("FILES_STATUS", status)
            .output()
            .unwrap()
    }
    fn run(&self, args: &[&str]) -> String {
        let o = self.output(
            args,
            r#"{"ok":true,"data":{"enabled":true,"state":"ready","indexedFiles":1,"rssMb":7}}"#,
        );
        assert!(
            o.status.success(),
            "{args:?}\n{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
}
fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/bash\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn private_install_diagnose_and_uninstall_without_python() {
    let f = Fixture::new();
    fs::copy(env!("CARGO_BIN_EXE_oma-cast-files"), &f.helper).unwrap();
    fs::set_permissions(&f.helper, fs::Permissions::from_mode(0o755)).unwrap();
    f.run(&["install"]);
    assert!(f
        .home
        .path()
        .join(".config/omarchy/plugins/typecraft.oma-cast")
        .is_symlink());
    assert!(f.home.path().join(".local/bin/oma-cast").is_symlink());
    let caps: serde_json::Value = serde_json::from_str(&f.run(&["previews", "status"])).unwrap();
    assert_eq!(caps["f3d"], false);
    assert_eq!(caps["pdf"], false);
    assert_eq!(caps["images"], true);
    let doctor = f.run(&["doctor"]);
    assert!(doctor.contains("optional poppler"));
    assert!(doctor.contains("optional f3d"));
    f.run(&["uninstall"]);
    assert!(!f.helper.exists());
    assert!(!f.home.path().join(".local/bin/oma-cast").exists());
    assert!(!f
        .home
        .path()
        .join(".config/omarchy/plugins/typecraft.oma-cast")
        .exists());
}

#[test]
fn doctor_diagnoses_absent_helper_independently() {
    let f = Fixture::new();
    f.link_plugin();
    let o = f.output(
        &["doctor"],
        r#"{"ok":true,"data":{"enabled":true,"state":"error","error":"helper-missing"}}"#,
    );
    assert_eq!(o.status.code(), Some(1));
    let text = String::from_utf8(o.stdout).unwrap();
    assert!(text.contains("FAIL  file search helper missing"), "{text}");
    assert!(text.contains("FAIL  file search: helper-missing"), "{text}");
    assert!(!text.contains("state unavailable"));
    // A deliberately disabled service does not require a helper installation.
    let o = f.output(
        &["doctor"],
        r#"{"ok":true,"data":{"enabled":false,"state":"disabled"}}"#,
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
}

#[test]
fn old_helper_does_not_block_status_roots_or_existing_settings() {
    let f = Fixture::new();
    f.link_plugin();
    script(&f.helper, "echo 'unknown argument' >&2\nexit 2\n");
    let doctor = f.run(&["doctor"]);
    assert!(doctor.contains("preview helper needs rebuilding"));
    assert!(doctor.contains("file search ready (1 files, 7 MB RSS)"));
    let o = f.output(
        &["files", "root"],
        r#"{"ok":true,"data":{"root":"/home/test/модели ; $(touch PWN)"}}"#,
    );
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8(o.stdout).unwrap(),
        "/home/test/модели ; $(touch PWN)\n"
    );
    let settings = f.home.path().join(".local/share/oma-cast/db/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(settings, r#"{"files":{"enabled":true}}"#).unwrap();
    assert!(f
        .run(&["install"])
        .contains("File search is enabled (existing settings)"));
}

#[test]
fn unreadable_status_and_missing_jq_fail_diagnosis() {
    let f = Fixture::new();
    f.link_plugin();
    let o = f.output(&["doctor"], "not JSON");
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8(o.stdout)
        .unwrap()
        .contains("cannot read file search status"));
    fs::remove_file(f.bin.join("jq")).unwrap();
    let o = f.output(
        &["doctor"],
        r#"{"ok":true,"data":{"enabled":true,"state":"ready"}}"#,
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8(o.stdout)
        .unwrap()
        .contains("FAIL  jq is installed"));
}
