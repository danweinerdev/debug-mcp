//! Breakpoint tracking (Spec FR-7).
//!
//! Mirrors Go `session_test.go`: `TestAddSourceBreakpointsMultipleFiles`,
//! `TestRemoveSourceBreakpointByID`, `TestRemoveBreakpointByIDNotFound`,
//! `TestListBreakpointsSorted`, `TestFunctionBreakpoints`,
//! `TestRemoveFunctionBreakpointByID`, `TestPendingBreakpointFlush`,
//! `TestSourceBreakpointsForFileCopy`.

use crate::{BreakpointInfo, SessionManager};

fn source_info(id: i64, file: &str, line: i64) -> BreakpointInfo {
    BreakpointInfo {
        id,
        ty: "source".to_string(),
        file: file.to_string(),
        line,
        function: String::new(),
        condition: String::new(),
        verified: true,
    }
}

fn function_info(id: i64, function: &str) -> BreakpointInfo {
    BreakpointInfo {
        id,
        ty: "function".to_string(),
        file: String::new(),
        line: 0,
        function: function.to_string(),
        condition: String::new(),
        verified: true,
    }
}

#[test]
fn add_source_breakpoints_multiple_files() {
    // Go `TestAddSourceBreakpointsMultipleFiles`.
    let sm = SessionManager::new();

    let bp1 = sm.add_source_breakpoint("/src/main.go", 10, "");
    let bp2 = sm.add_source_breakpoint("/src/main.go", 25, "x > 5");
    let bp3 = sm.add_source_breakpoint("/src/util.go", 42, "");

    assert_eq!((bp1.line, bp1.condition.as_str()), (10, ""));
    assert_eq!((bp2.line, bp2.condition.as_str()), (25, "x > 5"));
    assert_eq!(bp3.line, 42);

    let main_bps = sm.source_breakpoints_for_file("/src/main.go");
    assert_eq!(main_bps.len(), 2);
    assert_eq!((main_bps[0].line, main_bps[1].line), (10, 25));

    let util_bps = sm.source_breakpoints_for_file("/src/util.go");
    assert_eq!(util_bps.len(), 1);
    assert_eq!(util_bps[0].line, 42);

    // A file with no breakpoints returns an empty list.
    assert!(sm.source_breakpoints_for_file("/src/other.go").is_empty());
}

#[test]
fn remove_source_breakpoint_by_id() {
    // Go `TestRemoveSourceBreakpointByID` — source matched by line only.
    let sm = SessionManager::new();

    sm.add_source_breakpoint("/src/main.go", 10, "");
    sm.add_source_breakpoint("/src/main.go", 25, "");
    sm.add_breakpoint_response(source_info(1, "/src/main.go", 10));
    sm.add_breakpoint_response(source_info(2, "/src/main.go", 25));

    let (file_path, was_func) = sm.remove_breakpoint_by_id(1).expect("remove id 1");
    assert_eq!(file_path, "/src/main.go");
    assert!(!was_func);

    // Only line 25 remains.
    let bps = sm.source_breakpoints_for_file("/src/main.go");
    assert_eq!(bps.len(), 1);
    assert_eq!(bps[0].line, 25);

    // Response 1 is gone.
    let list = sm.list_breakpoints();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, 2);
}

#[test]
fn remove_breakpoint_by_id_not_found() {
    // Go `TestRemoveBreakpointByIDNotFound` — exact error string.
    let sm = SessionManager::new();
    let err = sm
        .remove_breakpoint_by_id(999)
        .expect_err("unknown id should error");
    assert_eq!(err, "breakpoint ID 999 not found");
}

#[test]
fn breakpoint_info_peeks_without_mutating() {
    // The read-only lookup used by the transactional remove path: it returns the tracked
    // metadata for a known id (and `None` for an unknown id) WITHOUT removing anything.
    let sm = SessionManager::new();
    sm.add_source_breakpoint("/src/main.go", 10, "");
    sm.add_breakpoint_response(source_info(1, "/src/main.go", 10));

    let info = sm.breakpoint_info(1).expect("known id");
    assert_eq!(info.id, 1);
    assert_eq!(info.ty, "source");
    assert_eq!(info.line, 10);
    assert!(sm.breakpoint_info(999).is_none());

    // The peek did not mutate: the breakpoint is still tracked and removable.
    assert_eq!(sm.list_breakpoints().len(), 1);
    assert!(sm.remove_breakpoint_by_id(1).is_ok());
}

#[test]
fn list_breakpoints_sorted() {
    // Go `TestListBreakpointsSorted` — ascending by id regardless of insert order.
    let sm = SessionManager::new();

    sm.add_breakpoint_response(source_info(5, "/a.go", 1));
    sm.add_breakpoint_response(function_info(2, "main"));
    sm.add_breakpoint_response(source_info(8, "/b.go", 10));

    let list = sm.list_breakpoints();
    let ids: Vec<i64> = list.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![2, 5, 8]);
}

#[test]
fn function_breakpoints_add_and_copy() {
    // Go `TestFunctionBreakpoints` — add returns the created bp; getter is a copy.
    let sm = SessionManager::new();

    let bp1 = sm.add_function_breakpoint("main", "");
    let bp2 = sm.add_function_breakpoint("handleRequest", "count > 3");

    assert_eq!((bp1.name.as_str(), bp1.condition.as_str()), ("main", ""));
    assert_eq!(
        (bp2.name.as_str(), bp2.condition.as_str()),
        ("handleRequest", "count > 3")
    );

    let mut all = sm.all_function_breakpoints();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name, "main");
    assert_eq!(all[1].name, "handleRequest");

    // Mutating the returned vector must not affect the session's copy.
    all[0].name = "modified".to_string();
    let original = sm.all_function_breakpoints();
    assert_eq!(original[0].name, "main");
}

#[test]
fn remove_function_breakpoint_by_id() {
    // Go `TestRemoveFunctionBreakpointByID` — function matched by name only.
    let sm = SessionManager::new();

    sm.add_function_breakpoint("main", "");
    sm.add_function_breakpoint("handler", "");
    sm.add_breakpoint_response(function_info(1, "main"));
    sm.add_breakpoint_response(function_info(2, "handler"));

    let (file_path, was_func) = sm.remove_breakpoint_by_id(1).expect("remove id 1");
    assert_eq!(file_path, "");
    assert!(was_func);

    let all = sm.all_function_breakpoints();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].name, "handler");
}

#[test]
fn pending_breakpoint_flush_idempotent() {
    // Go `TestPendingBreakpointFlush` — flush moves to active; second flush is a no-op.
    let sm = SessionManager::new();

    sm.add_pending_source_breakpoint("/src/main.go", 10, "");
    sm.add_pending_source_breakpoint("/src/main.go", 20, "x > 0");
    sm.add_pending_source_breakpoint("/src/util.go", 5, "");
    sm.add_pending_function_breakpoint("main", "");
    sm.add_pending_function_breakpoint("init", "");

    let (source_files, func_bps) = sm.flush_pending_breakpoints();

    assert_eq!(source_files.len(), 2);
    assert_eq!(source_files["/src/main.go"].len(), 2);
    assert_eq!(source_files["/src/util.go"].len(), 1);
    assert_eq!(func_bps.len(), 2);

    // Now active.
    assert_eq!(sm.source_breakpoints_for_file("/src/main.go").len(), 2);
    assert_eq!(sm.all_function_breakpoints().len(), 2);

    // Second flush returns empty and does not duplicate active state.
    let (source_files2, func_bps2) = sm.flush_pending_breakpoints();
    assert!(source_files2.is_empty());
    assert!(func_bps2.is_empty());
    assert_eq!(sm.source_breakpoints_for_file("/src/main.go").len(), 2);
    assert_eq!(sm.all_function_breakpoints().len(), 2);
}

#[test]
fn source_breakpoints_for_file_is_a_copy() {
    // Go `TestSourceBreakpointsForFileCopy` — defensive copy getter.
    let sm = SessionManager::new();

    sm.add_source_breakpoint("/src/main.go", 10, "");

    let mut bps = sm.source_breakpoints_for_file("/src/main.go");
    bps[0].line = 999;

    let original = sm.source_breakpoints_for_file("/src/main.go");
    assert_eq!(original[0].line, 10);
}
