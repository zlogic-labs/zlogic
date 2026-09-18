//! Current time, timezone conversion, and calendar/duration arithmetic.

use async_trait::async_trait;
use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Local, LocalResult, Months, NaiveDate,
    NaiveDateTime, TimeZone, Utc,
};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};
use zlogic_protocol::llm::ToolDefinition;

use crate::{
    PromptExample, Result, Tool, ToolCtx, ToolExecResult, ToolMeta, ToolPromptSpec, ToolRisk,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Now,
    Convert,
    Difference,
    Add,
    DateInfo,
    BusinessDays,
    AddBusinessDays,
}

#[derive(Debug, Default, Deserialize, serde::Serialize)]
#[serde(default)]
struct DurationInput {
    years: i32,
    months: i32,
    weeks: i64,
    days: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
}

impl DurationInput {
    fn is_zero(&self) -> bool {
        self.years == 0
            && self.months == 0
            && self.weeks == 0
            && self.days == 0
            && self.hours == 0
            && self.minutes == 0
            && self.seconds == 0
    }

    fn exact(&self) -> Option<Duration> {
        let seconds = self
            .weeks
            .checked_mul(7 * 24 * 60 * 60)?
            .checked_add(self.days.checked_mul(24 * 60 * 60)?)?
            .checked_add(self.hours.checked_mul(60 * 60)?)?
            .checked_add(self.minutes.checked_mul(60)?)?
            .checked_add(self.seconds)?;
        Some(Duration::seconds(seconds))
    }

    fn calendar_months(&self) -> Option<i32> {
        self.years.checked_mul(12)?.checked_add(self.months)
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    operation: Operation,
    /// IANA timezone such as `Asia/Shanghai`. Omitted means the machine's local timezone.
    timezone: Option<String>,
    /// RFC 3339 timestamp for convert/add, or YYYY-MM-DD / RFC 3339 for date_info.
    datetime: Option<String>,
    /// RFC 3339 timestamp for difference; YYYY-MM-DD for business_days.
    start: Option<String>,
    /// RFC 3339 timestamp for difference; YYYY-MM-DD for business_days.
    end: Option<String>,
    duration: Option<DurationInput>,
    /// Signed weekday count for add_business_days.
    amount: Option<i32>,
}

pub struct Time;

#[async_trait]
impl Tool for Time {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "time".into(),
            source: "builtin",
            risk: ToolRisk::Read,
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "time".into(),
            description: "Get the current time without putting a changing date in the system \
                          prompt; convert RFC 3339 timestamps between IANA timezones; calculate \
                          exact elapsed durations; add calendar years/months and exact \
                          weeks/days/time; inspect dates; and count or add business days. Use this \
                          whenever words such as today, now, tomorrow, elapsed, deadline, or \
                          timezone make the answer depend on the real clock."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "description": "now needs no other field; convert/add use datetime; difference and business_days use start/end; date_info optionally uses datetime; add_business_days uses a YYYY-MM-DD datetime and amount",
                        "enum": ["now", "convert", "difference", "add", "date_info", "business_days", "add_business_days"]
                    },
                    "timezone": {
                        "type": "string",
                        "description": "IANA timezone such as Asia/Shanghai or America/New_York; defaults to this machine's local timezone"
                    },
                    "datetime": {
                        "type": "string",
                        "description": "RFC 3339 timestamp for convert/add; YYYY-MM-DD or RFC 3339 for date_info; YYYY-MM-DD for add_business_days"
                    },
                    "start": { "type": "string", "description": "RFC 3339 timestamp for difference; YYYY-MM-DD for business_days" },
                    "end": { "type": "string", "description": "RFC 3339 timestamp for difference; YYYY-MM-DD for business_days" },
                    "duration": {
                        "type": "object",
                        "description": "Signed components to add. Years/months are calendar-aware; the rest are exact elapsed units.",
                        "properties": {
                            "years": { "type": "integer" },
                            "months": { "type": "integer" },
                            "weeks": { "type": "integer" },
                            "days": { "type": "integer" },
                            "hours": { "type": "integer" },
                            "minutes": { "type": "integer" },
                            "seconds": { "type": "integer" }
                        },
                        "additionalProperties": false
                    },
                    "amount": {
                        "type": "integer",
                        "description": "Signed business-day count for add_business_days"
                    }
                },
                "required": ["operation"],
                "additionalProperties": false
            }),
        }
    }

    fn prompt_spec(&self) -> Option<ToolPromptSpec> {
        Some(ToolPromptSpec {
            when: "The system prompt deliberately contains no current date. Reach for this tool \
                whenever the answer depends on now, today, a timezone, elapsed time, or a date \
                range — never on a date you recall.",
            contract: "Use IANA timezone names and RFC 3339 timestamps where the schema requests \
                them.",
            positive_examples: &[
                r#"{"operation":"now","timezone":"Asia/Shanghai"}"#,
                r#"{"operation":"difference","start":"2026-01-01T00:00:00+08:00","end":"2026-01-02T12:00:00+08:00"}"#,
            ],
            negative_examples: &[PromptExample {
                args: r#"{"operation":"now","timezone":"CST"}"#,
                why: "use an unambiguous IANA timezone",
            }],
        })
    }

    async fn execute(&self, _ctx: &ToolCtx, args: &str) -> Result<ToolExecResult> {
        let args: Args = crate::parse_args_with_prompt(args, self.prompt_spec())?;
        let result = match run(args) {
            Ok(value) => value,
            Err(message) => return Ok(ToolExecResult::failed(message)),
        };
        Ok(ToolExecResult::success(
            serde_json::to_string_pretty(&result).expect("time result is JSON"),
        ))
    }
}

fn run(args: Args) -> std::result::Result<Value, String> {
    let timezone = parse_timezone(args.timezone.as_deref())?;
    match args.operation {
        Operation::Now => Ok(render_instant(Utc::now(), timezone)),
        Operation::Convert => {
            let input = required(args.datetime, "datetime")?;
            let instant = parse_timestamp(&input)?;
            let timezone = timezone.ok_or_else(|| {
                "timezone is required for convert (for example `Asia/Shanghai`)".to_string()
            })?;
            Ok(render_instant(instant.with_timezone(&Utc), Some(timezone)))
        }
        Operation::Difference => difference(
            &required(args.start, "start")?,
            &required(args.end, "end")?,
            timezone,
        ),
        Operation::Add => add(
            &required(args.datetime, "datetime")?,
            args.duration
                .ok_or_else(|| "duration is required for add".to_string())?,
            timezone,
        ),
        Operation::DateInfo => date_info(args.datetime.as_deref(), timezone),
        Operation::BusinessDays => {
            business_days(&required(args.start, "start")?, &required(args.end, "end")?)
        }
        Operation::AddBusinessDays => add_business_days(
            &required(args.datetime, "datetime")?,
            args.amount
                .ok_or_else(|| "amount is required for add_business_days".to_string())?,
        ),
    }
}

fn required(value: Option<String>, name: &str) -> std::result::Result<String, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn parse_timezone(value: Option<&str>) -> std::result::Result<Option<Tz>, String> {
    value
        .map(|value| {
            value
                .parse::<Tz>()
                .map_err(|_| format!("{value:?} is not a valid IANA timezone"))
        })
        .transpose()
}

fn parse_timestamp(value: &str) -> std::result::Result<DateTime<FixedOffset>, String> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|error| format!("{value:?} is not a valid RFC 3339 timestamp: {error}"))
}

fn render_instant(utc: DateTime<Utc>, timezone: Option<Tz>) -> Value {
    match timezone {
        Some(timezone) => {
            let local = utc.with_timezone(&timezone);
            json!({
                "datetime": local.to_rfc3339(),
                "date": local.date_naive().to_string(),
                "time": local.time().format("%H:%M:%S").to_string(),
                "timezone": timezone.name(),
                "utc_offset": local.format("%:z").to_string(),
                "utc": utc.to_rfc3339(),
                "unix_seconds": utc.timestamp()
            })
        }
        None => {
            let local = utc.with_timezone(&Local);
            json!({
                "datetime": local.to_rfc3339(),
                "date": local.date_naive().to_string(),
                "time": local.time().format("%H:%M:%S").to_string(),
                "timezone": "local",
                "utc_offset": local.format("%:z").to_string(),
                "utc": utc.to_rfc3339(),
                "unix_seconds": utc.timestamp()
            })
        }
    }
}

fn difference(start: &str, end: &str, timezone: Option<Tz>) -> std::result::Result<Value, String> {
    let start = parse_timestamp(start)?;
    let end = parse_timestamp(end)?;
    let seconds = end.signed_duration_since(start).num_seconds();
    let sign = seconds.signum();
    let absolute = seconds.unsigned_abs();
    let days = absolute / 86_400;
    let hours = absolute % 86_400 / 3_600;
    let minutes = absolute % 3_600 / 60;
    let remainder_seconds = absolute % 60;
    let (start_date, end_date, calendar_timezone) = match timezone {
        Some(timezone) => (
            start.with_timezone(&timezone).date_naive(),
            end.with_timezone(&timezone).date_naive(),
            timezone.name().to_string(),
        ),
        None => (
            start.with_timezone(&Local).date_naive(),
            end.with_timezone(&Local).date_naive(),
            "local".to_string(),
        ),
    };
    let calendar_period = calendar_period(start_date, end_date)?;
    Ok(json!({
        "start": start.to_rfc3339(),
        "end": end.to_rfc3339(),
        "sign": sign,
        "total_seconds": seconds,
        "total_minutes": seconds as f64 / 60.0,
        "total_hours": seconds as f64 / 3_600.0,
        "total_days": seconds as f64 / 86_400.0,
        "calendar_days": (end_date - start_date).num_days(),
        "calendar_timezone": calendar_timezone,
        "calendar_period": calendar_period,
        "decomposed_absolute": {
            "days": days,
            "hours": hours,
            "minutes": minutes,
            "seconds": remainder_seconds
        }
    }))
}

fn calendar_period(start: NaiveDate, end: NaiveDate) -> std::result::Result<Value, String> {
    let (start, end, sign) = if end >= start {
        (start, end, 1)
    } else {
        (end, start, -1)
    };
    let mut months = (end.year() - start.year()) * 12 + end.month() as i32 - start.month() as i32;
    let mut anchor = start
        .checked_add_months(Months::new(months as u32))
        .ok_or_else(|| "calendar period is outside the supported date range".to_string())?;
    if anchor > end {
        months -= 1;
        anchor = start
            .checked_add_months(Months::new(months as u32))
            .ok_or_else(|| "calendar period is outside the supported date range".to_string())?;
    }
    Ok(json!({
        "sign": sign,
        "years": months / 12,
        "months": months % 12,
        "days": (end - anchor).num_days()
    }))
}

fn add(
    datetime: &str,
    duration: DurationInput,
    timezone: Option<Tz>,
) -> std::result::Result<Value, String> {
    if duration.is_zero() {
        return Err("duration must contain at least one non-zero component".into());
    }
    let parsed = parse_timestamp(datetime)?;
    let calendar_months = duration
        .calendar_months()
        .ok_or_else(|| "years/months overflow".to_string())?;
    let exact = duration
        .exact()
        .ok_or_else(|| "duration components overflow".to_string())?;

    let utc = match timezone {
        Some(timezone) => {
            let local = parsed.with_timezone(&Utc).with_timezone(&timezone);
            let shifted = shift_calendar(local.naive_local(), calendar_months)?;
            let shifted = match timezone.from_local_datetime(&shifted) {
                LocalResult::Single(value) => value,
                LocalResult::Ambiguous(earlier, _) => earlier,
                LocalResult::None => {
                    return Err(
                        "calendar addition landed in a local time skipped by daylight saving time"
                            .into(),
                    );
                }
            };
            shifted
                .checked_add_signed(exact)
                .ok_or_else(|| "result is outside the supported date range".to_string())?
                .with_timezone(&Utc)
        }
        None => {
            let shifted = shift_calendar(parsed.naive_local(), calendar_months)?;
            parsed
                .offset()
                .from_local_datetime(&shifted)
                .single()
                .expect("a fixed offset is never ambiguous")
                .checked_add_signed(exact)
                .ok_or_else(|| "result is outside the supported date range".to_string())?
                .with_timezone(&Utc)
        }
    };

    Ok(json!({
        "input": datetime,
        "duration": duration,
        "result": render_instant(utc, timezone)
    }))
}

fn shift_calendar(
    datetime: NaiveDateTime,
    months: i32,
) -> std::result::Result<NaiveDateTime, String> {
    let magnitude = months.unsigned_abs();
    if months >= 0 {
        datetime.checked_add_months(Months::new(magnitude))
    } else {
        datetime.checked_sub_months(Months::new(magnitude))
    }
    .ok_or_else(|| "calendar years/months produce an unsupported date".to_string())
}

fn date_info(value: Option<&str>, timezone: Option<Tz>) -> std::result::Result<Value, String> {
    let date = match value {
        Some(value) => match NaiveDate::parse_from_str(value, "%Y-%m-%d") {
            Ok(date) => date,
            Err(_) => {
                let instant = parse_timestamp(value)?.with_timezone(&Utc);
                match timezone {
                    Some(timezone) => instant.with_timezone(&timezone).date_naive(),
                    None => instant.with_timezone(&Local).date_naive(),
                }
            }
        },
        None => match timezone {
            Some(timezone) => Utc::now().with_timezone(&timezone).date_naive(),
            None => Local::now().date_naive(),
        },
    };
    let days_in_month = days_in_month(date.year(), date.month())?;
    Ok(json!({
        "date": date.to_string(),
        "weekday": date.weekday().to_string(),
        "weekday_number_monday": date.weekday().number_from_monday(),
        "day_of_year": date.ordinal(),
        "iso_week": date.iso_week().week(),
        "iso_week_year": date.iso_week().year(),
        "days_in_month": days_in_month,
        "is_leap_year": days_in_month_for_year(date.year()) == 366
    }))
}

fn days_in_month(year: i32, month: u32) -> std::result::Result<u32, String> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)
        .ok_or_else(|| "date is outside the supported range".to_string())?;
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .ok_or_else(|| "date is outside the supported range".to_string())?;
    Ok((next - first).num_days() as u32)
}

fn days_in_month_for_year(year: i32) -> i64 {
    NaiveDate::from_ymd_opt(year + 1, 1, 1)
        .zip(NaiveDate::from_ymd_opt(year, 1, 1))
        .map(|(end, start)| (end - start).num_days())
        .unwrap_or(365)
}

fn parse_date(value: &str, field: &str) -> std::result::Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|error| format!("{field} must be YYYY-MM-DD: {error}"))
}

/// Signed weekday count in the half-open interval [start, end).
fn business_days(start: &str, end: &str) -> std::result::Result<Value, String> {
    let start = parse_date(start, "start")?;
    let end = parse_date(end, "end")?;
    let direction = if end >= start { 1 } else { -1 };
    let mut cursor = start;
    let mut count = 0_i64;
    while cursor != end {
        if cursor.weekday().number_from_monday() <= 5 {
            count += direction;
        }
        cursor = cursor
            .checked_add_signed(Duration::days(direction))
            .ok_or_else(|| "date range is outside the supported range".to_string())?;
    }
    Ok(json!({
        "start": start.to_string(),
        "end": end.to_string(),
        "interval": "[start, end)",
        "business_days": count,
        "weekends_excluded": true,
        "holidays_excluded": false
    }))
}

fn add_business_days(date: &str, amount: i32) -> std::result::Result<Value, String> {
    let start = parse_date(date, "datetime")?;
    let direction = amount.signum() as i64;
    let mut remaining = amount.unsigned_abs();
    let mut cursor = start;
    while remaining > 0 {
        cursor = cursor
            .checked_add_signed(Duration::days(direction))
            .ok_or_else(|| "result is outside the supported date range".to_string())?;
        if cursor.weekday().number_from_monday() <= 5 {
            remaining -= 1;
        }
    }
    Ok(json!({
        "date": start.to_string(),
        "business_days_added": amount,
        "result": cursor.to_string(),
        "weekends_excluded": true,
        "holidays_excluded": false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difference_reports_signed_totals_and_components() {
        let value = difference(
            "2026-07-30T08:00:00Z",
            "2026-08-01T10:30:15Z",
            Some(chrono_tz::UTC),
        )
        .unwrap();
        assert_eq!(value["total_seconds"], 181_815);
        assert_eq!(value["decomposed_absolute"]["days"], 2);
        assert_eq!(value["decomposed_absolute"]["hours"], 2);
        assert_eq!(value["decomposed_absolute"]["minutes"], 30);
        assert_eq!(value["decomposed_absolute"]["seconds"], 15);
        assert_eq!(value["calendar_days"], 2);
        assert_eq!(value["calendar_period"]["days"], 2);
    }

    #[test]
    fn calendar_period_decomposes_years_months_and_days() {
        let value = calendar_period(
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            NaiveDate::from_ymd_opt(2025, 3, 2).unwrap(),
        )
        .unwrap();
        assert_eq!(value["years"], 1);
        assert_eq!(value["months"], 1);
        assert_eq!(value["days"], 2);
    }

    #[test]
    fn calendar_month_addition_clamps_the_day() {
        let value = add(
            "2024-01-31T12:00:00+08:00",
            DurationInput {
                months: 1,
                ..DurationInput::default()
            },
            Some(chrono_tz::Asia::Shanghai),
        )
        .unwrap();
        assert_eq!(value["result"]["datetime"], "2024-02-29T12:00:00+08:00");
    }

    #[test]
    fn business_day_operations_skip_weekends() {
        let value = business_days("2026-07-31", "2026-08-04").unwrap();
        assert_eq!(value["business_days"], 2);
        let value = add_business_days("2026-07-31", 2).unwrap();
        assert_eq!(value["result"], "2026-08-04");
    }

    #[test]
    fn timezone_conversion_crosses_the_date_line() {
        let value = run(Args {
            operation: Operation::Convert,
            timezone: Some("America/Los_Angeles".into()),
            datetime: Some("2026-01-01T01:00:00+08:00".into()),
            start: None,
            end: None,
            duration: None,
            amount: None,
        })
        .unwrap();
        assert_eq!(value["date"], "2025-12-31");
        assert_eq!(value["timezone"], "America/Los_Angeles");
    }
}
