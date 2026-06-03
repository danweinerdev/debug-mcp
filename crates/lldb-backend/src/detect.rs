//! lldb-dap detection (Spec FR-15, task 3.1). Go origin: `internal/detect/detect.go`
//! (`FindLLDBDAP`).
//!
//! Searches a fixed fallback chain and returns the first hit plus the
//! `--repl-mode=command` capability flag. The order, the substring-`lldb-dap`
//! capability rule, the versioned `20..=15` descending search, and the not-found
//! message (listing every searched candidate) reproduce the Go function exactly. The
//! macOS-only `xcrun` fallback is gated on the host OS (Spec OQ-4) — never attempted on
//! Linux.

use std::path::{Path, PathBuf};

use debugger_core::BackendError;

/// A detected lldb-dap binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// Full path to the binary (verbatim from the env var, PATH lookup, or `xcrun`).
    pub path: PathBuf,
    /// `true` when the binary is `lldb-dap` (LLVM 18+) — the flag that later gates
    /// `--repl-mode=command` and the `run_command` backtick fallback. `false` for the
    /// legacy `lldb-vscode`.
    pub is_lldb_dap: bool,
}

/// Locate the lldb-dap (or lldb-vscode) binary (Go `FindLLDBDAP`).
///
/// Detection order:
/// 1. `LLDB_DAP_PATH` — PATH lookup, else an absolute-path stat. Capable when the
///    basename contains `lldb-dap`.
/// 2. `lldb-dap` on PATH (capable).
/// 3. `lldb-dap-<N>` on PATH for `N` from 20 down to 15 inclusive (prefers higher;
///    capable).
/// 4. `lldb-vscode` on PATH (not capable).
/// 5. macOS only: `xcrun --find lldb-dap` (capable).
///
/// On no match → [`BackendError::Detect`] with
/// `lldb-dap binary not found; searched: <comma-list>`.
pub fn find_lldb_dap() -> Result<Detected, BackendError> {
    find_lldb_dap_with(&SystemEnv)
}

/// The environment surface detection depends on, factored behind a trait so the tests
/// can drive a fully deterministic search (Go uses `t.Setenv`/`t.TempDir`; Rust runs
/// tests concurrently, so a shared process env would race — this keeps each test's
/// PATH/env private).
pub(crate) trait Env {
    /// The value of an environment variable, or `None`/empty when unset.
    fn var(&self, key: &str) -> Option<String>;
    /// Resolve `name` against `PATH` (Go `exec.LookPath` for a bare name), returning
    /// the full path of an executable file if found.
    fn look_path(&self, name: &str) -> Option<PathBuf>;
    /// Whether `path` names an existing file (Go `os.Stat`), for the absolute-path
    /// `LLDB_DAP_PATH` fallback.
    fn path_exists(&self, path: &str) -> bool;
    /// Run `xcrun --find lldb-dap`, returning the trimmed stdout path on success.
    /// Only invoked on macOS.
    fn xcrun_find_lldb_dap(&self) -> Option<String>;
    /// Whether the host OS is macOS (`runtime.GOOS == "darwin"`).
    fn is_macos(&self) -> bool;
}

/// `true` when `name` (a basename) marks a repl-mode-capable binary: the basename
/// **contains** the substring `lldb-dap` (Go `strings.Contains(base, "lldb-dap")`).
fn is_capable_basename(name: &str) -> bool {
    name.contains("lldb-dap")
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

pub(crate) fn find_lldb_dap_with(env: &dyn Env) -> Result<Detected, BackendError> {
    let mut searched: Vec<String> = Vec::new();

    // 1. LLDB_DAP_PATH.
    if let Some(env_path) = env.var("LLDB_DAP_PATH").filter(|p| !p.is_empty()) {
        // PATH lookup first (Go `exec.LookPath(envPath)`), then absolute-path stat.
        if let Some(resolved) = env.look_path(&env_path) {
            // Go returns `envPath` verbatim (not the resolved path) here.
            let _ = resolved;
            return Ok(Detected {
                path: PathBuf::from(&env_path),
                is_lldb_dap: is_capable_basename(basename(&env_path)),
            });
        }
        if env.path_exists(&env_path) {
            return Ok(Detected {
                path: PathBuf::from(&env_path),
                is_lldb_dap: is_capable_basename(basename(&env_path)),
            });
        }
        searched.push(format!("LLDB_DAP_PATH={env_path}"));
    }

    // 2. lldb-dap on PATH (capable).
    if let Some(p) = env.look_path("lldb-dap") {
        return Ok(Detected {
            path: p,
            is_lldb_dap: true,
        });
    }
    searched.push("lldb-dap".to_string());

    // 3. lldb-dap-<N> for N = 20..=15 descending (prefers higher; capable).
    for v in (15..=20).rev() {
        let name = format!("lldb-dap-{v}");
        if let Some(p) = env.look_path(&name) {
            return Ok(Detected {
                path: p,
                is_lldb_dap: true,
            });
        }
        searched.push(name);
    }

    // 4. lldb-vscode on PATH (not capable).
    if let Some(p) = env.look_path("lldb-vscode") {
        return Ok(Detected {
            path: p,
            is_lldb_dap: false,
        });
    }
    searched.push("lldb-vscode".to_string());

    // 5. macOS only: xcrun --find lldb-dap (capable).
    if env.is_macos() {
        if let Some(p) = env.xcrun_find_lldb_dap() {
            let p = p.trim();
            if !p.is_empty() {
                return Ok(Detected {
                    path: PathBuf::from(p),
                    is_lldb_dap: true,
                });
            }
        }
        searched.push("xcrun --find lldb-dap".to_string());
    }

    Err(BackendError::Detect(format!(
        "lldb-dap binary not found; searched: {}",
        searched.join(", ")
    )))
}

/// The production [`Env`]: the real process environment, `PATH` resolution, and `xcrun`.
struct SystemEnv;

impl Env for SystemEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn look_path(&self, name: &str) -> Option<PathBuf> {
        look_path_in(name, std::env::var_os("PATH").as_deref())
    }

    fn path_exists(&self, path: &str) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    fn xcrun_find_lldb_dap(&self) -> Option<String> {
        let out = std::process::Command::new("xcrun")
            .args(["--find", "lldb-dap"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn is_macos(&self) -> bool {
        cfg!(target_os = "macos")
    }
}

/// Resolve `name` against `PATH` like Go's `exec.LookPath`: if `name` contains a path
/// separator, treat it as a path and check it directly; otherwise scan each `PATH`
/// entry. An entry is a match when it is an existing regular file with an executable
/// bit (Unix). Returns the full path.
fn look_path_in(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    // A name containing a separator is resolved as a path, not searched on PATH
    // (Go `LookPath` does the same).
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable_file(&p).then_some(p);
    }
    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}
