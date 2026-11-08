use chrono::{DateTime, TimeZone, Utc};
use furball::types::{EventFeeChange, Series, SeriesFeeChange};
use furball::{FeeScheduleRow, build_fee_schedule};
use rust_decimal::Decimal;
use serde_json::json;
use std::collections::BTreeMap;
use std::str::FromStr;

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap()
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn series(ticker: &str, fee_type: &str, multiplier: f64) -> Series {
    serde_json::from_value(json!({
        "ticker": ticker, "title": "t", "frequency": "daily", "category": "c",
        "fee_type": fee_type, "fee_multiplier": multiplier,
    }))
    .unwrap()
}

fn series_change(series: &str, day: u32, fee_type: &str, multiplier: f64) -> SeriesFeeChange {
    serde_json::from_value(json!({
        "id": format!("{series}-{day}-{multiplier}"), "series_ticker": series, "fee_type": fee_type,
        "fee_multiplier": multiplier, "scheduled_ts": at(day).to_rfc3339(),
    }))
    .unwrap()
}

fn event_change(
    event: &str,
    day: u32,
    fee_type: Option<&str>,
    multiplier: Option<f64>,
) -> EventFeeChange {
    serde_json::from_value(json!({
        "id": format!("{event}-{day}"), "event_ticker": event, "series_ticker": "A",
        "fee_type_override": fee_type, "fee_multiplier_override": multiplier,
        "scheduled_ts": at(day).to_rfc3339(),
    }))
    .unwrap()
}

fn assert_no_overlap(rows: &[FeeScheduleRow]) {
    let mut by_key: BTreeMap<(&str, Option<&str>), Vec<&FeeScheduleRow>> = BTreeMap::new();
    for row in rows {
        by_key
            .entry((&row.series_ticker, row.event_ticker.as_deref()))
            .or_default()
            .push(row);
    }
    for (key, mut group) in by_key {
        group.sort_by_key(|row| row.effective_from);
        for pair in group.windows(2) {
            let end = pair[0]
                .effective_to
                .unwrap_or_else(|| panic!("{key:?}: open row followed by another"));
            let next_start = pair[1]
                .effective_from
                .unwrap_or_else(|| panic!("{key:?}: undated row after another"));
            assert!(
                end <= next_start,
                "{key:?}: [{:?}, {end}) overlaps next starting {next_start}",
                pair[0].effective_from
            );
        }
    }
}

#[test]
fn series_changes_become_back_to_back_half_open_intervals_regardless_of_input_order() {
    let rows = build_fee_schedule(
        &[series("A", "flat", 2.0)],
        &[
            series_change("A", 20, "quadratic", 1.0),
            series_change("A", 5, "quadratic_with_maker_fees", 0.5),
            series_change("A", 12, "flat", 2.0),
        ],
        &[],
    );
    let spans: Vec<_> = rows
        .iter()
        .map(|row| (row.effective_from, row.effective_to))
        .collect();
    assert_eq!(
        spans,
        [
            (Some(at(5)), Some(at(12))),
            (Some(at(12)), Some(at(20))),
            (Some(at(20)), None)
        ]
    );
    assert_eq!(
        rows[0].fee_type.as_deref(),
        Some("quadratic_with_maker_fees")
    );
    assert_eq!(rows[0].fee_multiplier, Some(dec("0.5")));
    assert_no_overlap(&rows);
}

#[test]
fn same_instant_changes_keep_the_later_one_so_no_zero_width_interval_exists() {
    let rows = build_fee_schedule(
        &[],
        &[
            series_change("A", 5, "flat", 1.0),
            series_change("A", 5, "quadratic", 3.0),
        ],
        &[],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].fee_type.as_deref(), Some("quadratic"));
    assert_eq!(rows[0].fee_multiplier, Some(dec("3")));
}

#[test]
fn series_without_changes_gets_one_open_row_and_series_with_changes_gets_no_baseline() {
    let rows = build_fee_schedule(
        &[series("A", "quadratic", 1.0), series("B", "flat", 0.5)],
        &[series_change("B", 9, "flat", 0.5)],
        &[],
    );
    let a: Vec<_> = rows.iter().filter(|row| row.series_ticker == "A").collect();
    let b: Vec<_> = rows.iter().filter(|row| row.series_ticker == "B").collect();
    assert_eq!(
        (a.len(), a[0].effective_from, a[0].effective_to),
        (1, None, None)
    );
    assert_eq!((b.len(), b[0].effective_from), (1, Some(at(9))));
}

#[test]
fn cleared_event_override_ends_the_previous_row_and_adds_none() {
    let rows = build_fee_schedule(
        &[],
        &[],
        &[
            event_change("E1", 3, Some("flat"), Some(0.5)),
            event_change("E1", 10, None, None),
            event_change("E1", 15, None, Some(2.0)),
            event_change("E2", 4, Some("flat"), None),
        ],
    );
    let e1: Vec<_> = rows
        .iter()
        .filter(|row| row.event_ticker.as_deref() == Some("E1"))
        .collect();
    assert_eq!(e1.len(), 2);
    assert_eq!(
        (e1[0].effective_from, e1[0].effective_to),
        (Some(at(3)), Some(at(10)))
    );
    assert_eq!(
        (e1[1].effective_from, e1[1].effective_to),
        (Some(at(15)), None)
    );
    assert_eq!(
        (e1[1].fee_type.clone(), e1[1].fee_multiplier),
        (None, Some(dec("2")))
    );
    let e2 = rows
        .iter()
        .find(|row| row.event_ticker.as_deref() == Some("E2"))
        .unwrap();
    assert_eq!(
        (e2.fee_type.as_deref(), e2.fee_multiplier),
        (Some("flat"), None)
    );
    assert_no_overlap(&rows);
}
