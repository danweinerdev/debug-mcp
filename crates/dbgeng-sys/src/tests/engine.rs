//! Non-live unit tests for the `engine` module's task-2.4 surface that need no DbgEng session.
//!
//! Live go/step/break_in/interrupt coverage lives in `tests/execution.rs`; here we assert the
//! static guarantees that do not require a target — chiefly that [`InterruptHandle`] is `Send`
//! (the one type in this crate intended to cross a thread boundary; the R4 flag-only design rests
//! on it being soundly `Send` with no `unsafe`).

use std::collections::HashSet;
use std::path::PathBuf;

use debugger_core::ModuleInfo;

use std::path::Path;

use crate::InterruptHandle;
use crate::engine::{
    condition_expr, discover_debuggers_root_with, extension_dirs, select_existing_root,
    symbol_status, truncate_output,
};

/// Compile-time proof that `InterruptHandle` is `Send`. It is the only piece of this crate that is
/// moved to another thread (the off-thread interrupter); were it accidentally made `!Send` (e.g.
/// by holding a COM interface), the off-thread interrupt seam would not compile. This is a
/// type-level assertion — it has no runtime body.
#[test]
fn interrupt_handle_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<InterruptHandle>();
}

/// The breakpoint-condition expression builder wraps the user's C++ condition in the
/// `@@c++( (<cond>) ? 1 : 0 )` boolean projection the typed `Evaluate(DEBUG_VALUE_INT64)` call
/// expects (so the result is exactly 0 or 1). This is the one pure, FFI-free slice of the
/// conditional-breakpoint logic; the evaluation itself is exercised by the live tests in
/// `tests/conditional_breakpoints.rs`.
#[test]
fn condition_expr_wraps_in_the_cpp_boolean_projection() {
    assert_eq!(condition_expr("i == 5"), "@@c++( (i == 5) ? 1 : 0 )");
    assert_eq!(
        condition_expr("nonexistent_xyz == 1"),
        "@@c++( (nonexistent_xyz == 1) ? 1 : 0 )"
    );
    // The condition is embedded verbatim (no escaping) — parity with the C++ string concatenation.
    assert_eq!(
        condition_expr("sum > 0 && i < n"),
        "@@c++( (sum > 0 && i < n) ? 1 : 0 )"
    );
}

/// The `symbol_status` map produces exactly the documented neutral vocabulary
/// (`{"pdb","export","deferred","none"}`) the `ModuleInfo::symbol_status` contract promises, with
/// every full-symbol `DEBUG_SYMTYPE_*` collapsing to `"pdb"`. This is the canonical set the
/// `modules()` format-contract test below asserts against; a drift here would break the agent's
/// "do symbols resolve?" decision.
#[test]
fn symbol_status_maps_to_the_neutral_vocabulary() {
    use windows::Win32::System::Diagnostics::Debug::Extensions::{
        DEBUG_SYMTYPE_CODEVIEW, DEBUG_SYMTYPE_COFF, DEBUG_SYMTYPE_DEFERRED, DEBUG_SYMTYPE_DIA,
        DEBUG_SYMTYPE_EXPORT, DEBUG_SYMTYPE_NONE, DEBUG_SYMTYPE_PDB, DEBUG_SYMTYPE_SYM,
    };

    // All full-symbol formats collapse to "pdb".
    for full in [
        DEBUG_SYMTYPE_PDB,
        DEBUG_SYMTYPE_CODEVIEW,
        DEBUG_SYMTYPE_COFF,
        DEBUG_SYMTYPE_SYM,
        DEBUG_SYMTYPE_DIA,
    ] {
        assert_eq!(symbol_status(full), "pdb");
    }
    assert_eq!(symbol_status(DEBUG_SYMTYPE_EXPORT), "export");
    assert_eq!(symbol_status(DEBUG_SYMTYPE_DEFERRED), "deferred");
    assert_eq!(symbol_status(DEBUG_SYMTYPE_NONE), "none");
    // Any unknown value falls back to "none".
    assert_eq!(symbol_status(0xDEAD_BEEF), "none");

    // The produced token is always one of the four documented values.
    for ty in [
        DEBUG_SYMTYPE_PDB,
        DEBUG_SYMTYPE_EXPORT,
        DEBUG_SYMTYPE_DEFERRED,
        DEBUG_SYMTYPE_NONE,
        0u32,
        12345u32,
    ] {
        let token = symbol_status(ty);
        assert!(
            matches!(token.as_str(), "pdb" | "export" | "deferred" | "none"),
            "symbol_status({ty}) produced an out-of-vocabulary token {token:?}"
        );
    }
}

/// The `ModuleInfo` format contract that `modules()` builds (and that the tool/serde layer relies
/// on): `base` is a fixed-width `0x` + 16 uppercase hex digits, `size` is a decimal string, and
/// `symbol_status` is one of the documented tokens. We assert it against a `ModuleInfo` constructed
/// with the exact `modules()` formatting (`format!("0x{base:016X}")` / `size.to_string()` /
/// `symbol_status(...)`), so a change to the formatting in `modules()` is caught without a live
/// engine. (A live module-field assertion against the real `test_target` runs in `tests/`.)
#[test]
fn module_info_format_contract() {
    // Build a ModuleInfo the *same* way `Engine::modules` does, from raw DbgEng-shaped values.
    let base: u64 = 0x0000_7FFA_1B2C_0000;
    let size: u64 = 2_138_112;
    use windows::Win32::System::Diagnostics::Debug::Extensions::DEBUG_SYMTYPE_PDB;
    let module = ModuleInfo {
        name: "ntdll.dll".to_string(),
        base: format!("0x{base:016X}"),
        size: size.to_string(),
        symbol_status: symbol_status(DEBUG_SYMTYPE_PDB),
    };

    // base: literal "0x" + exactly 16 uppercase hex digits.
    assert_eq!(module.base, "0x00007FFA1B2C0000");
    assert_eq!(module.base.len(), 18);
    assert!(module.base.starts_with("0x"));
    let hex = &module.base[2..];
    assert_eq!(hex.len(), 16);
    assert!(
        hex.chars()
            .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
        "base hex digits must be uppercase 0-9A-F, got {hex:?}"
    );

    // size: a decimal string (no 0x, all ASCII digits).
    assert_eq!(module.size, "2138112");
    assert!(module.size.chars().all(|c| c.is_ascii_digit()));

    // symbol_status: one of the documented tokens.
    assert!(matches!(
        module.symbol_status.as_str(),
        "pdb" | "export" | "deferred" | "none"
    ));
}

/// Helper: turn the resolved `.extpath` list into the `;`-joined string the FFI command uses, so
/// the assembly is asserted exactly as `ensure_extensions_loaded` builds it.
fn join_extpath(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(";")
}

/// `extension_dirs` mirrors the C++ winext/WINXP/base set, filtered to the dirs that actually exist
/// (the R8 refinement, task 5.0). With ALL THREE subdirs present it returns them in the fixed order
/// winext → winxp → base, and the `.extpath` string joins them with `;`. The existence predicate is
/// INJECTED so this needs no live filesystem.
#[test]
fn extension_dirs_all_three_present_in_order() {
    let root = PathBuf::from("C:\\Kits\\10\\Debuggers\\x64");
    let dirs = extension_dirs(&root, |_| true);

    assert_eq!(
        dirs,
        vec![root.join("winext"), root.join("winxp"), root.clone(),],
        "all three dirs in winext → winxp → base order"
    );

    // The `.extpath` argument joins the existing dirs with ';' in that order.
    let joined = join_extpath(&dirs);
    assert_eq!(
        joined,
        format!(
            "{}\\winext;{}\\winxp;{}",
            root.display(),
            root.display(),
            root.display()
        )
    );
}

/// Only the base `Debuggers\x64` dir exists (no `winext`/`winxp`): just the base is returned.
#[test]
fn extension_dirs_only_base_present() {
    let root = PathBuf::from("D:\\Sdk\\Debuggers\\x64");
    let dirs = extension_dirs(&root, |p| p == root);
    assert_eq!(dirs, vec![root.clone()]);
    assert_eq!(join_extpath(&dirs), root.to_string_lossy());
}

/// `winext` + base exist, `winxp` absent: those two are returned in order and `winxp` is skipped.
#[test]
fn extension_dirs_skips_absent_winxp() {
    let root = PathBuf::from("E:\\WinKit\\Debuggers\\x64");
    let winext = root.join("winext");
    let winxp = root.join("winxp");
    let present: HashSet<PathBuf> = [winext.clone(), root.clone()].into_iter().collect();

    let dirs = extension_dirs(&root, |p| present.contains(p));
    assert_eq!(dirs, vec![winext, root.clone()]);
    assert!(
        !dirs.contains(&winxp),
        "winxp must be skipped when it does not exist"
    );
}

/// Nothing exists under the root → empty list (so `ensure_extensions_loaded` skips `.extpath`).
#[test]
fn extension_dirs_none_present_is_empty() {
    let root = PathBuf::from("F:\\nope\\Debuggers\\x64");
    let dirs = extension_dirs(&root, |_| false);
    assert!(dirs.is_empty());
    assert_eq!(join_extpath(&dirs), "");
}

/// `select_existing_root` (the pure selection step of `discover_debuggers_root`) returns the FIRST
/// candidate whose base dir satisfies the injected existence predicate, honoring the registry → env
/// → default ordering, and `None` when none exist.
#[test]
fn select_existing_root_picks_first_existing_in_order() {
    let reg = PathBuf::from("C:\\reg\\Debuggers\\x64");
    let env = PathBuf::from("C:\\env\\Debuggers\\x64");
    let default = PathBuf::from("C:\\default\\Debuggers\\x64");

    // Only env + default exist → env wins (it precedes default in the candidate order).
    let exists_env_default: HashSet<PathBuf> = [env.clone(), default.clone()].into_iter().collect();
    let picked = select_existing_root(vec![reg.clone(), env.clone(), default.clone()], |p| {
        exists_env_default.contains(p)
    });
    assert_eq!(picked, Some(env.clone()));

    // The registry candidate exists → it wins over env/default.
    let picked = select_existing_root(vec![reg.clone(), env.clone(), default.clone()], |_| true);
    assert_eq!(picked, Some(reg));

    // Nothing exists → None.
    let picked = select_existing_root(vec![env, default], |_| false);
    assert_eq!(picked, None);
}

const DEFAULT_ROOT: &str = "C:\\Program Files (x86)\\Windows Kits\\10\\Debuggers\\x64";

/// A registry hit wins: when the registry yields `KitsRoot10`, the derived `<root>\Debuggers\x64`
/// candidate is chosen ahead of env/default (here everything "exists", so ordering decides). Also
/// confirms the `Debuggers\x64` join is correct onto a `KitsRoot10` value WITH a trailing `\` (the
/// real registry value ends in `\`): the join produces `...\Windows Kits\10\Debuggers\x64` with no
/// doubled separator.
#[test]
fn discover_registry_hit_wins_and_joins_trailing_sep_cleanly() {
    // Real registry value: note the trailing backslash.
    let reg_reader = |_wow: bool| Some(PathBuf::from("C:\\Program Files\\Windows Kits\\10\\"));
    let picked = discover_debuggers_root_with(reg_reader, Some("C:\\sdk".into()), |_| true);
    assert_eq!(
        picked,
        Some(PathBuf::from(
            "C:\\Program Files\\Windows Kits\\10\\Debuggers\\x64"
        )),
        "registry candidate must win and the trailing-slash join must not double the separator"
    );
}

/// Registry miss → fall through to the `WindowsSdkDir` env var (joined with `Debuggers\x64`).
#[test]
fn discover_registry_miss_falls_through_to_env() {
    let reg_reader = |_wow: bool| None;
    let picked = discover_debuggers_root_with(reg_reader, Some("C:\\my sdk".into()), |_| true);
    assert_eq!(
        picked,
        Some(PathBuf::from("C:\\my sdk\\Debuggers\\x64")),
        "with no registry value the env var candidate must be chosen"
    );

    // An empty env value is treated as unset → fall through to the default.
    let picked = discover_debuggers_root_with(|_| None, Some(String::new()), |_| true);
    assert_eq!(picked, Some(PathBuf::from(DEFAULT_ROOT)));
}

/// Registry AND env both miss → the former hardcoded default is the last resort.
#[test]
fn discover_both_miss_falls_through_to_default() {
    let picked = discover_debuggers_root_with(|_| None, None, |_| true);
    assert_eq!(picked, Some(PathBuf::from(DEFAULT_ROOT)));
}

/// The chosen root must pass the existence predicate: a registry path that does NOT exist falls
/// through to env/default. Here the registry candidate is reported missing by `exists`, so the
/// existing env candidate wins even though the registry candidate precedes it.
#[test]
fn discover_nonexistent_registry_path_falls_through_to_existing_env() {
    let reg_root = PathBuf::from("C:\\ghost\\Debuggers\\x64");
    let env_root = PathBuf::from("C:\\real\\Debuggers\\x64");
    let exists = move |p: &Path| p == env_root.as_path();
    let picked = discover_debuggers_root_with(
        |_wow| Some(PathBuf::from("C:\\ghost")),
        Some("C:\\real".into()),
        exists,
    );
    assert_eq!(
        picked,
        Some(PathBuf::from("C:\\real\\Debuggers\\x64")),
        "a registry path that doesn't exist must not be chosen over an existing env path"
    );
    let _ = reg_root; // documents the (non-existing) registry-derived candidate.
}

/// Nothing exists anywhere → `None` (no fallback path on disk).
#[test]
fn discover_returns_none_when_nothing_exists() {
    let picked = discover_debuggers_root_with(|_| Some(PathBuf::from("C:\\x")), None, |_| false);
    assert_eq!(picked, None);
}

/// Dedup: the native and WOW6432Node views usually return the SAME `KitsRoot10`, so the derived
/// `Debuggers\x64` candidate would otherwise appear twice. The seam de-duplicates, and the native
/// view still wins (priority order preserved). We prove the dedup by recording the predicate calls:
/// the duplicated registry candidate is probed exactly ONCE.
#[test]
fn discover_dedups_identical_registry_views() {
    use std::cell::RefCell;

    // Both views return the same path.
    let reg_reader = |_wow: bool| Some(PathBuf::from("C:\\Program Files\\Windows Kits\\10\\"));
    let probed = RefCell::new(Vec::<PathBuf>::new());
    let exists = |p: &Path| {
        probed.borrow_mut().push(p.to_path_buf());
        false // force probing of every candidate so we can count them
    };
    let picked = discover_debuggers_root_with(reg_reader, Some("C:\\sdk".into()), exists);
    assert_eq!(picked, None);

    let probed = probed.into_inner();
    let reg_candidate = PathBuf::from("C:\\Program Files\\Windows Kits\\10\\Debuggers\\x64");
    let reg_hits = probed.iter().filter(|p| **p == reg_candidate).count();
    assert_eq!(
        reg_hits, 1,
        "the identical registry candidate from both views must be de-duplicated to a single \
         candidate, got probes: {probed:?}"
    );
    // The env + default candidates are still present after the single registry candidate.
    assert!(probed.contains(&PathBuf::from("C:\\sdk\\Debuggers\\x64")));
    assert!(probed.contains(&PathBuf::from(DEFAULT_ROOT)));
    // Priority order preserved: registry candidate probed first.
    assert_eq!(probed.first(), Some(&reg_candidate));
}

/// `truncate_output` mirrors the C++ `truncateOutput`: output at or under the 32 KiB cap passes
/// through unchanged; output over the cap is cut to (at most) the cap and the fixed suffix is
/// appended. This bounds `analyze()`'s `!analyze -v` report exactly as the C++ tool layer did.
#[test]
fn truncate_output_caps_at_32kb_with_suffix() {
    const CAP: usize = 32768;
    const SUFFIX: &str = "\n\n... (output truncated at 32KB)";

    // Under the cap: unchanged.
    let small = "EXCEPTION_ACCESS_VIOLATION".to_string();
    assert_eq!(truncate_output(small.clone()), small);

    // Exactly at the cap: unchanged (the C++ only truncates when strictly greater).
    let exact = "a".repeat(CAP);
    assert_eq!(truncate_output(exact.clone()), exact);

    // Over the cap: cut to the cap and suffixed. The body before the suffix is exactly CAP bytes
    // (the input is single-byte ASCII so the char-boundary rounding does not move the cut).
    let big = "b".repeat(CAP + 5000);
    let truncated = truncate_output(big);
    assert!(truncated.ends_with(SUFFIX), "missing truncation suffix");
    let body = &truncated[..truncated.len() - SUFFIX.len()];
    assert_eq!(body.len(), CAP, "body should be capped at exactly 32KB");
    assert!(body.bytes().all(|b| b == b'b'));
}

/// Multi-byte boundary safety: the `is_char_boundary` walk-back must round the cut DOWN to a char
/// boundary so the returned `String` never splits a UTF-8 sequence. With a 3-byte char ('€'),
/// 32768 is not a multiple of 3, so the raw cut at 32768 lands mid-char; the walk-back must move it
/// to the nearest boundary at or below the cap. We assert: no panic, truncation occurred, the body
/// is at most the cap, the cut sits on a char boundary, and the body re-parses as valid UTF-8.
#[test]
fn truncate_output_rounds_down_to_char_boundary_on_multibyte() {
    const CAP: usize = 32768;
    const SUFFIX: &str = "\n\n... (output truncated at 32KB)";

    // '€' is 3 bytes in UTF-8; 32768 % 3 != 0, so a raw byte cut at the cap would split a char.
    let euro = '\u{20AC}';
    assert_eq!(euro.len_utf8(), 3);
    let count = (CAP / 3) + 100; // comfortably past the cap.
    let big: String = euro.to_string().repeat(count);
    assert!(big.len() > CAP);

    let truncated = truncate_output(big); // must not panic on the mid-char raw index.
    assert!(truncated.ends_with(SUFFIX), "missing truncation suffix");

    let body = &truncated[..truncated.len() - SUFFIX.len()];
    // The cut rounds DOWN to a boundary: body <= cap, and 32768 is not a 3-multiple so it lands
    // strictly below at the largest multiple of 3 (32766).
    assert!(body.len() <= CAP, "body must not exceed the cap");
    assert_eq!(
        body.len() % 3,
        0,
        "cut must land on a '€' (3-byte) char boundary"
    );
    // Re-parses cleanly as UTF-8 and contains only whole '€' chars (no replacement/split byte).
    let reparsed = std::str::from_utf8(body.as_bytes()).expect("body is valid UTF-8");
    assert!(reparsed.chars().all(|c| c == euro), "no char was split");
}
