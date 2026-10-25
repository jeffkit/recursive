//! Opaque trace labels — the edge half of the identity-opacity ruling
//! (Goal 430, issue #244 W1; design source #146).
//!
//! Owner ruling (AGENTS.md, 2026-10-18 / 2026-10-25): the kernel never sees
//! identity; the application edge may *forward* opaque key/value labels for
//! observability, and the layers below carry them without reading a single
//! key; what a key means is decided only at the edge (HTTP handlers,
//! observability backends, skill sources). This module is that edge: labels
//! are **derived, validated and expanded** here and nowhere else.
//! `tests/identity_opacity_contract.rs` pins that the carriers downstream
//! (`observability::RunMeta` → collector) never mention a key by name.
//!
//! W1 wires one consumer: [`from_session_owner`] → `RunMeta::with_labels` →
//! `langfuse.trace.metadata.label.<key>`. [`expand_template`] is defined
//! here for the W2 consumers (per-session skill-source URL and endpoint
//! headers, Goal 431) and deliberately has no caller yet.
//!
//! Safety constraints carried over from #146 §三: labels derive **only** from
//! the verified `AuthIdentity` (no request-body or inbound-header entry
//! point); `subject` is not in the default set (PII — `session_id` already
//! links back to the owner); a violated constraint is an `Err` naming the key
//! and the rule, never the value.

use std::collections::BTreeMap;
use std::fmt;

/// Maximum number of labels on one session.
pub const MAX_LABELS: usize = 16;
/// Maximum key length in bytes (keys match `^[a-z][a-z0-9_]{0,63}$`).
pub const MAX_KEY_LEN: usize = 64;
/// Maximum value length in bytes.
pub const MAX_VALUE_BYTES: usize = 256;

/// Validated opaque `key → value` labels, sorted for a stable attribute
/// order. Construct through [`OpaqueLabels::try_from_pairs`] /
/// [`TryFrom<BTreeMap>`] — every constraint is enforced there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpaqueLabels(BTreeMap<String, String>);

/// The constraint a rejected label broke. Carries sizes, never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelsReason {
    /// Key is empty, does not start with `a-z`, or has a char outside `a-z0-9_`.
    KeyCharset,
    /// Key exceeds [`MAX_KEY_LEN`] bytes.
    KeyTooLong {
        /// Measured key length in bytes.
        len: usize,
    },
    /// Value exceeds [`MAX_VALUE_BYTES`] bytes.
    ValueTooLong {
        /// Measured value length in bytes.
        len: usize,
    },
    /// Value contains a control character (CR / LF included — one rule, no
    /// header-injection special cases).
    ValueControlChar,
    /// More than [`MAX_LABELS`] keys.
    TooMany {
        /// Number of keys supplied.
        count: usize,
    },
}

impl fmt::Display for LabelsReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyCharset => write!(f, "key must match ^[a-z][a-z0-9_]{{0,63}}$"),
            Self::KeyTooLong { len } => write!(f, "key is {len} bytes (max {MAX_KEY_LEN})"),
            Self::ValueTooLong { len } => {
                write!(f, "value is {len} bytes (max {MAX_VALUE_BYTES})")
            }
            Self::ValueControlChar => write!(f, "value contains a control character"),
            Self::TooMany { count } => write!(f, "{count} labels (max {MAX_LABELS})"),
        }
    }
}

/// A rejected label: the offending key and the constraint it broke. The
/// value is deliberately absent — it may be identity data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelsError {
    /// The offending key (empty when the key itself was empty).
    pub key: String,
    /// The constraint that failed.
    pub reason: LabelsReason,
}

impl fmt::Display for LabelsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "label `{}` rejected: {}", self.key, self.reason)
    }
}

impl std::error::Error for LabelsError {}

fn check_key(key: &str) -> Result<(), LabelsReason> {
    if key.len() > MAX_KEY_LEN {
        return Err(LabelsReason::KeyTooLong { len: key.len() });
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return Err(LabelsReason::KeyCharset),
    }
    if chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        Ok(())
    } else {
        Err(LabelsReason::KeyCharset)
    }
}

fn check_value(value: &str) -> Result<(), LabelsReason> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(LabelsReason::ValueTooLong { len: value.len() });
    }
    if value.chars().any(char::is_control) {
        return Err(LabelsReason::ValueControlChar);
    }
    Ok(())
}

impl TryFrom<BTreeMap<String, String>> for OpaqueLabels {
    type Error = LabelsError;

    fn try_from(map: BTreeMap<String, String>) -> Result<Self, LabelsError> {
        if map.len() > MAX_LABELS {
            return Err(LabelsError {
                key: map.keys().nth(MAX_LABELS).cloned().unwrap_or_default(),
                reason: LabelsReason::TooMany { count: map.len() },
            });
        }
        for (key, value) in &map {
            check_key(key)
                .and_then(|()| check_value(value))
                .map_err(|reason| LabelsError {
                    key: key.clone(),
                    reason,
                })?;
        }
        Ok(Self(map))
    }
}

impl OpaqueLabels {
    /// Validate and collect `(key, value)` pairs. A key given twice keeps
    /// its last value (map semantics); the cap counts distinct keys.
    pub fn try_from_pairs<K, V>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self, LabelsError>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let map: BTreeMap<String, String> = pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self::try_from(map)
    }

    /// Value of `key`, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Number of labels.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no labels.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Sorted `(key, value)` view.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

impl IntoIterator for OpaqueLabels {
    type Item = (String, String);
    type IntoIter = std::collections::btree_map::IntoIter<String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// The W1 default label set, derived from the session owner: `tenant` when
/// the owner has one, nothing otherwise — an untenanted session is simply
/// unlabelled, not an error (a W2 consumer whose template *needs* the key
/// fails closed on its own, Goal 431). `subject` is deliberately excluded.
///
/// Derived from a verified identity this cannot normally fail; an `Err`
/// means the tenant value itself is out of bounds and the caller must say
/// so (WARN naming key + rule) rather than drop the label silently.
pub fn from_session_owner(tenant: Option<&str>) -> Result<OpaqueLabels, LabelsError> {
    match tenant {
        Some(tenant) => OpaqueLabels::try_from_pairs([("tenant", tenant)]),
        None => Ok(OpaqueLabels::default()),
    }
}

/// A template placeholder naming a label the session does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLabel {
    /// The placeholder text between the braces.
    pub key: String,
}

impl fmt::Display for MissingLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template references missing label `{}`", self.key)
    }
}

impl std::error::Error for MissingLabel {}

/// Expand `{key}` placeholders in an operator template from `labels`.
///
/// `{{` and `}}` produce literal braces; a placeholder whose key the labels
/// do not carry is an `Err` (fail-closed — never an empty substitution). An
/// unterminated `{…` is reported as a missing label named by the trailing
/// text; a lone `}` is kept literally. Pure and edge-only: W2 consumers call
/// it while *building* a per-session tool registry, so tools downstream only
/// ever receive the expanded, concrete string.
pub fn expand_template(tpl: &str, labels: &OpaqueLabels) -> Result<String, MissingLabel> {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(pos) = rest.find(['{', '}']) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix("{{") {
            out.push('{');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("}}") {
            out.push('}');
            rest = after;
        } else if let Some(after) = tail.strip_prefix('}') {
            out.push('}');
            rest = after;
        } else {
            let inner = &tail[1..];
            let end = inner.find('}').ok_or_else(|| MissingLabel {
                key: inner.to_string(),
            })?;
            let key = &inner[..end];
            let value = labels.get(key).ok_or_else(|| MissingLabel {
                key: key.to_string(),
            })?;
            out.push_str(value);
            rest = &inner[end + 1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> OpaqueLabels {
        OpaqueLabels::try_from_pairs(pairs.iter().copied()).expect("fixture labels are valid")
    }

    fn reject(pairs: &[(&str, &str)]) -> LabelsError {
        OpaqueLabels::try_from_pairs(pairs.iter().copied()).expect_err("fixture must be rejected")
    }

    // ── constraints: six rejections, each with its legal boundary ─────────

    #[test]
    fn key_case_and_first_char_are_enforced() {
        for bad in ["Tenant", "9lives", "_x", ""] {
            assert_eq!(
                reject(&[(bad, "v")]),
                LabelsError {
                    key: bad.into(),
                    reason: LabelsReason::KeyCharset
                },
                "key {bad:?} must be rejected"
            );
        }
        assert_eq!(labels(&[("t", "v")]).get("t"), Some("v"));
    }

    #[test]
    fn key_charset_is_lowercase_digits_underscore() {
        for bad in ["cost-center", "a.b", "a b", "ünïcode", "a\n"] {
            assert_eq!(reject(&[(bad, "v")]).reason, LabelsReason::KeyCharset);
        }
        assert!(labels(&[("cost_center_7", "v")])
            .get("cost_center_7")
            .is_some());
    }

    #[test]
    fn key_length_is_capped_at_64() {
        let ok = "a".repeat(MAX_KEY_LEN);
        assert_eq!(labels(&[(ok.as_str(), "v")]).len(), 1);
        let long = "a".repeat(MAX_KEY_LEN + 1);
        let err = reject(&[(long.as_str(), "v")]);
        assert_eq!(err.key, long);
        assert_eq!(err.reason, LabelsReason::KeyTooLong { len: 65 });
        assert_eq!(err.reason.to_string(), "key is 65 bytes (max 64)");
    }

    #[test]
    fn key_count_is_capped_at_16() {
        let keys: Vec<String> = (0..=MAX_LABELS).map(|i| format!("k{i:02}")).collect();
        let sixteen: Vec<(&str, &str)> = keys[..MAX_LABELS]
            .iter()
            .map(|k| (k.as_str(), "v"))
            .collect();
        assert_eq!(labels(&sixteen).len(), MAX_LABELS);
        let seventeen: Vec<(&str, &str)> = keys.iter().map(|k| (k.as_str(), "v")).collect();
        let err = reject(&seventeen);
        assert_eq!(err.reason, LabelsReason::TooMany { count: 17 });
        assert_eq!(err.key, "k16", "the error names the first key past the cap");
        // A key repeated 17 times is one distinct key: not a cap violation.
        let repeated = vec![("same", "v"); MAX_LABELS + 1];
        assert_eq!(labels(&repeated).len(), 1);
    }

    #[test]
    fn value_length_is_capped_at_256_bytes() {
        let ok = "v".repeat(MAX_VALUE_BYTES);
        assert_eq!(labels(&[("k", ok.as_str())]).get("k"), Some(ok.as_str()));
        let long = "v".repeat(MAX_VALUE_BYTES + 1);
        let err = reject(&[("k", long.as_str())]);
        assert_eq!(err.reason, LabelsReason::ValueTooLong { len: 257 });
        assert!(
            !err.to_string().contains("vvvv"),
            "the error must not carry the value: {err}"
        );
        // Multi-byte: 86 × 3 bytes = 258 > 256 even though only 86 chars.
        let wide = "€".repeat(86);
        assert_eq!(
            reject(&[("k", wide.as_str())]).reason,
            LabelsReason::ValueTooLong { len: 258 }
        );
    }

    #[test]
    fn value_control_characters_are_rejected_crlf_included() {
        for bad in ["a\r\nb", "a\nb", "a\rb", "a\tb", "a\0b", "a\u{7f}b"] {
            let err = reject(&[("k", bad)]);
            assert_eq!(err.reason, LabelsReason::ValueControlChar, "{bad:?}");
            assert_eq!(err.key, "k");
        }
        // Printable non-ASCII and spaces are fine.
        assert_eq!(labels(&[("k", "Ünïcode ok")]).get("k"), Some("Ünïcode ok"));
        assert_eq!(labels(&[("k", "")]).get("k"), Some(""));
    }

    #[test]
    fn display_names_key_and_rule_only() {
        let err = reject(&[("bad key", "secret-value")]);
        assert_eq!(
            err.to_string(),
            "label `bad key` rejected: key must match ^[a-z][a-z0-9_]{0,63}$"
        );
        assert!(!err.to_string().contains("secret-value"));
    }

    // ── derivation ────────────────────────────────────────────────────────

    #[test]
    fn from_session_owner_yields_tenant_or_nothing() {
        let some = from_session_owner(Some("acme")).expect("a plain tenant is valid");
        assert_eq!(some.iter().collect::<Vec<_>>(), [("tenant", "acme")]);
        let none = from_session_owner(None).expect("no tenant is not an error");
        assert!(none.is_empty());
        assert_eq!(none, OpaqueLabels::default());
    }

    #[test]
    fn from_session_owner_rejects_an_out_of_bounds_tenant_naming_the_key() {
        let err = from_session_owner(Some("bad\r\nvalue")).expect_err("CRLF must be rejected");
        assert_eq!(err.key, "tenant");
        assert_eq!(err.reason, LabelsReason::ValueControlChar);
    }

    #[test]
    fn into_iter_hands_sorted_owned_pairs_to_run_meta() {
        let pairs: Vec<(String, String)> = labels(&[("zeta", "1"), ("alpha", "2")])
            .into_iter()
            .collect();
        assert_eq!(
            pairs,
            [
                ("alpha".to_string(), "2".to_string()),
                ("zeta".to_string(), "1".to_string())
            ]
        );
    }

    // ── template expansion (W2 primitive, no caller in W1) ────────────────

    #[test]
    fn expand_template_substitutes_every_placeholder() {
        let l = labels(&[("tenant", "acme"), ("region", "eu")]);
        assert_eq!(
            expand_template("https://{region}.skills.example/{tenant}/{tenant}.json", &l)
                .expect("all keys present"),
            "https://eu.skills.example/acme/acme.json"
        );
        assert_eq!(
            expand_template("no placeholders", &l).expect("literal"),
            "no placeholders"
        );
        assert_eq!(expand_template("", &l).expect("empty"), "");
    }

    #[test]
    fn expand_template_fails_closed_on_a_missing_key() {
        let l = labels(&[("tenant", "acme")]);
        assert_eq!(
            expand_template("x-{tenant}-{team}", &l),
            Err(MissingLabel { key: "team".into() })
        );
        assert_eq!(
            expand_template("{}", &l),
            Err(MissingLabel { key: String::new() })
        );
        assert_eq!(
            expand_template("prefix {tenant", &l),
            Err(MissingLabel {
                key: "tenant".into()
            }),
            "an unterminated placeholder is never silently kept"
        );
        assert_eq!(
            expand_template("{team}", &OpaqueLabels::default()),
            Err(MissingLabel { key: "team".into() })
        );
        assert_eq!(
            MissingLabel { key: "team".into() }.to_string(),
            "template references missing label `team`"
        );
    }

    #[test]
    fn expand_template_escapes_doubled_braces() {
        let l = labels(&[("tenant", "acme")]);
        assert_eq!(
            expand_template("{{literal}} {tenant} {{{tenant}}}", &l).expect("escapes"),
            "{literal} acme {acme}"
        );
        assert_eq!(
            expand_template("a } b", &l).expect("lone close brace is literal"),
            "a } b"
        );
    }
}
