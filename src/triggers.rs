//! Inbound triggers: cron schedules and webhooks that create agent runs.
//!
//! Issue #105: the server had no way to *start* work on its own — a run
//! existed only while a client held an HTTP/SSE connection open. This
//! module adds the two inbound entry points an assistant deployment
//! needs ("every day at 9:00, summarize and notify me"):
//!
//! - **Cron** ([`TriggerSpec::Cron`]) — a wall-clock schedule evaluated
//!   against the persisted `next_fire_at` timestamp.
//! - **Webhook** ([`TriggerSpec::Webhook`]) — a token endpoint; an HTTP
//!   POST to `/webhooks/{id}?key=...` fires it. The `?key=` secret is an
//!   *additional* credential, not a replacement for the server's own
//!   auth: the route lives on the protected router, so the request must
//!   also carry the usual API key (unless the server runs with auth
//!   disabled) — and since issue #85 that credential must be the one that
//!   registered the trigger (or an admin's).
//!
//! # Persistence and delivery contract
//!
//! Trigger definitions live in the workspace's user data dir
//! (`~/.recursive/<workspace>/triggers.json`), written atomically via
//! [`crate::atomic::atomic_write`] — the same durability discipline as
//! session metadata. Triggers survive a process restart; firing is the
//! host's job (the HTTP layer spawns the run and records the delivery in
//! [`Trigger::last_result`]).
//!
//! The cron engine is deliberately minute-granularity and std-only
//! (`cron_expr` parser in this file, no `cron` crate — issue #105 asked
//! for the *minimum* inbound path, and invariant #6 requires justifying
//! every new dependency). Supported fields: `min hour dom month dow`,
//! each a comma-separated list of `*`, `*/n`, `n`, or `n-m` ranges.
//! Day-of-month and day-of-week combine with OR when BOTH are restricted
//! (standard vixie-cron semantics); otherwise they intersect.
//!
//! Firing is **at-least-once with backlog collapse**: a due cron fires
//! once, then [`TriggerStore::advance_cron`] sets `next_fire_at` to the
//! first scheduled window strictly after *now*. A server that was down for
//! 3 days therefore runs the job once on restart (not 3 times) — the missed
//! windows are collapsed, not replayed. While the process is up and only a
//! little late, this is still exactly one window forward, so no window is
//! skipped for the ordinary case (missed windows can only come from real
//! downtime or a tick slower than the schedule).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// What kind of inbound trigger this is and when/how it fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSpec {
    /// Wall-clock schedule in vixie-cron subset syntax (minute-granular).
    Cron {
        /// `min hour dom month dow` — `*`, `*/n`, `n`, `n-m`, comma lists.
        expr: String,
    },
    /// Fired by an external HTTP POST to `/webhooks/{id}`.
    Webhook {
        /// Shared secret compared against the `?key=` query parameter.
        /// Empty disables the check (local-only deployments).
        #[serde(default)]
        secret: String,
    },
}

impl TriggerSpec {
    /// Discriminant string used in listings and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            TriggerSpec::Cron { .. } => "cron",
            TriggerSpec::Webhook { .. } => "webhook",
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

/// One registered trigger: the schedule plus the agent work to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    /// Stable identifier (also the webhook URL path segment).
    pub id: String,
    pub spec: TriggerSpec,
    /// Goal text handed to the agent when the trigger fires.
    pub goal: String,
    /// Session to resume with the goal. `None` = one-shot run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Where the run result should be delivered (see `crate::notify`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<crate::notify::NotifyTarget>,
    /// Cron only: RFC3339 timestamp of the next scheduled fire.
    /// Managed by [`TriggerStore::advance_cron`]; `None` means "not yet
    /// computed" — the first scheduler tick computes it from now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<String>,
    /// RFC3339 timestamp of the last fire (any delivery outcome).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    /// Outcome of the last delivery attempt — observability for "did my
    /// 9am summary actually go out?" without grepping logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    /// Issue #85: subject that registered the trigger. A trigger is work the
    /// server runs on its own (in a session, as an admin identity), so the
    /// caller that registered it is the only caller that may read, re-goal or
    /// remove it. `None` (a blob written by an older build) is unattributed:
    /// admins only — the same default-deny an unattributed session gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Issue #85: the owner's tenant, part of the ownership key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

impl Trigger {
    /// Create a new (disabled-by-default) trigger. Callers flip `enabled`
    /// after persisting — a trigger that has never been validated should
    /// not start firing on its own.
    pub fn new(
        id: impl Into<String>,
        spec: TriggerSpec,
        goal: impl Into<String>,
        session_id: Option<String>,
        notify: Option<crate::notify::NotifyTarget>,
    ) -> Self {
        Self {
            id: id.into(),
            spec,
            goal: goal.into(),
            session_id,
            notify,
            next_fire_at: None,
            last_fired_at: None,
            last_result: None,
            enabled: false,
            owner: None,
            tenant: None,
        }
    }

    /// Whether this trigger is due for firing at `now_epoch_secs`.
    ///
    /// Disabled triggers and webhooks are never due (webhooks fire through
    /// their HTTP endpoint, not the scheduler). A cron trigger with no
    /// `next_fire_at` is due — the caller must then immediately
    /// `advance_cron` so it does not re-fire every tick.
    pub fn is_due(&self, now_epoch_secs: i64) -> bool {
        if !self.enabled {
            return false;
        }
        match (&self.spec, &self.next_fire_at) {
            (TriggerSpec::Cron { .. }, Some(ts)) => match parse_rfc3339_utc(ts) {
                Some(next) => now_epoch_secs >= next,
                None => false,
            },
            (TriggerSpec::Cron { .. }, None) => true,
            (TriggerSpec::Webhook { .. }, _) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// File-backed registry of triggers for one workspace.
///
/// Reads and writes are plain load-modify-save over the on-disk JSON file
/// (persisted atomically via [`crate::atomic::atomic_write`]). There is no
/// in-process mutex and no cross-process lock — the last writer wins, so
/// two concurrent fires for the same trigger can lose one another's
/// `last_result` (same contract as `.meta.json`).
pub struct TriggerStore {
    path: PathBuf,
}

impl TriggerStore {
    /// Default location: `<user_workspace_dir>/triggers.json`.
    pub fn default_path(workspace: &Path) -> PathBuf {
        crate::paths::user_workspace_dir(workspace)
            .map(|d| d.join("triggers.json"))
            .unwrap_or_else(|_| PathBuf::from(".recursive").join("triggers.json"))
    }

    /// Open (not create) the store backed by an explicit path — tests use
    /// this; production uses [`TriggerStore::for_workspace`].
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Open the store at the workspace's default location.
    pub fn for_workspace(workspace: &Path) -> Self {
        Self::new(Self::default_path(workspace))
    }

    /// Load all triggers. A missing file is an empty registry (first run),
    /// not an error.
    pub fn load(&self) -> Result<Vec<Trigger>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(e)),
        };
        serde_json::from_slice(&bytes).map_err(|e| Error::Config {
            message: format!("corrupt trigger store {}: {e}", self.path.display()),
        })
    }

    /// Atomically persist the full registry.
    pub fn save(&self, triggers: &[Trigger]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let json = serde_json::to_string_pretty(triggers).map_err(|e| Error::Config {
            message: format!("serialize triggers: {e}"),
        })?;
        crate::atomic::atomic_write(&self.path, json.as_bytes()).map_err(Error::Io)
    }

    /// Insert or replace by id.
    pub fn upsert(&self, trigger: Trigger) -> Result<()> {
        let mut all = self.load()?;
        match all.iter_mut().find(|t| t.id == trigger.id) {
            Some(slot) => *slot = trigger,
            None => all.push(trigger),
        }
        self.save(&all)
    }

    /// Remove a trigger; `false` when the id was unknown.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let mut all = self.load()?;
        let before = all.len();
        all.retain(|t| t.id != id);
        let removed = all.len() != before;
        if removed {
            self.save(&all)?;
        }
        Ok(removed)
    }

    /// Get one trigger by id.
    pub fn get(&self, id: &str) -> Result<Option<Trigger>> {
        Ok(self.load()?.into_iter().find(|t| t.id == id))
    }

    /// After a cron fire: stamp `last_fired_at` and `last_result`, and
    /// point `next_fire_at` at the first scheduled window strictly after
    /// *now*.
    ///
    /// Advancing from *now* (rather than replaying every missed window
    /// forward from the stored `next_fire_at`) collapses a downtime backlog
    /// into a single catch-up fire — a 3-day outage of a daily trigger
    /// yields one run, not three. When the process is up and merely a
    /// little late, this is still exactly one window forward.
    ///
    /// Returns the updated trigger (`None` when the id is unknown or the
    /// trigger is not a cron — advancing a webhook is a caller bug).
    pub fn advance_cron(&self, id: &str, result: impl Into<String>) -> Result<Option<Trigger>> {
        let mut all = self.load()?;
        let Some(trigger) = all.iter_mut().find(|t| t.id == id) else {
            return Ok(None);
        };
        let TriggerSpec::Cron { expr } = trigger.spec.clone() else {
            return Ok(None);
        };
        let now = epoch_now();
        let next = next_after(&expr, now).ok_or_else(|| Error::Config {
            message: format!("trigger {id}: cannot advance cron expr '{expr}'"),
        })?;
        trigger.next_fire_at = Some(format_rfc3339_utc(next));
        trigger.last_fired_at = Some(format_rfc3339_utc(now));
        trigger.last_result = Some(result.into());
        let updated = trigger.clone();
        self.save(&all)?;
        Ok(Some(updated))
    }
}

// ---------------------------------------------------------------------------
// Time helpers (std-only; `chrono` stays a session-module detail)
// ---------------------------------------------------------------------------

/// Broken-down UTC civil time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Utc {
    pub year: i64,
    /// 1-12.
    pub month: u32,
    /// 1-31.
    pub day: u32,
    /// 0-23.
    pub hour: u32,
    /// 0-59.
    pub minute: u32,
    /// 0-6, Sunday == 0 (cron convention).
    pub weekday: u32,
}

/// Convert epoch seconds to UTC civil time (Howard Hinnant's algorithm,
/// same civil-from-days core as `crate::session::epoch_day_to_ymd`).
pub fn utc_from_epoch(secs: i64) -> Utc {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = crate::session::epoch_day_to_ymd(days);
    // 1970-01-01 was a Thursday (weekday 4 with Sunday == 0).
    let weekday = (days + 4).rem_euclid(7) as u32;
    Utc {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        minute: ((rem % 3600) / 60) as u32,
        weekday,
    }
}

/// Days-from-civil (Hinnant): inverse of [`utc_from_epoch`]'s date part.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Current epoch seconds. `mut` local clock tail only.
pub fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` (the format this module writes).
/// Lenient input: seconds may carry a fractional part; `+00:00` offsets
/// are accepted as Z.
pub fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    if bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ' {
        return None;
    }
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    if bytes[13] != b':' {
        return None;
    }
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    if bytes[16] != b':' {
        return None;
    }
    let second: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let secs_of_day = hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    Some(days_from_civil(year, month, day) * 86_400 + secs_of_day)
}

/// Format epoch seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_rfc3339_utc(secs: i64) -> String {
    let t = utc_from_epoch(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year, t.month, t.day, t.hour, t.minute, 0
    )
}

// ---------------------------------------------------------------------------
// Cron expression
// ---------------------------------------------------------------------------

/// One cron field: the set of allowed values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CronField {
    min: u32,
    max: u32,
    values: Vec<u32>,
}

impl CronField {
    fn contains(&self, v: u32) -> bool {
        self.values.contains(&v)
    }
}

/// A parsed `min hour dom month dow` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
    /// True when BOTH dom and dow are restricted (non-`*`) — cron then
    /// ORs them instead of intersecting.
    dom_dow_or: bool,
}

/// Parse a cron expression. Errors carry the offending field so a bad
/// `RECURSIVE_TRIGGER` surfaces at registration, not at 9am.
pub fn parse_cron(expr: &str) -> std::result::Result<CronExpr, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron expr must have 5 fields (min hour dom month dow), got {}: '{expr}'",
            fields.len()
        ));
    }
    let minute = parse_field(fields[0], 0, 59)?;
    let hour = parse_field(fields[1], 0, 23)?;
    let dom = parse_field(fields[2], 1, 31)?;
    let month = parse_field(fields[3], 1, 12)?;
    // Cron allows 7 for Sunday; normalize to 0.
    let mut dow = parse_field(fields[4], 0, 7)?;
    for v in &mut dow.values {
        if *v == 7 {
            *v = 0;
        }
    }
    dow.values.sort_unstable();
    dow.values.dedup();
    // Star detection happens AFTER normalization: a `*` dow field parses
    // as [0..=7], collapses to [0..=6], and must be recognised as the
    // full range again (otherwise dom/dow OR-semantics fire on
    // `30 2 *`-style expressions and unrelated weekdays match).
    dow.max = 6;
    let dom_star = dom.values.len() == (dom.max - dom.min + 1) as usize;
    let dow_star = dow.values.len() == (dow.max - dow.min + 1) as usize;
    Ok(CronExpr {
        minute,
        hour,
        dom,
        month,
        dow,
        dom_dow_or: !dom_star && !dow_star,
    })
}

fn parse_field(field: &str, min: u32, max: u32) -> std::result::Result<CronField, String> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty cron field component in '{field}'"));
        }
        // [step/]range where range is `*` or `a` or `a-b`.
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("bad cron step '{s}' in '{field}'"))?;
                if step == 0 {
                    return Err(format!("cron step must be >= 1 in '{field}'"));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let lo: u32 = a
                .trim()
                .parse()
                .map_err(|_| format!("bad cron bound '{a}' in '{field}'"))?;
            let hi: u32 = b
                .trim()
                .parse()
                .map_err(|_| format!("bad cron bound '{b}' in '{field}'"))?;
            (lo, hi)
        } else {
            let v: u32 = range
                .parse()
                .map_err(|_| format!("bad cron value '{range}' in '{field}'"))?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!(
                "cron range {lo}-{hi} outside {min}-{max} in '{field}'"
            ));
        }
        let mut v = lo;
        while v <= hi {
            values.push(v);
            v += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(CronField { min, max, values })
}

/// Scan horizon for [`next_after`] — four 366-day years, so a leap-day
/// schedule (`0 0 29 2 *`) is found from any starting point. Expressions
/// that match less often than this are rejected at registration.
const HORIZON_DAYS: i64 = 4 * 366;

/// The next time strictly after `from_secs` matching the expression
/// (minute granularity). Returns `None` when no match exists within the
/// 4-year scan horizon (expressions that fire less often than that, e.g.
/// `0 0 29 2 *` across a non-leap century, are refused at registration).
pub fn next_after(expr: &str, from_secs: i64) -> Option<i64> {
    let expr = parse_cron(expr).ok()?;
    // Start one minute past the from-time, truncated to the minute.
    let mut candidate = (from_secs.div_euclid(60) + 1) * 60;
    let horizon = candidate + HORIZON_DAYS * 86_400;
    while candidate < horizon {
        let t = utc_from_epoch(candidate);
        if !expr.month.contains(t.month) {
            // Fast-forward to the first day of the next month.
            candidate = jump_to_next_month(candidate);
            continue;
        }
        let dom_ok = expr.dom.contains(t.day);
        let dow_ok = expr.dow.contains(t.weekday);
        let day_ok = if expr.dom_dow_or {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        };
        if !day_ok {
            // Snap to the next midnight, NOT `candidate += 86_400`: the
            // hour/minute scan below can only move forward, so keeping this
            // day's time-of-day would skip the earlier windows of a later
            // matching day (e.g. "0 9 * * 5" scanned from Wed 15:00 would
            // walk past Friday 09:00 and lose a whole week). `day_ok` does
            // not depend on the time of day, so the next midnight is the
            // earliest instant a later day can match.
            candidate = (candidate.div_euclid(86_400) + 1) * 86_400;
            continue;
        }
        if !expr.hour.contains(t.hour) {
            candidate += 3600 - (t.minute as i64 * 60);
            continue;
        }
        if !expr.minute.contains(t.minute) {
            candidate += 60;
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Advance to 00:00:00 of the next calendar month (UTC).
fn jump_to_next_month(epoch_secs: i64) -> i64 {
    let t = utc_from_epoch(epoch_secs);
    let (ny, nm) = if t.month == 12 {
        (t.year + 1, 1)
    } else {
        (t.year, t.month + 1)
    };
    days_from_civil(ny, nm, 1) * 86_400
}

// ---------------------------------------------------------------------------
// Webhook auth
// ---------------------------------------------------------------------------

/// Check a webhook request's key against the configured secret.
/// An empty secret disables auth (explicit local-deployment choice).
pub fn webhook_key_matches(secret: &str, provided: Option<&str>) -> bool {
    if secret.is_empty() {
        return true;
    }
    // Constant-time-ish comparison: compare hashes so timing does not
    // leak the secret byte-by-byte over the network.
    let a = blake3::hash(secret.as_bytes());
    match provided {
        Some(k) => blake3::hash(k.as_bytes()) == a,
        None => false,
    }
}

/// Generate a fresh webhook secret (32 hex chars).
pub fn generate_secret() -> String {
    let id = uuid::Uuid::new_v4();
    blake3::hash(id.as_bytes().as_slice()).to_hex()[..32].to_string()
}

/// Generate a trigger id: `trig-` + 8 hex chars.
pub fn generate_trigger_id() -> String {
    let id = uuid::Uuid::new_v4();
    format!(
        "trig-{}",
        &blake3::hash(id.as_bytes().as_slice()).to_hex()[..8]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── cron field parsing ─────────────────────────────────────────────

    #[test]
    fn parse_cron_accepts_all_stars() {
        let e = parse_cron("* * * * *").expect("parse");
        assert!(e.minute.contains(0) && e.minute.contains(59));
        assert!(e.hour.contains(23));
        assert!(e.dom.contains(31));
        assert!(e.month.contains(12));
        assert!(e.dow.contains(6));
        assert!(!e.dom_dow_or);
    }

    #[test]
    fn parse_cron_values_ranges_and_steps() {
        let e = parse_cron("0,30 9-17 */2 1,6 1-5").expect("parse");
        assert_eq!(e.minute.values, vec![0, 30]);
        assert_eq!(e.hour.values, (9..=17).collect::<Vec<_>>());
        assert_eq!(
            e.dom.values,
            vec![1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31]
        );
        assert_eq!(e.month.values, vec![1, 6]);
        // Monday..Friday, no Sunday normalization involved.
        assert_eq!(e.dow.values, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_cron_rejects_bad_field_count_and_bounds() {
        assert!(parse_cron("* * * *").is_err(), "4 fields must fail");
        assert!(parse_cron("60 * * * *").is_err(), "minute 60 out of range");
        assert!(parse_cron("* 24 * * *").is_err(), "hour 24 out of range");
        assert!(parse_cron("* * 0 * *").is_err(), "dom 0 out of range");
        assert!(parse_cron("* * * 13 *").is_err(), "month 13 out of range");
        assert!(parse_cron("* * * * 8").is_err(), "dow 8 out of range");
        assert!(parse_cron("*/0 * * * *").is_err(), "zero step must fail");
        assert!(
            parse_cron("5-1 * * * *").is_err(),
            "inverted range must fail"
        );
    }

    #[test]
    fn parse_cron_normalizes_dow_7_to_sunday() {
        let e = parse_cron("0 9 * * 7").expect("parse");
        assert_eq!(e.dow.values, vec![0], "7 must normalize to Sunday=0");
    }

    // ── epoch ↔ civil round trip ───────────────────────────────────────

    #[test]
    fn utc_round_trips_known_instants() {
        // 2026-01-01T00:00:00Z
        let epoch = days_from_civil(2026, 1, 1) * 86_400;
        let t = utc_from_epoch(epoch);
        assert_eq!((t.year, t.month, t.day), (2026, 1, 1));
        assert_eq!((t.hour, t.minute), (0, 0));
        assert_eq!(t.weekday, 4, "2026-01-01 is a Thursday");

        // 1970-01-01T00:00:00Z — Thursday, weekday 4.
        let t0 = utc_from_epoch(0);
        assert_eq!((t0.year, t0.month, t0.day), (1970, 1, 1));
        assert_eq!(t0.weekday, 4);

        // Leap-day: 2024-02-29T12:34:00Z.
        let leap = days_from_civil(2024, 2, 29) * 86_400 + 12 * 3600 + 34 * 60;
        let tl = utc_from_epoch(leap);
        assert_eq!((tl.year, tl.month, tl.day), (2024, 2, 29));
        assert_eq!((tl.hour, tl.minute), (12, 34));
    }

    #[test]
    fn rfc3339_parse_format_round_trip() {
        let s = "2026-03-05T09:30:00Z";
        let epoch = parse_rfc3339_utc(s).expect("parse");
        assert_eq!(format_rfc3339_utc(epoch), s);
        assert_eq!(
            parse_rfc3339_utc("2026-03-05T09:30:00.123Z"),
            Some(epoch),
            "fractional seconds tolerated"
        );
        assert_eq!(parse_rfc3339_utc("not-a-time"), None);
        assert_eq!(parse_rfc3339_utc("2026-13-05T09:30:00Z"), None, "month 13");
        assert_eq!(parse_rfc3339_utc("2026-03-05T24:30:00Z"), None, "hour 24");
    }

    // ── next_after ─────────────────────────────────────────────────────

    /// 2026-01-05 (Monday) 09:00:00Z → epoch.
    fn monday_0900() -> i64 {
        parse_rfc3339_utc("2026-01-05T09:00:00Z").expect("fixture")
    }

    #[test]
    fn next_after_daily_expr() {
        // "30 9 * * *" from Mon 09:00 → same day 09:30.
        let next = next_after("30 9 * * *", monday_0900()).expect("next");
        assert_eq!(format_rfc3339_utc(next), "2026-01-05T09:30:00Z");
    }

    #[test]
    fn next_after_rolls_to_next_day() {
        // "0 9 * * *" from Mon 09:00 → Tue 09:00.
        let next = next_after("0 9 * * *", monday_0900()).expect("next");
        assert_eq!(format_rfc3339_utc(next), "2026-01-06T09:00:00Z");
    }

    #[test]
    fn next_after_dow_restriction_skips_non_matching_days() {
        // "0 9 * * 1" (Mondays) from Mon 09:00 → next Monday.
        let next = next_after("0 9 * * 1", monday_0900()).expect("next");
        assert_eq!(format_rfc3339_utc(next), "2026-01-12T09:00:00Z");
    }

    #[test]
    fn next_after_keeps_a_matching_days_earlier_window() {
        // Regression (issue #105 review): the day fast-forward must snap to
        // the next midnight. Advancing by a flat 86_400 kept the scan's
        // time-of-day, so a *matching* day reached at 15:00 skipped its own
        // 09:00 window and the scan jumped a whole cycle.
        let wed_1500 = parse_rfc3339_utc("2026-01-07T15:00:00Z").expect("Wed 15:00");
        assert_eq!(
            format_rfc3339_utc(next_after("0 9 * * 5", wed_1500).expect("next")),
            "2026-01-09T09:00:00Z",
            "Friday 09:00 two days out, not the Friday after"
        );
        // Same shape once the day walk starts from a non-midnight scan:
        // 02:00 on the next Monday, not the Monday after.
        assert_eq!(
            format_rfc3339_utc(next_after("0 2 * * 1", wed_1500).expect("next")),
            "2026-01-12T02:00:00Z"
        );
        // dom-restricted: the Jan 1 window is in the past, so Feb 1 — a
        // month late would mean the scan lost the January cycle.
        let thu_1200 = parse_rfc3339_utc("2026-01-15T12:00:00Z").expect("Thu 12:00");
        assert_eq!(
            format_rfc3339_utc(next_after("0 0 1 * *", thu_1200).expect("next")),
            "2026-02-01T00:00:00Z"
        );
    }

    #[test]
    fn next_after_is_within_one_cycle_from_a_non_midnight_start() {
        // A weekly schedule can never be more than 7 days out, whatever
        // the time of day the scan starts at. Pinned as a property across
        // every hour of the week so the day fast-forward cannot regress
        // into skipping a cycle again.
        let base = parse_rfc3339_utc("2026-01-05T00:00:00Z").expect("Mon 00:00");
        for hour in 0..(7 * 24) {
            let from = base + i64::from(hour) * 3600;
            let next = next_after("0 9 * * 5", from).expect("weekly expr always matches");
            assert!(
                next - from <= 7 * 86_400,
                "from {}: next {} is more than one week out",
                format_rfc3339_utc(from),
                format_rfc3339_utc(next)
            );
            assert!(next > from, "must be strictly after the from-time");
        }
    }

    #[test]
    fn next_after_dom_dow_or_semantics() {
        // dom=13 restricted AND dow=5 (Friday) restricted → OR semantics.
        // 2026-01-13 is a Tuesday; 2026-01-09 and 2026-01-16 are Fridays.
        let from = parse_rfc3339_utc("2026-01-08T00:00:00Z").expect("from");
        let next = next_after("0 12 13 * 5", from).expect("next");
        // Jan 9 (Fri) comes before Jan 13 (Tue).
        assert_eq!(format_rfc3339_utc(next), "2026-01-09T12:00:00Z");
    }

    #[test]
    fn next_after_month_jump_is_fast() {
        // "0 0 1 1 *" (Jan 1st) evaluated from July must land on next
        // Jan 1 — the month fast-forward keeps this from scanning ~4400
        // minutes one at a time.
        let from = parse_rfc3339_utc("2026-07-15T00:00:00Z").expect("from");
        let next = next_after("0 0 1 1 *", from).expect("next");
        assert_eq!(format_rfc3339_utc(next), "2027-01-01T00:00:00Z");
    }

    #[test]
    fn next_after_strictly_after_from() {
        // The fire time itself must NOT match again (at-least-once, not
        // twice).
        let next = next_after("0 9 * * *", monday_0900() - 1).expect("next");
        assert_eq!(format_rfc3339_utc(next), "2026-01-05T09:00:00Z");
    }

    #[test]
    fn next_after_impossible_date_returns_none() {
        // Feb 30th never exists — must terminate with None, not spin.
        // (Feb is month 2, so the month fast-forward makes this
        // unreachable — the horizon scan proves termination either way.)
        assert_eq!(next_after("0 0 30 2 *", monday_0900()), None);
    }

    #[test]
    fn next_after_unreachable_dom_returns_none() {
        // `0 0 31 2 *` (Feb 31) can never match a real date; with the
        // day fast-forward the scan must still terminate within the
        // 4-year horizon.
        assert_eq!(next_after("0 0 31 2 *", monday_0900()), None);
    }

    #[test]
    fn next_after_finds_a_leap_day_within_the_horizon() {
        // `0 0 29 2 *` matches only Feb 29, so the scan must span more
        // than one year — the horizon is 4 years, matching the
        // "never fires within 4 years" rejection message.
        let from = parse_rfc3339_utc("2026-03-01T00:00:00Z").expect("from");
        let next = next_after("0 0 29 2 *", from).expect("leap day must be found");
        assert_eq!(format_rfc3339_utc(next), "2028-02-29T00:00:00Z");
    }

    #[test]
    fn next_after_invalid_expr_is_none_not_panic() {
        assert_eq!(next_after("not a cron", monday_0900()), None);
    }

    // ── Trigger is_due ─────────────────────────────────────────────────

    #[test]
    fn due_cron_with_passed_next_fire() {
        let mut t = Trigger::new(
            "a",
            TriggerSpec::Cron {
                expr: "* * * * *".into(),
            },
            "goal",
            None,
            None,
        );
        assert!(!t.is_due(monday_0900()), "disabled triggers never fire");
        t.enabled = true;
        assert!(t.is_due(monday_0900()), "unset next_fire_at = due now");
        t.next_fire_at = Some("2026-01-05T09:00:00Z".into());
        assert!(t.is_due(monday_0900()), "now == next_fire is due");
        assert!(!t.is_due(monday_0900() - 1), "before next_fire not due");
        t.next_fire_at = Some("garbage".into());
        assert!(!t.is_due(monday_0900()), "unparseable next_fire never due");
    }

    #[test]
    fn webhooks_are_never_scheduler_due() {
        let mut t = Trigger::new(
            "w",
            TriggerSpec::Webhook {
                secret: String::new(),
            },
            "goal",
            None,
            None,
        );
        t.enabled = true;
        t.next_fire_at = Some("2000-01-01T00:00:00Z".into());
        assert!(!t.is_due(i64::MAX / 2));
    }

    // ── store round trip ───────────────────────────────────────────────

    fn temp_store() -> (tempfile::TempDir, TriggerStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TriggerStore::new(dir.path().join("triggers.json"));
        (dir, store)
    }

    #[test]
    fn store_missing_file_loads_empty() {
        let (_d, store) = temp_store();
        assert!(store.load().expect("load").is_empty());
    }

    #[test]
    fn store_upsert_get_delete_round_trip() {
        let (_d, store) = temp_store();
        let mut t = Trigger::new(
            "trig-1",
            TriggerSpec::Webhook {
                secret: "s3cret".into(),
            },
            "daily summary",
            Some("sess-1".into()),
            None,
        );
        t.enabled = true;
        store.upsert(t.clone()).expect("upsert");

        let got = store.get("trig-1").expect("get").expect("present");
        assert_eq!(got, t);
        assert!(matches!(got.spec, TriggerSpec::Webhook { ref secret } if secret == "s3cret"));

        // Upsert replaces, not duplicates.
        let mut t2 = t.clone();
        t2.goal = "updated".into();
        store.upsert(t2).expect("upsert2");
        assert_eq!(store.load().expect("load").len(), 1);
        assert_eq!(store.get("trig-1").expect("get").unwrap().goal, "updated");

        assert!(store.delete("trig-1").expect("delete"));
        assert!(
            !store.delete("trig-1").expect("delete again"),
            "second delete is a miss"
        );
        assert!(store.get("trig-1").expect("get").is_none());
    }

    #[test]
    fn advance_cron_collapses_a_downtime_backlog_to_one_window() {
        let (_d, store) = temp_store();
        let mut t = Trigger::new(
            "trig-cron",
            TriggerSpec::Cron {
                expr: "0 9 * * *".into(),
            },
            "morning",
            None,
            None,
        );
        t.enabled = true;
        // Scheduled for 3 days ago and the process was down since: the
        // catch-up must fire ONCE and land on the next window at/after
        // now, not replay the three missed days.
        let now = epoch_now();
        t.next_fire_at = Some(format_rfc3339_utc(now - 3 * 86_400));
        store.upsert(t).expect("upsert");

        let advanced = store
            .advance_cron("trig-cron", "delivered")
            .expect("advance")
            .expect("trigger updated");
        let next = parse_rfc3339_utc(advanced.next_fire_at.as_deref().expect("next set"))
            .expect("parseable");
        assert!(
            next > now,
            "the collapsed window must be in the future, got {}",
            advanced.next_fire_at.as_deref().unwrap_or("")
        );
        assert!(
            next - now <= 86_400,
            "a daily schedule collapses to at most one day ahead, got {}s",
            next - now
        );
        assert_eq!(advanced.last_result.as_deref(), Some("delivered"));
        assert!(advanced.last_fired_at.is_some());

        // The trigger is no longer due: one fire consumed the whole
        // backlog (the bug was one fire per missed window per tick).
        let stored = store.get("trig-cron").expect("get").expect("present");
        assert!(!stored.is_due(epoch_now()), "backlog must be consumed");

        // Advancing an unknown id → None.
        assert!(store.advance_cron("nope", "x").expect("advance").is_none());
    }

    #[test]
    fn advance_cron_from_a_future_window_does_not_fire_early() {
        let (_d, store) = temp_store();
        let mut t = Trigger::new(
            "trig-future",
            TriggerSpec::Cron {
                expr: "0 9 * * *".into(),
            },
            "g",
            None,
            None,
        );
        t.enabled = true;
        store.upsert(t).expect("upsert");
        let advanced = store
            .advance_cron("trig-future", "delivered")
            .expect("advance")
            .expect("updated");
        let next =
            parse_rfc3339_utc(advanced.next_fire_at.as_deref().expect("next")).expect("parseable");
        assert!(next > epoch_now(), "next window is always strictly ahead");
    }

    /// Regression (issue #105 review): after a restart on a day *after* the
    /// window, `advance_cron` recomputes from `now` — the old day
    /// fast-forward lost the next window whenever the scan reached a
    /// matching weekday later than its scheduled hour, silently dropping a
    /// whole week. A weekly schedule is never more than one week out.
    ///
    /// (`advance_cron` reads `epoch_now()` itself, so the bound is the
    /// assertion; the deterministic per-window cases live in the
    /// `next_after_*` tests above.)
    #[test]
    fn advance_cron_keeps_weekly_schedules_within_one_cycle() {
        let (_d, store) = temp_store();
        let mut t = Trigger::new(
            "trig-weekly",
            TriggerSpec::Cron {
                expr: "0 9 * * 5".into(),
            },
            "weekly summary",
            None,
            None,
        );
        t.enabled = true;
        // Last window a week ago and the process was down since: the
        // overdue window is catch-up fired, the next one is computed
        // forward from the restart.
        t.next_fire_at = Some(format_rfc3339_utc(epoch_now() - 7 * 86_400));
        store.upsert(t).expect("upsert");

        let advanced = store
            .advance_cron("trig-weekly", "delivered")
            .expect("advance")
            .expect("trigger updated");
        let next = parse_rfc3339_utc(advanced.next_fire_at.as_deref().expect("next set"))
            .expect("parseable");
        let after = epoch_now();
        assert!(
            next <= after + 7 * 86_400,
            "a weekly schedule must stay within one week of the restart, got {}",
            advanced.next_fire_at.as_deref().unwrap_or("")
        );
    }

    #[test]
    fn advance_cron_refuses_webhook_triggers() {
        let (_d, store) = temp_store();
        store
            .upsert(Trigger::new(
                "trig-w",
                TriggerSpec::Webhook {
                    secret: String::new(),
                },
                "g",
                None,
                None,
            ))
            .expect("upsert");
        assert!(store
            .advance_cron("trig-w", "x")
            .expect("advance")
            .is_none());
    }

    #[test]
    fn store_corrupt_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("triggers.json");
        std::fs::write(&path, b"{not json").expect("write");
        let store = TriggerStore::new(path);
        assert!(
            store.load().is_err(),
            "corrupt store must surface, not reset"
        );
    }

    // ── webhook auth ───────────────────────────────────────────────────

    #[test]
    fn webhook_key_matches_contract() {
        assert!(webhook_key_matches("", None), "empty secret = open");
        assert!(webhook_key_matches("", Some("anything")));
        assert!(webhook_key_matches("s3cret", Some("s3cret")));
        assert!(!webhook_key_matches("s3cret", Some("wrong")));
        assert!(!webhook_key_matches("s3cret", None), "missing key rejected");
    }

    #[test]
    fn generated_ids_are_unique_and_shaped() {
        let id1 = generate_trigger_id();
        let id2 = generate_trigger_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("trig-"));
        assert_eq!(id1.len(), "trig-".len() + 8);
        assert_eq!(generate_secret().len(), 32);
    }

    #[test]
    fn trigger_kind_discriminants() {
        assert_eq!(
            TriggerSpec::Cron {
                expr: String::new()
            }
            .kind(),
            "cron"
        );
        assert_eq!(
            TriggerSpec::Webhook {
                secret: String::new()
            }
            .kind(),
            "webhook"
        );
    }

    #[test]
    fn trigger_new_defaults_to_disabled() {
        let t = Trigger::new(
            "x",
            TriggerSpec::Webhook {
                secret: String::new(),
            },
            "g",
            None,
            None,
        );
        assert!(!t.enabled);
        assert!(t.session_id.is_none());
        assert!(t.notify.is_none());
    }
}
