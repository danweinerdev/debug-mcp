//! Memory handlers: `read_memory`, `disassemble` (Spec FR-13, task 5.4).
//!
//! `read_memory` normalizes the `0x` prefix, reads, and formats a hex dump (empty data →
//! `bytes_read:0`, no dump). `disassemble` defaults to **20** instructions (Spec OQ-1 —
//! intentional deviation from Go's code value of 10) and resolves the current PC via
//! `stack_trace(levels=1)` when no address is given.

use mcp_session::State;
use serde_json::{Map, Value};

use crate::errors;
use crate::format::format_hex_dump;
use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;
use crate::Args;

/// The default disassemble instruction count (Spec OQ-1 — 20, the documented intent; Go's
/// code value of 10 is treated as a latent bug).
const DEFAULT_DISASSEMBLE_COUNT: i64 = 20;

impl ToolServer {
    /// `read_memory` (Spec FR-13.1). Guard stopped → normalize address → `read_memory` →
    /// empty data → `bytes_read:0`; else hex dump over the parsed address.
    pub(crate) async fn handle_read_memory(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let address = match args.require_string("address") {
            Ok(a) => a,
            Err(e) => return ToolOutcome::error(e),
        };
        let count = match args.require_int("count") {
            Ok(c) => c,
            Err(e) => return ToolOutcome::error(e),
        };

        let address = normalize_hex(&address);

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::READ_MEMORY.request_failed.to_string()),
        };

        let read = match backend.read_memory(&address, count).await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::READ_MEMORY.render(e)),
        };

        // Empty data → {address(=response), bytes_read:0}, no hex_dump.
        if read.data.is_empty() {
            return ToolOutcome::Json(
                RespBuilder::new()
                    .set("address", read.address)
                    .set("bytes_read", 0)
                    .build(),
            );
        }

        // Parse the normalized request address (sans 0x) as hex for the dump's row labels.
        let addr_str = address
            .strip_prefix("0x")
            .or_else(|| address.strip_prefix("0X"))
            .unwrap_or(&address);
        let start_addr = match u64::from_str_radix(addr_str, 16) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::error(format!("failed to parse address: {e}")),
        };

        let bytes_read = read.data.len();
        let hex_dump = format_hex_dump(&read.data, start_addr);

        ToolOutcome::Json(
            RespBuilder::new()
                .set("address", read.address)
                .set("bytes_read", bytes_read)
                .set("hex_dump", hex_dump)
                .build(),
        )
    }

    /// `disassemble` (Spec FR-13.2). Guard stopped → default count 20 → current-PC path via
    /// `stack_trace(levels=1)` when no address → normalize → `disassemble` → format.
    pub(crate) async fn handle_disassemble(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let mut address = args.get_string("address", "");

        // instruction_count default 20, overridden only when present and > 0.
        let mut instruction_count = DEFAULT_DISASSEMBLE_COUNT;
        if let Some(ic) = args.get_f64("instruction_count") {
            if ic > 0.0 {
                instruction_count = ic as i64;
            }
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::DISASSEMBLE.request_failed.to_string()),
        };

        // No address → resolve the current PC via the top frame's instruction pointer.
        let mut current_pc = String::new();
        if address.is_empty() {
            let thread_id = self
                .session
                .last_stopped()
                .map(|e| e.thread_id)
                .unwrap_or(1);
            let (frames, _total) = match backend.stack_trace(thread_id, 0, 1).await {
                Ok(v) => v,
                Err(e) => return ToolOutcome::error(errors::STACK_TRACE.render(e)),
            };
            let ip = frames
                .first()
                .and_then(|f| f.instruction_pointer.clone())
                .filter(|ip| !ip.is_empty());
            match ip {
                Some(ip) => {
                    address = ip.clone();
                    current_pc = ip;
                }
                None => {
                    return ToolOutcome::error("no instruction pointer available for current frame")
                }
            }
        }

        // Normalize the address (and current PC) with a 0x prefix.
        address = normalize_hex(&address);
        if !current_pc.is_empty() {
            current_pc = normalize_hex(&current_pc);
        }

        let instructions = match backend.disassemble(&address, instruction_count).await {
            Ok(i) => i,
            Err(e) => return ToolOutcome::error(errors::DISASSEMBLE.render(e)),
        };

        let items: Vec<Value> = instructions
            .iter()
            .map(|inst| {
                let mut entry = Map::new();
                entry.insert("address".to_string(), Value::from(inst.address.clone()));
                entry.insert(
                    "instruction".to_string(),
                    Value::from(inst.instruction.clone()),
                );
                if !inst.bytes.is_empty() {
                    entry.insert("bytes".to_string(), Value::from(inst.bytes.clone()));
                }
                if !inst.symbol.is_empty() {
                    entry.insert("symbol".to_string(), Value::from(inst.symbol.clone()));
                }
                if let Some(path) = &inst.source_path {
                    if !path.is_empty() {
                        entry.insert("file".to_string(), Value::from(path.clone()));
                        entry.insert("line".to_string(), Value::from(inst.line));
                    }
                }
                if !current_pc.is_empty() && inst.address == current_pc {
                    entry.insert("is_current_pc".to_string(), Value::from(true));
                }
                Value::Object(entry)
            })
            .collect();

        let count = items.len();
        ToolOutcome::Json(
            RespBuilder::new()
                .set("instructions", Value::Array(items))
                .set("count", count)
                .set("start_address", address)
                .build(),
        )
    }
}

/// Ensure a `0x` prefix on a hex address string (Go's `strings.HasPrefix` check for both
/// `0x` and `0X`).
fn normalize_hex(address: &str) -> String {
    if address.starts_with("0x") || address.starts_with("0X") {
        address.to_string()
    } else {
        format!("0x{address}")
    }
}
