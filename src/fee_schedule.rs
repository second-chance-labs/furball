//! Fee schedule: series fees and event overrides as non-overlapping time intervals.
//!
//! Rows are half-open `[effective_from, effective_to)` intervals. Rows with a `None`
//! `event_ticker` are series fees; rows with an event ticker are overrides layered on the
//! series fee, where a `None` fee field means "inherit from the series".

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

use crate::types::{EventFeeChange, Series, SeriesFeeChange};

/// One interval of fee schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct FeeScheduleRow {
    /// Series the fee applies to.
    pub series_ticker: String,
    /// Set for event overrides, `None` for series fees.
    pub event_ticker: Option<String>,
    /// Start of the interval (inclusive); `None` when the fee has applied since before any recorded change.
    pub effective_from: Option<DateTime<Utc>>,
    /// End of the interval (exclusive); `None` while still in force.
    pub effective_to: Option<DateTime<Utc>>,
    /// Kalshi fee type name; `None` on an override that inherits the series fee type.
    pub fee_type: Option<String>,
    /// Fee multiplier; `None` on an override that inherits the series multiplier.
    pub fee_multiplier: Option<Decimal>,
    /// Endpoint the row came from.
    pub source: String,
}

/// Builds the fee schedule from current series records, series fee changes and event overrides.
///
/// - A series with no changes gets one open-ended row from its current fee.
/// - A series with changes gets one row per change, each ending where the next begins. The fee
///   before the first change is not reported by Kalshi, so no row covers it.
/// - Two changes at the same instant keep the later one in input order.
/// - An event override with no fee fields clears the override: it ends the previous row and adds none.
pub fn build_fee_schedule(
    series: &[Series],
    series_changes: &[SeriesFeeChange],
    event_changes: &[EventFeeChange],
) -> Vec<FeeScheduleRow> {
    let mut rows = Vec::new();

    let mut changes_by_series: BTreeMap<&str, Vec<&SeriesFeeChange>> = BTreeMap::new();
    for change in series_changes {
        changes_by_series
            .entry(&change.series_ticker)
            .or_default()
            .push(change);
    }
    for current in series {
        if !changes_by_series.contains_key(current.ticker.as_str()) {
            rows.push(FeeScheduleRow {
                series_ticker: current.ticker.clone(),
                event_ticker: None,
                effective_from: None,
                effective_to: None,
                fee_type: Some(current.fee_type.clone().into()),
                fee_multiplier: Some(current.fee_multiplier),
                source: format!("GET /series/{}", current.ticker),
            });
        }
    }
    for (ticker, changes) in changes_by_series {
        let changes = latest_per_instant(changes, |change| change.scheduled_ts);
        for (index, change) in changes.iter().enumerate() {
            rows.push(FeeScheduleRow {
                series_ticker: ticker.to_owned(),
                event_ticker: None,
                effective_from: Some(change.scheduled_ts),
                effective_to: changes.get(index + 1).map(|next| next.scheduled_ts),
                fee_type: Some(change.fee_type.clone().into()),
                fee_multiplier: Some(change.fee_multiplier),
                source: "GET /series/fee_changes".into(),
            });
        }
    }

    let mut overrides_by_event: BTreeMap<(&str, &str), Vec<&EventFeeChange>> = BTreeMap::new();
    for change in event_changes {
        overrides_by_event
            .entry((&change.series_ticker, &change.event_ticker))
            .or_default()
            .push(change);
    }
    for ((series_ticker, event_ticker), changes) in overrides_by_event {
        let changes = latest_per_instant(changes, |change| change.scheduled_ts);
        for (index, change) in changes.iter().enumerate() {
            if change.fee_type_override.is_none() && change.fee_multiplier_override.is_none() {
                continue;
            }
            rows.push(FeeScheduleRow {
                series_ticker: series_ticker.to_owned(),
                event_ticker: Some(event_ticker.to_owned()),
                effective_from: Some(change.scheduled_ts),
                effective_to: changes.get(index + 1).map(|next| next.scheduled_ts),
                fee_type: change.fee_type_override.clone().map(Into::into),
                fee_multiplier: change.fee_multiplier_override,
                source: "GET /events/fee_changes".into(),
            });
        }
    }

    rows.sort_by(|a, b| {
        (&a.series_ticker, &a.event_ticker, a.effective_from).cmp(&(
            &b.series_ticker,
            &b.event_ticker,
            b.effective_from,
        ))
    });
    rows
}

/// Sorts by instant and keeps only the last item of each instant.
fn latest_per_instant<T>(mut items: Vec<&T>, instant: impl Fn(&T) -> DateTime<Utc>) -> Vec<&T> {
    items.sort_by_key(|item| instant(item));
    let mut kept: Vec<&T> = Vec::with_capacity(items.len());
    for item in items {
        match kept.last() {
            Some(last) if instant(last) == instant(item) => {
                kept.pop();
            }
            _ => {}
        }
        kept.push(item);
    }
    kept
}
