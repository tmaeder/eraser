//! View models for status visualization: chips, deadline bars, funnels, region breakdown,
//! sending progress and the autopilot activity chart. Templates: partials/status.html.

use crate::autopilot::RoundReport;
use crate::broker::Broker;
use crate::history::{
    AutopilotRound, CLIENT_OTHER, Chip, DEADLINE_DAYS, Funnel, Record, RequestState, Tracking,
};
use chrono::{DateTime, Local, TimeDelta};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) type States = HashMap<String, RequestState>;

pub(super) fn chip(states: &States, id: &str, now: DateTime<Local>) -> Chip {
    states
        .get(id)
        .map_or_else(Chip::not_contacted, |s| s.chip(now))
}

/// Label of a broker's send button; None hides it (no email, deleted, or open and recent).
pub(super) fn send_label(
    states: &States,
    b: &Broker,
    now: DateTime<Local>,
    reminder_days: i64,
) -> Option<&'static str> {
    if b.email.is_empty() {
        return None;
    }
    match states.get(&b.id) {
        None => Some("Send request"),
        Some(s) if s.can_resend(now, reminder_days) => Some("Send again"),
        Some(_) => None,
    }
}

/// Short open/receipt summary for one tracked request (history rows).
pub(super) fn opens(t: Tracking) -> Value {
    let when = |d: Option<DateTime<Local>>| d.map(|d| d.format("%b %-d").to_string());
    let text = if let Some(at) = when(t.read_receipt_at) {
        format!("Read receipt {at}")
    } else if t.open_count == 0 {
        "Not opened yet".into()
    } else if t.client == CLIENT_OTHER {
        let n = t.open_count;
        format!(
            "Opened {}, last {}",
            if n == 1 {
                "once".into()
            } else {
                format!("{n}×")
            },
            when(t.last_opened_at).unwrap_or_default()
        )
    } else {
        "Image proxy only (not proof of reading)".into()
    };
    json!({
        "kind": if t.read_receipt_at.is_some() { "receipt" } else if t.client == CLIENT_OTHER { "opened" } else { "none" },
        "text": text,
    })
}

/// A history row: failed / interrupted attempts keep their own chip, the rest show the
/// broker's current state.
pub(super) fn record_chip(states: &States, r: &Record, now: DateTime<Local>) -> Chip {
    Chip::for_record(r).unwrap_or_else(|| chip(states, &r.broker_id, now))
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub(super) struct Deadline {
    day: i64,
    /// Fill width, 0–100.
    pct: i64,
    reminder_day: i64,
    reminder_pct: i64,
    /// ok | late (past reminder day) | overdue (past the deadline)
    state: &'static str,
    text: String,
}

/// The 30-day bar for open requests (None once resolved or never sent).
pub(super) fn deadline(
    s: Option<&RequestState>,
    now: DateTime<Local>,
    reminder_days: i64,
) -> Option<Deadline> {
    let s = s.filter(|s| s.is_open())?;
    let day = s.day(now);
    let reminder_day = reminder_days.clamp(1, DEADLINE_DAYS);
    let state = if day > DEADLINE_DAYS {
        "overdue"
    } else if day >= reminder_day {
        "late"
    } else {
        "ok"
    };
    let text = if day > DEADLINE_DAYS {
        format!("{} days overdue", day - DEADLINE_DAYS)
    } else {
        format!("Day {day} of {DEADLINE_DAYS}")
    };
    Some(Deadline {
        day,
        pct: day.min(DEADLINE_DAYS) * 100 / DEADLINE_DAYS,
        reminder_day,
        reminder_pct: reminder_day * 100 / DEADLINE_DAYS,
        state,
        text,
    })
}

fn pct(n: i64, of: i64) -> i64 {
    if of > 0 { n * 100 / of } else { 0 }
}

/// Funnel rows: main stages and side exits, each linking to `href(stage)`.
pub(super) fn funnel(f: &Funnel, href: impl Fn(&str) -> String) -> Value {
    let stage = |key: &str, label: &str, n: i64| json!({ "key": key, "label": label, "count": n, "pct": pct(n, f.sent), "href": href(key) });
    let exit = |key: &str, chip: &str, label: &str, hint: &str, n: i64| json!({ "key": key, "chip": chip, "label": label, "hint": hint, "count": n, "pct": pct(n, f.sent), "href": href(key) });
    json!({
        "sent": f.sent,
        "proxy_opened": f.proxy_opened,
        "proxy_href": href("proxy_opened"),
        "receipts": f.receipts,
        "receipt_href": href("receipt"),
        "stages": [
            stage("sent", "Sent", f.sent),
            stage("opened", "Opened", f.opened),
            stage("replied", "Replied", f.replied),
            stage("deleted", "Deleted", f.deleted),
        ],
        "exits": [
            exit("rejected", "rejected", "Rejected", "refused to delete", f.rejected),
            exit("action", "action", "Needs you", "a form, link or ID check", f.needs_you),
            exit("no_response", "reminder", "No response", "no reply after the reminder", f.no_response),
        ],
    })
}

const REGIONS: &[(&str, &str)] = &[
    ("ch", "Switzerland"),
    ("eu", "EU"),
    ("us", "US"),
    ("global", "Global"),
    ("other", "Other"),
];

/// Contacted and deleted per region bucket (unknown regions fold into "other").
pub(super) fn regions(brokers: &[Broker], states: &States) -> Vec<Value> {
    let mut counts: HashMap<&str, (i64, i64, i64)> = HashMap::new();
    for b in brokers {
        let region = b.region.to_ascii_lowercase();
        let key = REGIONS
            .iter()
            .map(|(k, _)| *k)
            .find(|k| *k == region)
            .unwrap_or("other");
        let c = counts.entry(key).or_default();
        c.0 += 1;
        if let Some(s) = states.get(&b.id).filter(|s| s.is_sent()) {
            c.1 += 1;
            c.2 += i64::from(s.matches_stage("deleted"));
        }
    }
    REGIONS
        .iter()
        .filter_map(|(key, label)| {
            let (total, contacted, deleted) = *counts.get(key)?;
            Some(json!({
                "key": key,
                "label": label,
                "href": if *key == "other" { String::new() } else { format!("/brokers?region={key}") },
                "total": total,
                "contacted": contacted,
                "deleted": deleted,
                "contacted_pct": pct(contacted, total),
                "deleted_pct": pct(deleted, total),
                "rate": pct(deleted, contacted),
            }))
        })
        .collect()
}

/// Today's sends against the daily limit, and when the remaining brokers are all contacted.
/// ponytail: assumes the whole daily budget goes to new brokers (reminders and company
/// requests share it); good enough for a "by about" date.
pub(super) fn sending(sent_today: i64, limit: i64, remaining: i64, now: DateTime<Local>) -> Value {
    let left_today = (limit - sent_today).max(0);
    let eta = if remaining == 0 {
        String::new()
    } else {
        let days = if remaining <= left_today {
            0
        } else {
            (remaining - left_today + limit - 1) / limit.max(1)
        };
        (now + TimeDelta::days(days))
            .format("%a, %b %-d")
            .to_string()
    };
    json!({
        "sent_today": sent_today,
        "limit": limit,
        "pct": pct(sent_today.min(limit), limit),
        "remaining": remaining,
        "eta": eta,
        "eta_today": remaining > 0 && remaining <= left_today,
    })
}

const ACTIVITY_ROUNDS: usize = 14;
const BAR_W: i64 = 14;
const BAR_GAP: i64 = 6;
const BAR_H: i64 = 36;

/// Small multiples (one row per measure) over the last rounds, oldest left.
pub(super) fn activity(rounds: &[AutopilotRound]) -> Value {
    let finished: Vec<(&AutopilotRound, RoundReport)> = rounds
        .iter()
        .filter(|r| r.finished_at.is_some())
        .filter_map(|r| Some((r, serde_json::from_str(&r.report).ok()?)))
        .take(ACTIVITY_ROUNDS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let when = |r: &AutopilotRound| {
        r.started_at
            .map(|t| t.with_timezone(&Local).format("%b %-d, %H:%M").to_string())
            .unwrap_or_default()
    };
    type Measure = fn(&RoundReport) -> usize;
    let measures: [(&str, Measure); 4] = [
        ("Emails sent", |x| {
            x.requests_sent + x.reminders_sent + x.company_requests
        }),
        ("Links confirmed", |x| x.links_confirmed),
        ("Unsubscribed", |x| x.unsubscribed),
        ("New to-dos", |x| x.todos_added),
    ];
    let rows: Vec<Value> = measures
        .iter()
        .map(|(label, f)| {
            let values: Vec<i64> = finished.iter().map(|(_, x)| f(x) as i64).collect();
            let max = values.iter().copied().max().unwrap_or(0);
            let bars: Vec<Value> = values
                .iter()
                .zip(&finished)
                .enumerate()
                .map(|(i, (&v, (r, _)))| {
                    let h = if max > 0 { (v * BAR_H / max).max(2) } else { 2 };
                    let h = if v == 0 { 2 } else { h };
                    json!({
                        "x": i as i64 * (BAR_W + BAR_GAP),
                        "y": BAR_H - h,
                        "h": h,
                        "v": v,
                        "zero": v == 0,
                        "dry": r.dry_run,
                        "title": format!("{}: {v} {}{}", when(r), label.to_lowercase(), if r.dry_run { " (dry run)" } else { "" }),
                    })
                })
                .collect();
            json!({
                "label": label,
                "max": max,
                "total": values.iter().sum::<i64>(),
                "bars": bars,
            })
        })
        .collect();
    let n = finished.len() as i64;
    json!({
        "rows": rows,
        "width": (n * (BAR_W + BAR_GAP) - BAR_GAP).max(1),
        "height": BAR_H,
        "bar_w": BAR_W,
        "first": finished.first().map(|(r, _)| when(r)).unwrap_or_default(),
        "last": finished.last().map(|(r, _)| when(r)).unwrap_or_default(),
        "any_dry": finished.iter().any(|(r, _)| r.dry_run),
        "table": finished.iter().map(|(r, x)| json!({
            "when": when(r),
            "dry": r.dry_run,
            "values": measures.iter().map(|(_, f)| f(x)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{PipelineStatus, Status};

    fn state(days_ago: i64, reminder: bool) -> RequestState {
        RequestState {
            broker_id: "b".into(),
            record_id: 1,
            status: Status::Sent,
            error: String::new(),
            pipeline: None,
            tracking: None,
            sent_at: Some(Local::now() - TimeDelta::days(days_ago)),
            reminder_sent_at: reminder.then(Local::now),
            replies: 0,
            open_tasks: 0,
        }
    }

    #[test]
    fn deadline_bar_states() {
        let now = Local::now();
        let d = deadline(Some(&state(6, false)), now, 20).unwrap();
        assert_eq!((d.day, d.pct, d.reminder_pct, d.state), (6, 20, 66, "ok"));
        assert_eq!(d.text, "Day 6 of 30");
        assert_eq!(
            deadline(Some(&state(21, false)), now, 20).unwrap().state,
            "late"
        );
        let d = deadline(Some(&state(33, true)), now, 30).unwrap();
        assert_eq!((d.pct, d.reminder_pct, d.state), (100, 100, "overdue"));
        assert_eq!(d.text, "3 days overdue");
        let mut done = state(10, false);
        done.pipeline = Some(PipelineStatus::Confirmed);
        assert_eq!(deadline(Some(&done), now, 20), None);
        assert_eq!(deadline(None, now, 20), None);
    }

    #[test]
    fn send_label_per_state() {
        let now = Local::now();
        let b = Broker {
            id: "b".into(),
            email: "privacy@b.example".into(),
            ..Default::default()
        };
        let label = |s: Option<RequestState>| {
            let states: States = s.into_iter().map(|s| (s.broker_id.clone(), s)).collect();
            send_label(&states, &b, now, 20)
        };
        assert_eq!(label(None), Some("Send request"));
        assert_eq!(label(Some(state(5, false))), None, "open, recent");
        assert_eq!(
            label(Some(state(25, false))),
            Some("Send again"),
            "open, past reminder"
        );
        let mut s = state(40, false);
        s.pipeline = Some(PipelineStatus::Confirmed);
        assert_eq!(label(Some(s.clone())), None, "deleted");
        s.pipeline = Some(PipelineStatus::Rejected);
        assert_eq!(label(Some(s.clone())), Some("Send again"));
        s.status = Status::Failed;
        s.pipeline = None;
        assert_eq!(label(Some(s)), Some("Send again"));
        let no_email = Broker {
            email: String::new(),
            ..b.clone()
        };
        assert_eq!(send_label(&States::new(), &no_email, now, 20), None);
    }

    #[test]
    fn sending_eta() {
        let now = Local::now();
        let v = sending(50, 250, 100, now);
        assert_eq!(
            (v["pct"].as_i64(), v["eta_today"].as_bool()),
            (Some(20), Some(true))
        );
        let v = sending(250, 250, 600, now);
        let expect = (now + TimeDelta::days(3)).format("%a, %b %-d").to_string();
        assert_eq!(v["eta"], expect);
        assert_eq!(sending(0, 250, 0, now)["eta"], "");
    }
}
