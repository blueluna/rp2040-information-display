pub const TIMEZONE: jiff::tz::TimeZone = jiff::tz::get!("Europe/Stockholm");

/// Convert a jiff Timestamp to a time::PrimitiveDateTime (UTC) for the RTC.
pub fn unix_to_primitive_datetime(timestamp: jiff::Timestamp) -> Option<time::PrimitiveDateTime> {
    let dt = jiff::tz::Offset::UTC.to_datetime(timestamp);
    let date = time::Date::from_calendar_date(
        dt.year() as i32,
        time::Month::try_from(dt.month() as u8).ok()?,
        dt.day() as u8,
    )
    .ok()?;
    let t = time::Time::from_hms(dt.hour() as u8, dt.minute() as u8, dt.second() as u8).ok()?;
    Some(time::PrimitiveDateTime::new(date, t))
}

/// Convert a time::PrimitiveDateTime (UTC) read from the RTC back to a jiff Timestamp.
pub fn rtc_to_jiff(dt: time::PrimitiveDateTime) -> Option<jiff::Timestamp> {
    jiff::Timestamp::from_second(dt.assume_utc().unix_timestamp()).ok()
}
