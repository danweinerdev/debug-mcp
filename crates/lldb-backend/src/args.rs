//! lldb-dap launch/attach argument shapes (Spec FR-17.9, task 3.3). Go origin:
//! `internal/dap/types.go` (`LLDBDAPLaunchArgs`, `LLDBDAPAttachArgs`).
//!
//! These structs serialize into the DAP `LaunchRequest`/`AttachRequest` `arguments`
//! field and reach lldb-dap, so their JSON field names and omission rules are
//! **behavioral** (Spec Appendix A): `program` is always present; every other field is
//! omitted when empty/false, reproducing Go's `omitempty`. The omission of `stopOnEntry`
//! when `false` in particular changes what lldb-dap receives (Spec FR-4.7), so it is
//! intentional, not incidental.

use serde::Serialize;

/// Skip-serialize predicate for a `bool` field with Go `omitempty` semantics (omit when
/// `false`).
fn is_false(b: &bool) -> bool {
    !*b
}

/// Skip-serialize predicate for an `i64` field with Go `omitempty` semantics (omit when
/// `0`).
fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// lldb-dap launch arguments (Go `LLDBDAPLaunchArgs`). `program` is always serialized;
/// all other fields follow Go's `omitempty`.
///
/// The command-list fields (`initCommands`, …) are part of the wire contract but are
/// never populated by the current tool layer (Go leaves them `nil`); they are modeled
/// for shape-parity and always omitted today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LldbLaunchArgs {
    pub program: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub cwd: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,

    #[serde(rename = "stopOnEntry", skip_serializing_if = "is_false")]
    pub stop_on_entry: bool,

    #[serde(rename = "initCommands", skip_serializing_if = "Vec::is_empty")]
    pub init_commands: Vec<String>,

    #[serde(rename = "preRunCommands", skip_serializing_if = "Vec::is_empty")]
    pub pre_run_commands: Vec<String>,

    #[serde(rename = "postRunCommands", skip_serializing_if = "Vec::is_empty")]
    pub post_run_commands: Vec<String>,

    #[serde(rename = "stopCommands", skip_serializing_if = "Vec::is_empty")]
    pub stop_commands: Vec<String>,

    #[serde(rename = "exitCommands", skip_serializing_if = "Vec::is_empty")]
    pub exit_commands: Vec<String>,

    #[serde(rename = "terminateCommands", skip_serializing_if = "Vec::is_empty")]
    pub terminate_commands: Vec<String>,
}

impl LldbLaunchArgs {
    /// Build launch args from a [`LaunchSpec`]-derived set. `env` is `Vec<(k,v)>` so the
    /// neutral spec stays serde-friendly; it serializes as a JSON object via the custom
    /// `env` map serialization below.
    ///
    /// [`LaunchSpec`]: debugger_core::LaunchSpec
    pub fn new(
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stop_on_entry: bool,
    ) -> Self {
        LldbLaunchArgs {
            program,
            args,
            cwd: cwd.unwrap_or_default(),
            env,
            stop_on_entry,
            init_commands: Vec::new(),
            pre_run_commands: Vec::new(),
            post_run_commands: Vec::new(),
            stop_commands: Vec::new(),
            exit_commands: Vec::new(),
            terminate_commands: Vec::new(),
        }
    }
}

/// lldb-dap attach arguments (Go `LLDBDAPAttachArgs`). All fields follow Go's
/// `omitempty`: `pid` omitted when 0, `program`/`coreFile` when empty, `waitFor`/
/// `stopOnEntry` when false, the command list when empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LldbAttachArgs {
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub pid: i64,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub program: String,

    #[serde(rename = "waitFor", skip_serializing_if = "is_false")]
    pub wait_for: bool,

    #[serde(rename = "stopOnEntry", skip_serializing_if = "is_false")]
    pub stop_on_entry: bool,

    #[serde(rename = "attachCommands", skip_serializing_if = "Vec::is_empty")]
    pub attach_commands: Vec<String>,

    #[serde(rename = "coreFile", skip_serializing_if = "String::is_empty")]
    pub core_file: String,
}

impl LldbAttachArgs {
    /// Build attach args. Attach always sets `stopOnEntry=true` (Go `attach.go`); the
    /// caller sets `pid` (when `pid>0`) or `wait_for=true` + `program=<name>`.
    pub fn new(pid: Option<i64>, wait_for_name: Option<String>) -> Self {
        let mut a = LldbAttachArgs {
            pid: 0,
            program: String::new(),
            wait_for: false,
            stop_on_entry: true,
            attach_commands: Vec::new(),
            core_file: String::new(),
        };
        match pid {
            Some(pid) if pid > 0 => a.pid = pid,
            _ => {
                a.wait_for = true;
                a.program = wait_for_name.unwrap_or_default();
            }
        }
        a
    }
}

/// Serialize launch/attach args to a `serde_json::Value` object. The `env` field needs
/// to be a JSON **object** (`{"KEY":"value"}`), not the array a `Vec<(String,String)>`
/// would serialize to, so launch args go through a manual conversion here rather than
/// `serde_json::to_value` directly.
pub fn launch_args_to_value(a: &LldbLaunchArgs) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("program".to_string(), a.program.clone().into());
    if !a.args.is_empty() {
        map.insert("args".to_string(), a.args.clone().into());
    }
    if !a.cwd.is_empty() {
        map.insert("cwd".to_string(), a.cwd.clone().into());
    }
    if !a.env.is_empty() {
        let env_obj: serde_json::Map<String, serde_json::Value> = a
            .env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        map.insert("env".to_string(), serde_json::Value::Object(env_obj));
    }
    if a.stop_on_entry {
        map.insert("stopOnEntry".to_string(), true.into());
    }
    // The command-list fields are never populated today; include them only if non-empty
    // for forward-compatibility with the same omitempty contract.
    insert_str_list(&mut map, "initCommands", &a.init_commands);
    insert_str_list(&mut map, "preRunCommands", &a.pre_run_commands);
    insert_str_list(&mut map, "postRunCommands", &a.post_run_commands);
    insert_str_list(&mut map, "stopCommands", &a.stop_commands);
    insert_str_list(&mut map, "exitCommands", &a.exit_commands);
    insert_str_list(&mut map, "terminateCommands", &a.terminate_commands);
    serde_json::Value::Object(map)
}

/// Serialize attach args to a `serde_json::Value` object (no map-valued field, so the
/// derived `Serialize` is exact, but we go through the same helper for symmetry and to
/// keep the omitempty rules in one place).
pub fn attach_args_to_value(a: &LldbAttachArgs) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if a.pid != 0 {
        map.insert("pid".to_string(), a.pid.into());
    }
    if !a.program.is_empty() {
        map.insert("program".to_string(), a.program.clone().into());
    }
    if a.wait_for {
        map.insert("waitFor".to_string(), true.into());
    }
    if a.stop_on_entry {
        map.insert("stopOnEntry".to_string(), true.into());
    }
    insert_str_list(&mut map, "attachCommands", &a.attach_commands);
    if !a.core_file.is_empty() {
        map.insert("coreFile".to_string(), a.core_file.clone().into());
    }
    serde_json::Value::Object(map)
}

fn insert_str_list(
    map: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    list: &[String],
) {
    if !list.is_empty() {
        map.insert(key.to_string(), list.to_vec().into());
    }
}
