//! Network egress policy.
//!
//! v1 model: egress only, no ingress (process backends do not listen).
//!
//! Semantics (documented contract, enforced by every backend):
//! - `default` deny is the baseline; `allow` targets open holes.
//! - Where both sets mention overlapping targets, allow wins over deny (E2B
//!   semantics), **except** for the protected ranges below.
//! - **Protected ranges can never be allowed**, no matter what the caller asks
//!   for. This is the one place where caller intent is overridden, and it is a
//!   deliberate fail-closed decision (CubeSandbox `netpolicy.go` and
//!   microsandbox make the same choice): private networks, link-local, and
//!   cloud metadata endpoints are where sandbox escapes and credential theft
//!   live.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default posture for egress traffic. Wire spelling is lowercase to match
/// the rest of the JSON contract (`deny`/`allow`, as in the OpenAPI enum).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressDefault {
    #[default]
    Deny,
    Allow,
}

/// One egress target: an IP range or a (possibly wildcarded) hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    Cidr(IpNet),
    Host(Domain),
}

/// An egress rule target with its kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetTarget {
    /// CIDR (`10.0.0.0/8`) or hostname (`example.com`, `*.example.com`).
    pub target: String,
}

/// A validated hostname pattern: lowercase, no scheme/port, wildcard only as a
/// leading `*.` label.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Domain(String);

impl Domain {
    /// Validate and normalize a hostname pattern.
    ///
    /// Allowed: `example.com`, `api.example.com`, `*.example.com`.
    /// Rejected: `*` alone, `*.` prefix without a suffix, embedded wildcards,
    /// schemes, ports, empty labels, `..`, non-ASCII hosts.
    pub fn parse(raw: &str) -> Result<Self> {
        let host = raw.trim().to_ascii_lowercase();
        if host.is_empty() {
            return Err(Error::Policy("domain must not be empty".into()));
        }
        if host.contains("://") || host.contains('/') {
            return Err(Error::Policy(format!(
                "domain must not contain a scheme or path: {raw:?}"
            )));
        }
        if host.contains(':') {
            return Err(Error::Policy(format!(
                "domain must not contain a port: {raw:?}"
            )));
        }
        // Walk labels; a wildcard may only be the first label, written `*`.
        let labels: Vec<&str> = host.split('.').collect();
        for (i, label) in labels.iter().enumerate() {
            if *label == "*" {
                if i != 0 {
                    return Err(Error::Policy(format!(
                        "wildcard is only allowed as the first label: {raw:?}"
                    )));
                }
                if labels.len() < 3 {
                    return Err(Error::Policy(format!(
                        "wildcard needs at least two more labels (use *.example.com): {raw:?}"
                    )));
                }
                continue;
            }
            if label.is_empty() {
                return Err(Error::Policy(format!("empty label in domain: {raw:?}")));
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(Error::Policy(format!(
                    "domain labels may contain only [a-z0-9-]: {raw:?}"
                )));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(Error::Policy(format!(
                    "domain labels must not start or end with '-': {raw:?}"
                )));
            }
        }
        if labels.len() < 2 {
            return Err(Error::Policy(format!(
                "domain needs at least two labels (got {raw:?})"
            )));
        }
        Ok(Self(host))
    }

    /// True if this pattern matches `host` (exact, or wildcard suffix match).
    pub fn matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if let Some(suffix) = self.0.strip_prefix("*.") {
            // Wildcard matches exactly one or more leading labels, but never
            // the bare suffix itself: `*.example.com` does not match
            // `example.com`. Matches `a.example.com`, `a.b.example.com`.
            host.ends_with(&format!(".{suffix}"))
        } else {
            self.0 == host
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Egress policy as submitted by a caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetPolicy {
    #[serde(default)]
    pub egress_default: EgressDefault,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<NetTarget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<NetTarget>,
    /// Route all egress through the service's credential-injection proxy.
    /// The workload never talks to the internet directly: the backend opens
    /// exactly one hole — the proxy endpoint — and injects proxy env vars.
    /// User allow/deny lists and an allow default are meaningless here and
    /// rejected: credentials live in the proxy, not in the policy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proxy: bool,
}

/// Normalized egress policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNetPolicy {
    pub egress_default: EgressDefault,
    pub allow: Vec<TargetKind>,
    pub deny: Vec<TargetKind>,
    /// Ranges that are always denied and can never be allowed.
    pub protected: Vec<IpNet>,
    /// Proxy mode: direct egress is replaced by the single proxy hole.
    pub proxy: bool,
}

/// Ranges no policy may open. Kept as one table so tests and enforcement agree.
pub const PROTECTED_RANGES_V4: &[&str] = &[
    "0.0.0.0/8",      // "this network"
    "10.0.0.0/8",     // RFC1918 private
    "100.64.0.0/10",  // CGNAT shared address space
    "127.0.0.0/8",    // loopback
    "169.254.0.0/16", // link-local (includes cloud metadata endpoints)
    "172.16.0.0/12",  // RFC1918 private
    "192.168.0.0/16", // RFC1918 private
    "224.0.0.0/4",    // multicast
    "240.0.0.0/4",    // reserved
];

pub const PROTECTED_RANGES_V6: &[&str] = &["::1/128", "fc00::/7", "fe80::/10"];

/// Parse a CIDR or bare IP into an `IpNet`.
pub fn parse_target_net(raw: &str) -> Result<IpNet> {
    if raw.contains('/') {
        IpNet::from_str(raw).map_err(|e| Error::Policy(format!("invalid CIDR {raw:?}: {e}")))
    } else {
        let ip = IpAddr::from_str(raw)
            .map_err(|_| Error::Policy(format!("invalid IP address {raw:?}")))?;
        IpNet::new(ip, if ip.is_ipv4() { 32 } else { 128 })
            .map_err(|e| Error::Policy(format!("invalid IP {raw:?}: {e}")))
    }
}

/// True if `net` overlaps any protected range. Proper CIDR subnets are
/// tree-structured, so partial overlap always implies containment in one
/// direction; checking both directions also covers equality.
fn hits_protected(net: &IpNet, protected: &[IpNet]) -> bool {
    protected.iter().any(|p| p.contains(net) || net.contains(p))
}

impl NetPolicy {
    /// Validate and normalize into [`ResolvedNetPolicy`].
    pub fn resolve(&self) -> Result<ResolvedNetPolicy> {
        let mut protected = Vec::new();
        for raw in PROTECTED_RANGES_V4.iter().chain(PROTECTED_RANGES_V6.iter()) {
            protected.push(IpNet::from_str(raw).expect("built-in protected range must parse"));
        }

        let mut allow = Vec::new();
        for target in &self.allow {
            allow.push(resolve_target(
                &target.target,
                &protected,
                /* is_allow */ true,
            )?);
        }
        let mut deny = Vec::new();
        for target in &self.deny {
            deny.push(resolve_target(
                &target.target,
                &protected,
                /* is_allow */ false,
            )?);
        }

        // Proxy mode replaces direct egress entirely; mixing it with target
        // lists would imply two enforcement paths for one sandbox.
        if self.proxy
            && (!self.allow.is_empty()
                || !self.deny.is_empty()
                || self.egress_default == EgressDefault::Allow)
        {
            return Err(Error::Policy(
                "net.proxy replaces direct egress; allow/deny lists and an allow default are rejected under it"
                    .into(),
            ));
        }

        // Allow-vs-deny overlap is legal (allow wins at enforcement); protected
        // overlap in `allow` is not. `deny` may restate protected ranges.
        Ok(ResolvedNetPolicy {
            egress_default: self.egress_default,
            allow,
            deny,
            protected,
            proxy: self.proxy,
        })
    }
}

fn resolve_target(raw: &str, protected: &[IpNet], is_allow: bool) -> Result<TargetKind> {
    // A target is a CIDR/IP iff it parses as one; otherwise it is a hostname.
    let looks_like_net = raw.contains('/') || raw.parse::<IpAddr>().is_ok();
    if looks_like_net {
        let net = parse_target_net(raw)?;
        if is_allow && hits_protected(&net, protected) {
            return Err(Error::Policy(format!(
                "target {raw:?} overlaps a protected range that can never be allowed"
            )));
        }
        Ok(TargetKind::Cidr(net))
    } else {
        Ok(TargetKind::Host(Domain::parse(raw)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(allow: &[&str], deny: &[&str], default: EgressDefault) -> Result<ResolvedNetPolicy> {
        NetPolicy {
            egress_default: default,
            proxy: false,
            allow: allow
                .iter()
                .map(|t| NetTarget {
                    target: t.to_string(),
                })
                .collect(),
            deny: deny
                .iter()
                .map(|t| NetTarget {
                    target: t.to_string(),
                })
                .collect(),
        }
        .resolve()
    }

    #[test]
    fn protected_table_parses() {
        // The constant table is enforcement-critical; it must always parse.
        for raw in PROTECTED_RANGES_V4.iter().chain(PROTECTED_RANGES_V6.iter()) {
            assert!(
                IpNet::from_str(raw).is_ok(),
                "protected range {raw} must parse"
            );
        }
    }

    #[test]
    fn bare_ip_becomes_host_net() {
        let resolved = resolve(&["1.2.3.4"], &[], EgressDefault::Deny).unwrap();
        assert_eq!(
            resolved.allow,
            vec![TargetKind::Cidr(IpNet::from_str("1.2.3.4/32").unwrap())]
        );
    }

    #[test]
    fn cidr_parses() {
        let resolved = resolve(&["203.0.113.0/24"], &[], EgressDefault::Deny).unwrap();
        assert_eq!(
            resolved.allow,
            vec![TargetKind::Cidr(IpNet::from_str("203.0.113.0/24").unwrap())]
        );
    }

    #[test]
    fn malformed_cidr_is_rejected() {
        let err = resolve(&["203.0.113.0/33"], &[], EgressDefault::Deny).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
        let err = resolve(&["not-an-ip/24"], &[], EgressDefault::Deny).unwrap_err();
        assert_eq!(err.code(), "policy_validation");
    }

    #[test]
    fn private_ranges_can_never_be_allowed() {
        for raw in [
            "10.0.0.0/8",
            "192.168.1.0/24",
            "172.16.0.0/12",
            "169.254.169.254/32",
            "127.0.0.1",
            "0.0.0.0/0",
        ] {
            let err = resolve(&[raw], &[], EgressDefault::Deny).unwrap_err();
            assert_eq!(
                err.code(),
                "policy_validation",
                "allowing {raw} must be rejected"
            );
            assert!(
                err.to_string().contains("protected"),
                "error names the reason: {err}"
            );
        }
    }

    #[test]
    fn deny_list_may_restate_protected_ranges() {
        // Explicitly denying protected ranges is redundant but legal.
        let resolved = resolve(
            &["example.com"],
            &["169.254.169.254/32"],
            EgressDefault::Deny,
        )
        .unwrap();
        assert!(!resolved.proxy);
        assert!(resolved.deny.contains(&TargetKind::Cidr(
            IpNet::from_str("169.254.169.254/32").unwrap()
        )));
    }

    #[test]
    fn domain_forms_accepted() {
        for raw in ["example.com", "api.example.com", "*.example.com"] {
            assert!(Domain::parse(raw).is_ok(), "{raw} must parse");
        }
    }

    #[test]
    fn hostile_domain_forms_rejected() {
        for raw in [
            "*",            // wildcard alone
            "*.",           // wildcard without suffix
            "a.*.com",      // embedded wildcard
            "*.example",    // wildcard needs two more labels
            "example",      // single label
            "",             // empty
            "http://x.com", // scheme
            "x.com:443",    // port
            "a..com",       // empty label
            "-bad.com",     // leading dash
            "bad-.com",     // trailing dash
        ] {
            let err = Domain::parse(raw).unwrap_err();
            assert_eq!(err.code(), "policy_validation", "{raw:?} must be rejected");
        }
    }

    #[test]
    fn domain_is_normalized_lowercase() {
        let d = Domain::parse("API.Example.COM").unwrap();
        assert_eq!(d.as_str(), "api.example.com");
    }

    #[test]
    fn wildcard_matches_subdomains_not_bare_suffix() {
        let d = Domain::parse("*.example.com").unwrap();
        assert!(d.matches("api.example.com"));
        assert!(d.matches("a.b.example.com"));
        assert!(!d.matches("example.com"));
        assert!(!d.matches("notexample.com"));
    }

    #[test]
    fn exact_domain_matches_only_itself() {
        let d = Domain::parse("example.com").unwrap();
        assert!(d.matches("example.com"));
        assert!(!d.matches("api.example.com"));
    }

    #[test]
    fn proxy_mode_rejects_target_lists_and_allow_default() {
        let policy = NetPolicy {
            proxy: true,
            allow: vec![NetTarget {
                target: "example.com".into(),
            }],
            ..Default::default()
        };
        let err = policy.resolve().unwrap_err();
        assert_eq!(err.code(), "policy_validation");

        let policy = NetPolicy {
            proxy: true,
            egress_default: EgressDefault::Allow,
            ..Default::default()
        };
        assert_eq!(policy.resolve().unwrap_err().code(), "policy_validation");

        // Bare proxy mode with deny default is the valid form.
        let resolved = NetPolicy {
            proxy: true,
            ..Default::default()
        }
        .resolve()
        .unwrap();
        assert!(resolved.proxy);
    }

    #[test]
    fn default_deny_is_the_zero_value() {
        let policy = NetPolicy::default();
        assert_eq!(policy.egress_default, EgressDefault::Deny);
        assert!(policy.allow.is_empty() && policy.deny.is_empty());
    }
}
