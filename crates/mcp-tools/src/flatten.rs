//! `flatten_variables` — an exact port of Go `variables_util.go`'s `FlattenVariables`
//! (Spec FR-11).
//!
//! The algorithm fetches one variables level at a time, emits each variable in DAP
//! response order, recurses into children up to `depth` (prefixing names with a dotted
//! path), and stops the instant the emitted count reaches `max_count`. The top-level
//! filter is a case-insensitive substring match applied **only** to top-level names;
//! children of an included parent are always included.
//!
//! Fetching is abstracted behind [`VariableFetcher`] so tests can inject canned
//! responses (mirroring Go's `fakeVarServer`); a blanket impl lets any
//! `&dyn DebuggerBackend` fetch via [`DebuggerBackend::variables`].

use async_trait::async_trait;
use debugger_core::{BackendError, DebuggerBackend, Variable};
use serde::Serialize;

/// A flat, JSON-ready variable (Go `FlatVariable`, Spec FR-11.2). Nested names are
/// dotted paths (`parent.child.grandchild`). `type`/`has_children`/`children_count`
/// are omitted when empty/false/zero to match Go's `omitempty`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlatVariable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub ty: String,
    #[serde(skip_serializing_if = "is_false")]
    pub has_children: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub children_count: i64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Fetches the children of a variables reference — the one operation
/// [`flatten_variables`] needs. Abstracted so tests can supply canned levels without a
/// live backend.
#[async_trait]
pub trait VariableFetcher {
    /// Fetch the variables for `variables_reference` (one DAP `VariablesRequest`).
    async fn fetch(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError>;
}

/// Any `DebuggerBackend` fetches variables via its `variables` method.
#[async_trait]
impl<T: DebuggerBackend + ?Sized> VariableFetcher for T {
    async fn fetch(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        self.variables(variables_reference).await
    }
}

/// Flatten a variables reference into `(flat_list, truncated)` (Spec FR-11, Go
/// `FlattenVariables`).
///
/// - `filter`: case-insensitive substring match on **top-level** names only; empty
///   matches all.
/// - `depth`: how many child levels to expand. At each container, `depth > 0` emits
///   the bare parent then recurses with `depth-1`; `depth == 0` emits the parent with
///   `has_children=true` and `children_count = named + indexed`.
/// - `max_count`: cap on total emitted nodes (containers AND leaves). The cap is
///   checked after every emitted node; the result never exceeds `max_count`, and
///   `truncated` is `true` iff the cap was hit.
pub async fn flatten_variables(
    fetch: &(impl VariableFetcher + ?Sized),
    variables_reference: i64,
    depth: i64,
    max_count: usize,
    filter: &str,
) -> Result<(Vec<FlatVariable>, bool), BackendError> {
    let vars = fetch.fetch(variables_reference).await?;

    let mut result: Vec<FlatVariable> = Vec::new();
    let filter_lower = filter.to_lowercase();

    for v in &vars {
        if !filter.is_empty() && !v.name.to_lowercase().contains(&filter_lower) {
            continue;
        }

        let mut flat = FlatVariable {
            name: v.name.clone(),
            value: v.value.clone(),
            ty: v.ty.clone(),
            has_children: false,
            children_count: 0,
        };

        if v.variables_reference > 0 {
            if depth > 0 {
                result.push(flat);
                if result.len() >= max_count {
                    return Ok((result, true));
                }
                let truncated = flatten_recursive(
                    fetch,
                    v.variables_reference,
                    &v.name,
                    depth - 1,
                    &mut result,
                    max_count,
                )
                .await?;
                if truncated {
                    return Ok((result, true));
                }
            } else {
                flat.has_children = true;
                flat.children_count = v.named + v.indexed;
                result.push(flat);
                if result.len() >= max_count {
                    return Ok((result, true));
                }
            }
        } else {
            result.push(flat);
            if result.len() >= max_count {
                return Ok((result, true));
            }
        }
    }

    Ok((result, false))
}

/// Recursive helper (Go `flattenRecursive`). Fetches the children of `var_ref`,
/// prefixes their names with `prefix.`, appends to `result`, and returns `true` if the
/// cap was hit. No filter is applied at child levels.
async fn flatten_recursive(
    fetch: &(impl VariableFetcher + ?Sized),
    var_ref: i64,
    prefix: &str,
    depth: i64,
    result: &mut Vec<FlatVariable>,
    max_count: usize,
) -> Result<bool, BackendError> {
    let vars = fetch.fetch(var_ref).await?;

    for v in &vars {
        let child_name = format!("{prefix}.{}", v.name);
        let mut flat = FlatVariable {
            name: child_name.clone(),
            value: v.value.clone(),
            ty: v.ty.clone(),
            has_children: false,
            children_count: 0,
        };

        if v.variables_reference > 0 {
            if depth > 0 {
                result.push(flat);
                if result.len() >= max_count {
                    return Ok(true);
                }
                let truncated = Box::pin(flatten_recursive(
                    fetch,
                    v.variables_reference,
                    &child_name,
                    depth - 1,
                    result,
                    max_count,
                ))
                .await?;
                if truncated {
                    return Ok(true);
                }
            } else {
                flat.has_children = true;
                flat.children_count = v.named + v.indexed;
                result.push(flat);
                if result.len() >= max_count {
                    return Ok(true);
                }
            }
        } else {
            result.push(flat);
            if result.len() >= max_count {
                return Ok(true);
            }
        }
    }

    Ok(false)
}
