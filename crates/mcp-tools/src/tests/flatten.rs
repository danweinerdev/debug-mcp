//! `flatten_variables` parity — every vector from Go `variables_util_test.go`.
//!
//! A `FakeFetcher` plays Go's `fakeVarServer`: it maps a `variables_reference` to a
//! canned `Vec<Variable>` via a closure, so each level's response is deterministic.

use async_trait::async_trait;
use debugger_core::{BackendError, Variable};

use crate::{flatten_variables, FlatVariable, VariableFetcher};

/// A `Variable` builder defaulting the rarely-set fields, matching the Go test's
/// `godap.Variable{...}` literals.
fn var(name: &str, value: &str, ty: &str, vref: i64) -> Variable {
    Variable {
        name: name.to_string(),
        value: value.to_string(),
        ty: ty.to_string(),
        variables_reference: vref,
        named: 0,
        indexed: 0,
    }
}

fn var_named(name: &str, value: &str, ty: &str, vref: i64, named: i64) -> Variable {
    Variable {
        name: name.to_string(),
        value: value.to_string(),
        ty: ty.to_string(),
        variables_reference: vref,
        named,
        indexed: 0,
    }
}

/// Maps a variables reference to a canned level (Go's `fakeVarServer` handler).
struct FakeFetcher<F: Fn(i64) -> Vec<Variable> + Send + Sync>(F);

#[async_trait]
impl<F: Fn(i64) -> Vec<Variable> + Send + Sync> VariableFetcher for FakeFetcher<F> {
    async fn fetch(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        Ok((self.0)(variables_reference))
    }
}

fn fetcher(
    f: impl Fn(i64) -> Vec<Variable> + Send + Sync,
) -> FakeFetcher<impl Fn(i64) -> Vec<Variable> + Send + Sync> {
    FakeFetcher(f)
}

#[tokio::test]
async fn basic() {
    // Go TestFlattenVariablesBasic.
    let f = fetcher(|_| {
        vec![
            var("x", "42", "int", 0),
            var("y", "3.14", "float64", 0),
            var("name", "\"hello\"", "string", 0),
        ]
    });

    let (result, truncated) = flatten_variables(&f, 1, 0, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 3);
    assert_eq!(
        (
            result[0].name.as_str(),
            result[0].value.as_str(),
            result[0].ty.as_str()
        ),
        ("x", "42", "int")
    );
    assert_eq!(
        (
            result[1].name.as_str(),
            result[1].value.as_str(),
            result[1].ty.as_str()
        ),
        ("y", "3.14", "float64")
    );
    assert_eq!(
        (
            result[2].name.as_str(),
            result[2].value.as_str(),
            result[2].ty.as_str()
        ),
        ("name", "\"hello\"", "string")
    );
}

#[tokio::test]
async fn filter_top_level_case_insensitive() {
    // Go TestFlattenVariablesFilter.
    let f = fetcher(|_| {
        vec![
            var("count", "10", "int", 0),
            var("Counter", "20", "int", 0),
            var("name", "\"test\"", "string", 0),
            var("myCount", "30", "int", 0),
        ]
    });

    let (result, truncated) = flatten_variables(&f, 1, 0, 100, "count").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].name, "count");
    assert_eq!(result[1].name, "Counter");
    assert_eq!(result[2].name, "myCount");
}

#[tokio::test]
async fn has_children_at_depth_zero() {
    // Go TestFlattenVariablesHasChildrenAtDepthZero.
    let f = fetcher(|_| {
        vec![
            var("x", "1", "int", 0),
            var_named("myStruct", "{...}", "MyStruct", 5, 3),
        ]
    });

    let (result, truncated) = flatten_variables(&f, 1, 0, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 2);
    assert!(!result[0].has_children, "leaf should not have children");
    assert!(
        result[1].has_children,
        "struct should have children at depth 0"
    );
    assert_eq!(result[1].children_count, 3);
}

#[tokio::test]
async fn recursive_one_level() {
    // Go TestFlattenVariablesRecursive.
    let f = fetcher(|vref| match vref {
        1 => vec![
            var("x", "1", "int", 0),
            var_named("point", "{x:10, y:20}", "Point", 2, 2),
        ],
        2 => vec![var("x", "10", "int", 0), var("y", "20", "int", 0)],
        _ => vec![],
    });

    let (result, truncated) = flatten_variables(&f, 1, 1, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 4);
    assert_eq!(
        (result[0].name.as_str(), result[0].value.as_str()),
        ("x", "1")
    );
    assert_eq!(
        (result[1].name.as_str(), result[1].value.as_str()),
        ("point", "{x:10, y:20}")
    );
    assert_eq!(
        (result[2].name.as_str(), result[2].value.as_str()),
        ("point.x", "10")
    );
    assert_eq!(
        (result[3].name.as_str(), result[3].value.as_str()),
        ("point.y", "20")
    );
}

#[tokio::test]
async fn deep_recursion() {
    // Go TestFlattenVariablesDeepRecursion — three levels, depth=2.
    let f = fetcher(|vref| match vref {
        1 => vec![var_named("root", "{...}", "Root", 2, 1)],
        2 => vec![var_named("child", "{...}", "Child", 3, 1)],
        3 => vec![var("leaf", "42", "int", 0)],
        _ => vec![],
    });

    let (result, truncated) = flatten_variables(&f, 1, 2, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].name, "root");
    assert_eq!(result[1].name, "root.child");
    assert_eq!(
        (result[2].name.as_str(), result[2].value.as_str()),
        ("root.child.leaf", "42")
    );
}

#[tokio::test]
async fn depth_limit_stops_recursion() {
    // Go TestFlattenVariablesDepthLimitStopsRecursion — depth=1 must not reach ref 3.
    let f = fetcher(|vref| match vref {
        1 => vec![var_named("root", "{...}", "Root", 2, 1)],
        2 => vec![var_named("nested", "{...}", "Nested", 3, 2)],
        3 => panic!("should not recurse to depth 3 when depth=1"),
        _ => vec![],
    });

    let (result, truncated) = flatten_variables(&f, 1, 1, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 2);
    assert_eq!(result[1].name, "root.nested");
    assert!(
        result[1].has_children,
        "expected has_children at depth limit"
    );
    assert_eq!(result[1].children_count, 2);
}

#[tokio::test]
async fn truncation_top_level() {
    // Go TestFlattenVariablesTruncation — maxCount=3 over 5 leaves.
    let f = fetcher(|_| {
        vec![
            var("a", "1", "int", 0),
            var("b", "2", "int", 0),
            var("c", "3", "int", 0),
            var("d", "4", "int", 0),
            var("e", "5", "int", 0),
        ]
    });

    let (result, truncated) = flatten_variables(&f, 1, 0, 3, "").await.unwrap();
    assert!(truncated);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].name, "a");
    assert_eq!(result[1].name, "b");
    assert_eq!(result[2].name, "c");
}

#[tokio::test]
async fn truncation_during_recursion() {
    // Go TestFlattenVariablesTruncationDuringRecursion — a, s, s.f1 then truncate.
    let f = fetcher(|vref| match vref {
        1 => vec![var("a", "1", "int", 0), var_named("s", "{...}", "S", 2, 3)],
        2 => vec![
            var("f1", "10", "int", 0),
            var("f2", "20", "int", 0),
            var("f3", "30", "int", 0),
        ],
        _ => vec![],
    });

    let (result, truncated) = flatten_variables(&f, 1, 1, 3, "").await.unwrap();
    assert!(truncated);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].name, "a");
    assert_eq!(result[1].name, "s");
    assert_eq!(result[2].name, "s.f1");
}

#[tokio::test]
async fn filter_not_applied_to_children() {
    // Go TestFlattenVariablesFilterNotAppliedToChildren.
    let f = fetcher(|vref| match vref {
        1 => vec![
            var_named("point", "{...}", "Point", 2, 2),
            var("count", "5", "int", 0),
        ],
        2 => vec![var("x", "10", "int", 0), var("y", "20", "int", 0)],
        _ => vec![],
    });

    let (result, truncated) = flatten_variables(&f, 1, 1, 100, "point").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].name, "point");
    assert_eq!(result[1].name, "point.x");
    assert_eq!(result[2].name, "point.y");
}

#[tokio::test]
async fn empty_result() {
    // Go TestFlattenVariablesEmptyResult.
    let f = fetcher(|_| vec![]);
    let (result, truncated) = flatten_variables(&f, 1, 0, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 0);
}

#[tokio::test]
async fn filter_no_match() {
    // Go TestFlattenVariablesFilterNoMatch.
    let f = fetcher(|_| vec![var("x", "1", "int", 0), var("y", "2", "int", 0)]);
    let (result, truncated) = flatten_variables(&f, 1, 0, 100, "zzz").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 0);
}

#[tokio::test]
async fn children_count_indexed_and_named() {
    // Go TestFlattenVariablesChildrenCountIndexedAndNamed — named(1) + indexed(10) = 11.
    let f = fetcher(|_| {
        vec![Variable {
            name: "arr".to_string(),
            value: "[...]".to_string(),
            ty: "[]int".to_string(),
            variables_reference: 5,
            named: 1,
            indexed: 10,
        }]
    });

    let (result, truncated) = flatten_variables(&f, 1, 0, 100, "").await.unwrap();
    assert!(!truncated);
    assert_eq!(result.len(), 1);
    assert!(result[0].has_children);
    assert_eq!(result[0].children_count, 11);
}

#[test]
fn flat_variable_json_omitempty() {
    // Go TestFlatVariableJSONMarshal — type/has_children/children_count omitempty.
    let basic = FlatVariable {
        name: "x".into(),
        value: "42".into(),
        ty: "int".into(),
        has_children: false,
        children_count: 0,
    };
    let v = serde_json::to_value(&basic).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "name": "x", "value": "42", "type": "int" })
    );

    let with_children = FlatVariable {
        name: "myStruct".into(),
        value: "{...}".into(),
        ty: "MyStruct".into(),
        has_children: true,
        children_count: 3,
    };
    let v = serde_json::to_value(&with_children).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "name": "myStruct",
            "value": "{...}",
            "type": "MyStruct",
            "has_children": true,
            "children_count": 3
        })
    );

    let omit_type = FlatVariable {
        name: "val".into(),
        value: "hello".into(),
        ty: String::new(),
        has_children: false,
        children_count: 0,
    };
    let v = serde_json::to_value(&omit_type).unwrap();
    assert_eq!(v, serde_json::json!({ "name": "val", "value": "hello" }));

    let leaf = FlatVariable {
        name: "leaf".into(),
        value: "true".into(),
        ty: "bool".into(),
        has_children: false,
        children_count: 0,
    };
    let v = serde_json::to_value(&leaf).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "name": "leaf", "value": "true", "type": "bool" })
    );
}
