//! Contract tests: the OpenAPI spec (docs/openapi.yaml) must stay in lockstep
//! with the served route table and the error-code vocabulary. Drift in any
//! direction fails the gate — the spec is the surface clients build against,
//! and an undocumented or stale route is a contract break.
//!
//! The spec is parsed with a minimal structure-aware scanner (2-space path
//! keys under `paths:`, 4-space method keys under each path) instead of a
//! YAML dependency: the file style is fixed by this test, and style drift
//! fails loudly rather than silently degrading the check.

use std::collections::BTreeSet;

const SPEC: &str = include_str!("../../../docs/openapi.yaml");

/// (method, path) pairs of the served surface — the router's own table.
fn table() -> Vec<(&'static str, &'static str)> {
    sandbox_api::routes::route_table()
        .into_iter()
        .map(|(method, path, _)| (method, path))
        .collect()
}

/// Extract `paths:` entries: a map of path → set of methods, from exactly
/// two indentation levels of the hand-written spec.
fn spec_routes() -> Vec<(String, BTreeSet<String>)> {
    let mut inside_paths = false;
    let mut current: Option<(String, BTreeSet<String>)> = None;
    let mut out = Vec::new();

    for line in SPEC.lines() {
        if line == "paths:" {
            inside_paths = true;
            continue;
        }
        if !inside_paths {
            continue;
        }
        // A new top-level section (no indentation) ends the paths block.
        if !line.starts_with(' ') && !line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("  /") {
            if let Some(entry) = current.take() {
                out.push(entry);
            }
            let path = rest
                .strip_suffix(':')
                .expect("path lines end with ':' — fixed spec style");
            current = Some((format!("/{path}"), BTreeSet::new()));
        } else if let Some(rest) = line.strip_prefix("    ") {
            let method = rest
                .split(':')
                .next()
                .expect("method lines end with ':' — fixed spec style")
                .trim()
                .to_ascii_uppercase();
            if matches!(method.as_str(), "GET" | "POST" | "DELETE" | "PUT" | "PATCH") {
                let entry = current
                    .as_mut()
                    .expect("method nested under a path — fixed spec style");
                entry.1.insert(method);
            }
        }
    }
    if let Some(entry) = current.take() {
        out.push(entry);
    }
    out
}

/// Error codes declared under `components.errorCodes` (lists of `- code`).
fn spec_error_codes() -> Vec<String> {
    let mut inside = false;
    let mut codes = Vec::new();
    for line in SPEC.lines() {
        if line.starts_with("  errorCodes:") {
            inside = true;
            continue;
        }
        if inside {
            if !line.starts_with(' ') && !line.is_empty() {
                break;
            }
            if let Some(code) = line.trim().strip_prefix("- ") {
                codes.push(code.trim().to_string());
            }
        }
    }
    codes
}

/// The full code vocabulary, built from the models themselves.
fn domain_error_codes() -> Vec<&'static str> {
    use sandbox_core::Error;
    use sandbox_core::error::BackendFailureKind;
    vec![
        Error::Policy(String::new()).code(),
        Error::Lifecycle(String::new()).code(),
        Error::Budget(String::new()).code(),
        Error::Backend {
            kind: BackendFailureKind::Spawn,
            message: String::new(),
        }
        .code(),
        Error::Backend {
            kind: BackendFailureKind::Kill,
            message: String::new(),
        }
        .code(),
        Error::Backend {
            kind: BackendFailureKind::Io,
            message: String::new(),
        }
        .code(),
        Error::Backend {
            kind: BackendFailureKind::Other,
            message: String::new(),
        }
        .code(),
        Error::Internal(String::new()).code(),
    ]
}

fn registry_error_codes() -> Vec<&'static str> {
    vec![
        "sandbox_not_found",
        "backend_unavailable",
        "unauthorized",
        "payload_too_large",
    ]
}

#[test]
fn every_route_is_documented_with_matching_methods() {
    let spec: Vec<(String, BTreeSet<String>)> = spec_routes();
    assert!(
        !spec.is_empty(),
        "spec has no paths — parser or file broken"
    );
    for (method, path) in table() {
        let entry = spec
            .iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("route {method} {path} missing from openapi.yaml"));
        assert!(
            entry.1.contains(method),
            "route {method} {path} documented in spec only as {:?}",
            entry.1
        );
    }
}

#[test]
fn every_documented_route_is_served() {
    let served: BTreeSet<(String, String)> = table()
        .into_iter()
        .map(|(m, p)| (p.to_string(), m.to_string()))
        .collect();
    for (path, methods) in spec_routes() {
        for method in methods {
            assert!(
                served.contains(&(path.clone(), method.clone())),
                "spec documents {method} {path} but nothing serves it"
            );
        }
    }
}

#[test]
fn every_error_code_is_documented() {
    let declared: BTreeSet<String> = spec_error_codes().into_iter().collect();
    assert!(!declared.is_empty(), "spec declares no error codes");
    for code in domain_error_codes()
        .into_iter()
        .chain(registry_error_codes())
    {
        assert!(
            declared.contains(code),
            "error code {code:?} not documented in openapi.yaml errorCodes"
        );
    }
}

#[test]
fn declared_error_codes_are_exactly_the_vocabulary() {
    let mut declared = spec_error_codes();
    declared.sort();
    let mut expected: Vec<String> = domain_error_codes()
        .into_iter()
        .chain(registry_error_codes())
        .map(str::to_string)
        .collect();
    expected.sort();
    assert_eq!(
        declared, expected,
        "spec error codes drifted from the closed vocabulary"
    );
}

#[test]
fn spec_version_is_a_bumpable_semver() {
    let version_line = SPEC
        .lines()
        .find(|l| l.trim_start().starts_with("version:"))
        .expect("spec carries a version");
    let version = version_line
        .trim()
        .strip_prefix("version:")
        .expect("version: key")
        .trim();
    let digits: Vec<&str> = version.split('.').collect();
    assert_eq!(
        digits.len(),
        3,
        "version must be semver (major.minor.patch), got {version:?}"
    );
    for part in digits {
        assert!(
            part.parse::<u64>().is_ok(),
            "version parts must be numeric, got {version:?}"
        );
    }
}

/// Extract the property names of a named schema from `components.schemas`.
/// Handles the two styles this spec uses: a direct `properties:` block and an
/// `allOf:` whose object arm carries the properties (flattened views). Style
/// drift fails loudly: a schema with no recognized property block panes the
/// test instead of passing vacuously.
fn schema_properties(name: &str) -> std::collections::BTreeSet<String> {
    let header = format!("    {name}:");
    let lines: Vec<&str> = SPEC.lines().collect();
    let start = lines
        .iter()
        .position(|l| *l == header)
        .unwrap_or_else(|| panic!("schema {name} must exist in the spec"));
    let mut props = std::collections::BTreeSet::new();
    for line in lines.iter().skip(start + 1) {
        // Next 4-space schema key ends this schema's block.
        if line.starts_with("    ") && !line.starts_with("      ") {
            break;
        }
        // Direct style: 8-space `key:` under `      properties:`.
        let direct = line.strip_prefix("        ");
        // allOf style: 12-space `key:` under `          properties:`.
        let flattened = line.strip_prefix("            ");
        for candidate in [direct, flattened].into_iter().flatten() {
            if let Some(key) = candidate
                .split(':')
                .next()
                .filter(|k| k.chars().all(|c| c.is_ascii_alphanumeric()))
            {
                props.insert(key.to_string());
            }
        }
    }
    assert!(
        !props.is_empty(),
        "schema {name} must declare properties; the extraction style drifted"
    );
    props
}

/// The serialized shape of a model must equal the documented schema property
/// set — both directions: a field the code writes but the spec omits is an
/// undocumented contract break; a spec field the code never writes is a lie
/// clients will code against.
#[test]
fn usage_ledger_schema_matches_serialization() {
    use sandbox_api::ledger::UsageLedger;
    let ledger = UsageLedger::new(1);
    let written: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&serde_json::to_string(&ledger).expect("serializes")).expect("object");
    assert_eq!(
        written
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        schema_properties("UsageLedger")
    );
}

#[test]
fn exec_outcome_schema_matches_serialization() {
    let outcome = sandbox_core::ExecOutcome {
        exit_code: Some(0),
        timed_out: false,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
    };
    let written: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&serde_json::to_string(&outcome).expect("serializes"))
            .expect("object");
    assert_eq!(
        written
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        schema_properties("ExecOutcome")
    );
}

/// Extract property names of a nested section (`net`, `fs`, …) inside a
/// top-level schema: `    Schema:` → `        section:` → `          properties:`
/// → 12-space `key:` entries, until the block dedents.
fn nested_schema_properties(schema: &str, section: &str) -> std::collections::BTreeSet<String> {
    let lines: Vec<&str> = SPEC.lines().collect();
    let schema_header = format!("    {schema}:");
    let schema_start = lines
        .iter()
        .position(|l| *l == schema_header)
        .unwrap_or_else(|| panic!("schema {schema} must exist in the spec"));
    let section_header = format!("        {section}:");
    let section_start = lines[schema_start..]
        .iter()
        .position(|l| *l == section_header)
        .unwrap_or_else(|| panic!("schema {schema} must have a {section} section"))
        + schema_start;
    let mut props = std::collections::BTreeSet::new();
    for line in lines.iter().skip(section_start + 1) {
        if line.starts_with("        ") && !line.starts_with("          ") {
            break; // next section of the schema
        }
        if let Some(key) = line
            .strip_prefix("            ")
            .and_then(|rest| rest.split(':').next())
            .filter(|k| k.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            props.insert(key.to_string());
        }
    }
    assert!(
        !props.is_empty(),
        "schema {schema}.{section} must declare properties; the extraction style drifted"
    );
    props
}

#[test]
fn net_policy_schema_matches_serialization() {
    use sandbox_core::policy::{EgressDefault, NetPolicy, NetTarget};
    // Every field populated: skip_serializing_if fields appear, so the
    // comparison is exact equality in both directions.
    let policy = NetPolicy {
        egress_default: EgressDefault::Deny,
        allow: vec![NetTarget {
            target: "example.com".into(),
        }],
        deny: vec![NetTarget {
            target: "203.0.113.0/24".into(),
        }],
        proxy: true,
    };
    let written: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&serde_json::to_string(&policy).expect("serializes")).expect("object");
    assert_eq!(
        written
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        nested_schema_properties("SandboxSpec", "net")
    );
}

#[test]
fn budget_schema_matches_serialization() {
    let budget = sandbox_core::Budget {
        memory_mb: Some(512),
        pids_max: Some(64),
    };
    let written: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&serde_json::to_string(&budget).expect("serializes")).expect("object");
    assert_eq!(
        written
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        nested_schema_properties("SandboxSpec", "budget")
    );
}

/// include_str! pins the spec to docs/openapi.yaml — moving it breaks the
/// build, not silently the contract. This asserts the parsed file is really
/// the OpenAPI document, not an empty stub.
#[test]
fn spec_is_the_openapi_document() {
    assert!(
        SPEC.contains("openapi: 3.0.3"),
        "spec must declare OpenAPI 3.0.3"
    );
    assert!(SPEC.contains("title: b-sandbox API"), "{SPEC}");
}
