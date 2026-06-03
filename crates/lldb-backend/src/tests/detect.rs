//! Detection tests — mirror Go `internal/detect/detect_test.go`.
//!
//! Go uses `t.Setenv` + `t.TempDir` + 0o755 dummy bins. Rust runs tests concurrently,
//! so a shared process env (`PATH`/`LLDB_DAP_PATH`) would race; we drive a deterministic
//! [`FakeEnv`] through the crate-internal [`Env`] seam instead, which models the same
//! inputs (a PATH-resolvable name set, env vars, absolute-path existence, `xcrun`, and
//! the macOS gate) without touching the real filesystem.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::detect::{find_lldb_dap_with, Env};

/// A deterministic [`Env`]: a name→path map for PATH lookups, an env-var map, a set of
/// existing absolute paths, an optional `xcrun` result, and the macOS gate.
#[derive(Default)]
struct FakeEnv {
    vars: HashMap<String, String>,
    /// PATH-resolvable lookups: the key is what `look_path` is called with (a bare name,
    /// or an absolute path containing `/`), the value is the resolved full path.
    look_path: HashMap<String, PathBuf>,
    exists: HashSet<String>,
    xcrun: Option<String>,
    macos: bool,
}

impl FakeEnv {
    fn with_var(mut self, k: &str, v: &str) -> Self {
        self.vars.insert(k.to_string(), v.to_string());
        self
    }
    fn with_path(mut self, name: &str, full: &str) -> Self {
        self.look_path.insert(name.to_string(), PathBuf::from(full));
        self
    }
    fn with_exists(mut self, path: &str) -> Self {
        self.exists.insert(path.to_string());
        self
    }
    fn with_xcrun(mut self, out: &str) -> Self {
        self.xcrun = Some(out.to_string());
        self
    }
    fn macos(mut self) -> Self {
        self.macos = true;
        self
    }
}

impl Env for FakeEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn look_path(&self, name: &str) -> Option<PathBuf> {
        self.look_path.get(name).cloned()
    }
    fn path_exists(&self, path: &str) -> bool {
        self.exists.contains(path)
    }
    fn xcrun_find_lldb_dap(&self) -> Option<String> {
        self.xcrun.clone()
    }
    fn is_macos(&self) -> bool {
        self.macos
    }
}

#[test]
fn env_var_capable() {
    // Go `TestFindLLDBDAP_EnvVar`: LLDB_DAP_PATH points at a basename containing
    // "lldb-dap" → capable; the path is returned verbatim.
    let bin = "/tmp/custom/my-custom-lldb-dap";
    let env = FakeEnv::default()
        .with_var("LLDB_DAP_PATH", bin)
        .with_path(bin, bin);
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from(bin));
    assert!(got.is_lldb_dap, "basename contains lldb-dap ⇒ capable");
}

#[test]
fn env_var_lldb_vscode_not_capable() {
    // Go `TestFindLLDBDAP_EnvVarLLDBVscode`: basename `lldb-vscode` ⇒ not capable.
    let bin = "/tmp/custom/lldb-vscode";
    let env = FakeEnv::default()
        .with_var("LLDB_DAP_PATH", bin)
        .with_path(bin, bin);
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from(bin));
    assert!(!got.is_lldb_dap, "basename lldb-vscode ⇒ not capable");
}

#[test]
fn env_var_absolute_path_stat_fallback() {
    // LLDB_DAP_PATH not on PATH but exists as an absolute path (Go's `os.Stat` branch).
    let bin = "/opt/lldb-dap";
    let env = FakeEnv::default()
        .with_var("LLDB_DAP_PATH", bin)
        .with_exists(bin);
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from(bin));
    assert!(got.is_lldb_dap);
}

#[test]
fn env_var_priority_over_path() {
    // Go `TestFindLLDBDAP_EnvVarPriority`: env var wins over lldb-dap on PATH.
    let env_bin = "/env/custom-lldb-dap-binary";
    let env = FakeEnv::default()
        .with_var("LLDB_DAP_PATH", env_bin)
        .with_path(env_bin, env_bin)
        .with_path("lldb-dap", "/path/lldb-dap");
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from(env_bin));
}

#[test]
fn in_path_capable() {
    // Go `TestFindLLDBDAP_InPath`: lldb-dap on PATH ⇒ capable, full path returned.
    let env = FakeEnv::default().with_path("lldb-dap", "/usr/bin/lldb-dap");
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from("/usr/bin/lldb-dap"));
    assert!(got.is_lldb_dap);
}

#[test]
fn versioned_in_path() {
    // Go `TestFindLLDBDAP_VersionedInPath`: only lldb-dap-18 present.
    let env = FakeEnv::default().with_path("lldb-dap-18", "/usr/bin/lldb-dap-18");
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from("/usr/bin/lldb-dap-18"));
    assert!(got.is_lldb_dap);
}

#[test]
fn versioned_prefers_higher() {
    // Go `TestFindLLDBDAP_VersionedPrefersHigher`: both -15 and -19 present ⇒ 19 wins
    // (search descends 20..=15).
    let env = FakeEnv::default()
        .with_path("lldb-dap-15", "/usr/bin/lldb-dap-15")
        .with_path("lldb-dap-19", "/usr/bin/lldb-dap-19");
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from("/usr/bin/lldb-dap-19"));
    assert!(got.is_lldb_dap);
}

#[test]
fn fallback_to_lldb_vscode() {
    // Go `TestFindLLDBDAP_FallbackToLLDBVscode`: only lldb-vscode present ⇒ not capable.
    let env = FakeEnv::default().with_path("lldb-vscode", "/usr/bin/lldb-vscode");
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from("/usr/bin/lldb-vscode"));
    assert!(!got.is_lldb_dap);
}

#[test]
fn lldb_dap_before_lldb_vscode() {
    // Go `TestFindLLDBDAP_LLDBDAPBeforeLLDBVscode`: both present ⇒ lldb-dap wins.
    let env = FakeEnv::default()
        .with_path("lldb-dap", "/usr/bin/lldb-dap")
        .with_path("lldb-vscode", "/usr/bin/lldb-vscode");
    let got = find_lldb_dap_with(&env).expect("found");
    assert_eq!(got.path, PathBuf::from("/usr/bin/lldb-dap"));
    assert!(got.is_lldb_dap);
}

#[test]
fn nothing_found_lists_candidates() {
    // Go `TestFindLLDBDAP_NothingFound`: error names every searched candidate.
    let env = FakeEnv::default();
    let err = find_lldb_dap_with(&env).expect_err("nothing found");
    let msg = err.to_string();
    assert!(msg.contains("lldb-dap binary not found"), "got: {msg}");
    for name in [
        "lldb-dap",
        "lldb-dap-18",
        "lldb-dap-20",
        "lldb-dap-15",
        "lldb-vscode",
    ] {
        assert!(
            msg.contains(name),
            "error should mention {name}, got: {msg}"
        );
    }
    // Not on (simulated) macOS ⇒ no xcrun mention.
    assert!(!msg.contains("xcrun"), "no xcrun on non-macOS, got: {msg}");
}

#[test]
fn darwin_xcrun_found() {
    // macOS-only `xcrun --find lldb-dap` fallback (Go's `runtime.GOOS == "darwin"`).
    let env = FakeEnv::default()
        .macos()
        .with_xcrun("/Applications/Xcode.app/.../lldb-dap\n");
    let got = find_lldb_dap_with(&env).expect("found via xcrun");
    assert_eq!(
        got.path,
        PathBuf::from("/Applications/Xcode.app/.../lldb-dap")
    );
    assert!(got.is_lldb_dap, "xcrun result is capable");
}

#[test]
fn darwin_nothing_found_lists_xcrun() {
    // On macOS with no hit, the not-found message also lists the xcrun candidate.
    let env = FakeEnv::default().macos();
    let err = find_lldb_dap_with(&env).expect_err("nothing found");
    let msg = err.to_string();
    assert!(msg.contains("xcrun --find lldb-dap"), "got: {msg}");
}
