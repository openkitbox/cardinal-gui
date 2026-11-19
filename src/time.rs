//! Jiff time boundary for GUI business state.
//!
//! `gpui-component` 0.10 still exposes date-picker values from its own
//! dependency. The adapter below keeps that implementation detail at the
//! boundary; application state and calculations use Jiff.

use gpui_kit::component::calendar::Date as PickerDate;
use gpui_kit::component::date_picker::DatePickerState;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

pub(crate) fn picker_date(value: PickerDate) -> Option<Date> {
    let value = value.start()?;
    value.to_string().parse().ok()
}

pub(crate) fn picker_value(date: Option<Date>) -> PickerDate {
    PickerDate::Single(date.and_then(|date| date.to_string().parse().ok()))
}

pub(crate) fn monday_picker(state: DatePickerState) -> DatePickerState {
    state.first_day_of_week("Mon".parse().expect("Monday is a valid weekday"))
}

pub(crate) fn local_timestamp(date: Date, time: Time, zone: TimeZone) -> Option<Timestamp> {
    zone.to_ambiguous_timestamp(time.to_datetime(date))
        .unambiguous()
        .ok()
}

pub(crate) fn date(at: Timestamp, zone: TimeZone) -> Date {
    at.to_zoned_datetime(zone).date()
}

pub(crate) fn format(at: Timestamp, zone: TimeZone, pattern: &str) -> String {
    at.to_zoned_datetime(zone).strftime(pattern).to_string()
}

pub(crate) fn offset(at: Timestamp, zone: TimeZone) -> SignedDuration {
    SignedDuration::from_secs(i64::from(at.to_zoned_datetime(zone).offset().seconds()))
}

pub(crate) fn now() -> Timestamp {
    Timestamp::now()
}
