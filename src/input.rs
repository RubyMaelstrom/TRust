//! HTML input numeric domains and stepping, shared by the DOM and native UI.
//! WHATWG HTML snapshot e5071a20 (2026-09-06), #number-state,
//! #common-input-element-apis, #attr-input-step and #dates-and-times.

const DAY: f64 = 86_400_000.0;

/// HTML's parser deliberately accepts a numeric prefix. Value sanitization
/// instead uses `valid_number`, which checks the entire authoring grammar.
pub(crate) fn parse_number(s: &str) -> Option<f64> {
    let s = s.trim_start_matches(['\t', '\n', '\u{c}', '\r', ' ']);
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let mut digits = i - start;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        digits += i - start;
    }
    if digits == 0 {
        return None;
    }
    let end = i;
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            i = end;
        }
    }
    s[..i]
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|n| if n == 0.0 { 0.0 } else { n })
}

pub(crate) fn valid_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let integer = i > start;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    } else if !integer {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == b.len() && parse_number(s).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumericType {
    Number,
    Range,
    Date,
    Month,
    Week,
    Time,
    DateTime,
}

impl NumericType {
    pub(crate) fn from_type(t: &str) -> Option<Self> {
        Some(match t {
            "number" => Self::Number,
            "range" => Self::Range,
            "date" => Self::Date,
            "month" => Self::Month,
            "week" => Self::Week,
            "time" => Self::Time,
            "datetime-local" => Self::DateTime,
            _ => return None,
        })
    }

    pub(crate) fn parse(self, s: &str) -> Option<f64> {
        match self {
            Self::Number | Self::Range => parse_number(s),
            Self::Month => {
                let (y, m) = month(s)?;
                Some(((y - 1970) * 12 + m - 1) as f64)
            }
            Self::Date => Some(date(s)? as f64 * DAY),
            Self::Week => {
                let (y, w) = s.split_once("-W")?;
                let y = year(y)?;
                let w = two_digits(w)?;
                let start = week_start(y);
                (w >= 1 && w <= (week_start(y + 1) - start) / 7)
                    .then_some((start + (w - 1) * 7) as f64 * DAY)
            }
            Self::Time => time(s),
            Self::DateTime => {
                let (d, t) = s.split_once(['T', ' '])?;
                Some(date(d)? as f64 * DAY + time(t)?)
            }
        }
    }

    pub(crate) fn format(self, n: f64) -> String {
        if !n.is_finite() {
            return String::new();
        }
        if matches!(self, Self::Number | Self::Range) {
            // HTML's "best representation" delegates to ECMA-262
            // #sec-numeric-types-number-tostring (e28783d5, 2026-09-06).
            if n == 0.0 {
                return "0".into();
            }
            if !(1e-6..1e21).contains(&n.abs()) {
                let s = format!("{n:e}");
                let (mantissa, exponent) = s.split_once('e').unwrap();
                return if exponent.starts_with('-') {
                    s
                } else {
                    format!("{mantissa}e+{exponent}")
                };
            }
            return n.to_string();
        }
        if self == Self::Month {
            if n.abs() > 1e12 {
                return String::new();
            }
            let n = n.floor() as i64;
            let y = 1970 + n.div_euclid(12);
            return if y > 0 {
                format!("{y:04}-{:02}", n.rem_euclid(12) + 1)
            } else {
                String::new()
            };
        }
        // Guard integer arithmetic, keeping the entire ECMAScript Date range.
        if n.abs() > 8.64e15 {
            return String::new();
        }
        if self == Self::Time {
            return format_time(n);
        }
        let days = (n / DAY).floor() as i64;
        let (mut y, m, d) = civil_from_days(days);
        if self == Self::Week {
            y = civil_from_days(days + 3 - (days + 3).rem_euclid(7)).0;
            return if y > 0 {
                format!("{y:04}-W{:02}", (days - week_start(y)).div_euclid(7) + 1)
            } else {
                String::new()
            };
        }
        if y <= 0 {
            return String::new();
        }
        let date = format!("{y:04}-{m:02}-{d:02}");
        if self == Self::DateTime {
            format!("{date}T{}", format_time(n))
        } else {
            date
        }
    }
}

fn digits(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}
fn two_digits(s: &str) -> Option<i64> {
    (s.len() == 2).then(|| digits(s)).flatten()
}
fn year(s: &str) -> Option<i64> {
    (s.len() >= 4)
        .then(|| digits(s))
        .flatten()
        .filter(|y| (1..=1_000_000).contains(y))
}
fn month(s: &str) -> Option<(i64, i64)> {
    let (y, m) = s.split_once('-')?;
    Some((year(y)?, two_digits(m).filter(|m| (1..=12).contains(m))?))
}
fn date(s: &str) -> Option<i64> {
    let (ym, d) = s.rsplit_once('-')?;
    let (y, m) = month(ym)?;
    let d = two_digits(d)?;
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max = match m {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (d >= 1 && d <= max).then_some(days_from_civil(y, m, d))
}
// Gregorian 400-year eras; no locale, time-zone or DST conversions.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yo = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    era * 146097 + yo * 365 + yo / 4 - yo / 100 + (153 * mp + 2) / 5 + d - 1 - 719468
}
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yo = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yo + yo / 4 - yo / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (yo + era * 400 + i64::from(m <= 2), m, d)
}
fn week_start(y: i64) -> i64 {
    let jan4 = days_from_civil(y, 1, 4);
    jan4 - (jan4 + 3).rem_euclid(7)
}
fn time(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h = two_digits(parts.next()?)?;
    let m = two_digits(parts.next()?)?;
    let seconds = parts.next();
    if h > 23 || m > 59 || parts.next().is_some() {
        return None;
    }
    let mut ms = 0;
    if let Some(seconds) = seconds {
        let (s, fraction) = seconds
            .split_once('.')
            .map_or((seconds, None), |(s, f)| (s, Some(f)));
        let s = two_digits(s)?;
        if s > 59 {
            return None;
        }
        ms = s * 1000;
        if let Some(f) = fraction {
            if !(1..=3).contains(&f.len()) {
                return None;
            }
            ms += digits(f)? * 10i64.pow(3 - f.len() as u32);
        }
    }
    Some(((h * 60 + m) * 60 * 1000 + ms) as f64)
}
fn format_time(n: f64) -> String {
    let ms = n.floor().rem_euclid(DAY) as i64;
    let mut s = format!("{:02}:{:02}", ms / 3_600_000, ms / 60_000 % 60);
    if ms % 60_000 != 0 {
        s.push_str(&format!(":{:02}", ms / 1000 % 60));
        if ms % 1000 != 0 {
            s.push_str(format!(".{:03}", ms % 1000).trim_end_matches('0'));
        }
    }
    s
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NumericInput {
    pub kind: NumericType,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub base: f64,
}

impl NumericInput {
    pub(crate) fn new(
        kind: NumericType,
        min: Option<&str>,
        max: Option<&str>,
        step: Option<&str>,
        value: Option<&str>,
    ) -> Self {
        let parsed_min = min.and_then(|s| kind.parse(s));
        let min = parsed_min.or((kind == NumericType::Range).then_some(0.0));
        let max = max
            .and_then(|s| kind.parse(s))
            .or((kind == NumericType::Range).then_some(100.0));
        let base = parsed_min
            .or_else(|| value.and_then(|s| kind.parse(s)))
            .unwrap_or(if kind == NumericType::Week {
                -259_200_000.0
            } else {
                0.0
            });
        let scale = match kind {
            NumericType::Date => DAY,
            NumericType::Week => 7.0 * DAY,
            NumericType::Time | NumericType::DateTime => 1000.0,
            _ => 1.0,
        };
        let default = if matches!(kind, NumericType::Time | NumericType::DateTime) {
            60.0
        } else {
            1.0
        };
        let step = if step.is_some_and(|s| s.eq_ignore_ascii_case("any")) {
            None
        } else {
            Some(
                step.and_then(parse_number)
                    .filter(|s| *s > 0.0)
                    .unwrap_or(default)
                    * scale,
            )
        };
        Self {
            kind,
            min,
            max,
            step,
            base,
        }
    }

    pub(crate) fn sanitize(self, value: &str) -> String {
        let valid = if matches!(self.kind, NumericType::Number | NumericType::Range) {
            valid_number(value)
        } else {
            self.kind.parse(value).is_some()
        };
        if self.kind == NumericType::Range {
            let min = self.min.unwrap();
            let max = self.max.unwrap();
            let mut n = if valid {
                parse_number(value).unwrap()
            } else if max < min {
                min
            } else {
                min / 2.0 + max / 2.0
            };
            let original = n;
            n = n.max(min);
            // Range overflow correction is explicitly disabled for reversed
            // bounds; min still applies and stepping APIs remain a no-op.
            let upper_bound = if max < min { f64::INFINITY } else { max };
            n = n.min(upper_bound);
            if let Some(step) = self.step
                && self.mismatch(n)
            {
                let q = self.quotient(n, step);
                let lower = self.grid(q.floor(), step);
                let upper = self.grid(q.ceil(), step);
                if lower >= min && (upper > upper_bound || n - lower < upper - n) {
                    n = lower;
                } else if upper <= upper_bound {
                    n = upper;
                }
            }
            if valid && n == original {
                value.into()
            } else {
                self.kind.format(n)
            }
        } else if !valid {
            String::new()
        } else if self.kind == NumericType::DateTime {
            self.kind.format(self.kind.parse(value).unwrap())
        } else {
            value.into()
        }
    }

    // Round only within a few floating-point ULPs, never an absolute epsilon:
    // a 1e-12 step is just as real as a unit step.
    fn quotient(self, n: f64, step: f64) -> f64 {
        let q = (n - self.base) / step;
        if (q - q.round()).abs() <= (q.abs() * f64::EPSILON * 4.0).min(1e-7) {
            q.round()
        } else {
            q
        }
    }
    fn grid(self, q: f64, step: f64) -> f64 {
        // Work in decimal units when they fit exactly in binary64. This keeps
        // ordinary decimal steps (0.1, 0.01, ...) from accumulating drift.
        let exponent = |n: f64| {
            let s = n.to_string();
            let (mantissa, exp) = s
                .split_once(['e', 'E'])
                .map_or((s.as_str(), 0), |(m, e)| (m, e.parse::<i32>().unwrap_or(0)));
            exp - mantissa.split_once('.').map_or(0, |(_, f)| f.len() as i32)
        };
        let exp = exponent(step).min(exponent(self.base));
        let scale = 10f64.powi(-exp.clamp(-308, 308));
        let b = self.base * scale;
        let s = step * scale;
        let n = b.round() + q * s.round();
        if s >= 1.0 && b.abs() < 4.5e15 && n.abs() < 4.5e15 && n.is_finite() {
            n / scale
        } else {
            q.mul_add(step, self.base)
        }
    }
    pub(crate) fn mismatch(self, n: f64) -> bool {
        self.step.is_some_and(|step| {
            let q = self.quotient(n, step);
            q.is_finite() && q.fract() != 0.0
        })
    }
    /// HTML #dom-input-stepup: method direction is distinct from the signed
    /// Web IDL count, including during alignment and the direction guard.
    pub(crate) fn stepped(
        self,
        current: &str,
        down: bool,
        count: i32,
    ) -> Result<Option<String>, ()> {
        let step = self.step.ok_or(())?;
        if !step.is_finite() || self.min.zip(self.max).is_some_and(|(a, b)| a > b) {
            return Ok(None);
        }
        let low = self
            .min
            .map(|n| self.grid(self.quotient(n, step).ceil(), step));
        let high = self
            .max
            .map(|n| self.grid(self.quotient(n, step).floor(), step));
        if low.zip(high).is_some_and(|(a, b)| a > b) {
            return Ok(None);
        }
        let before = self.kind.parse(current).unwrap_or(0.0);
        let q = self.quotient(before, step);
        if !q.is_finite() {
            return Ok(None);
        }
        let q = if q.fract() != 0.0 {
            if down { q.floor() } else { q.ceil() }
        } else {
            q + if down {
                -f64::from(count)
            } else {
                f64::from(count)
            }
        };
        let mut n = self.grid(q, step);
        if let Some(low) = low {
            n = n.max(low);
        }
        if let Some(high) = high {
            n = n.min(high);
        }
        if !n.is_finite() || (down && n > before) || (!down && n < before) {
            return Ok(None);
        }
        Ok(Some(self.kind.format(n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_input_grammar_and_decimal_stepping() {
        for s in ["+1", " 1", "1.", "1e", "0x10", "NaN", "Infinity", "1e309"] {
            assert!(!valid_number(s), "{s}");
        }
        for s in [".5", "-.5", "1e+2", "-0", "1e-12"] {
            assert!(valid_number(s), "{s}");
        }
        assert_eq!(parse_number(" \t+1.5oops"), Some(1.5));
        let c = NumericInput::new(NumericType::Number, None, None, Some("0.1"), None);
        let mut value = "0".to_string();
        for _ in 0..10 {
            value = c.stepped(&value, false, 1).unwrap().unwrap();
        }
        assert_eq!(value, "1");
        assert_eq!(c.stepped("0.25", false, 5).unwrap().as_deref(), Some("0.3"));
        assert_eq!(c.stepped("1", false, -1).unwrap(), None);
        let tiny = NumericInput::new(NumericType::Number, None, None, Some("1e-12"), None);
        assert_eq!(
            tiny.stepped("0", false, 1).unwrap().as_deref(),
            Some("1e-12")
        );
        for (n, expected) in [
            (-0.0, "0"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (-1.25e21, "-1.25e+21"),
            (f64::from_bits(1), "5e-324"),
        ] {
            assert_eq!(NumericType::Number.format(n), expected);
        }
        let range = NumericInput::new(NumericType::Range, None, None, None, None);
        assert_eq!(range.sanitize("0050.0"), "0050.0");
        let reversed =
            NumericInput::new(NumericType::Range, Some("10"), Some("5"), Some("3"), None);
        assert_eq!(reversed.sanitize("4"), "10");
        assert_eq!(reversed.sanitize("20"), "19");
        assert_eq!(reversed.stepped("19", false, 1), Ok(None));
    }
    #[test]
    fn numeric_input_calendar_domains() {
        for (kind, value, number) in [
            (NumericType::Date, "1970-01-01", 0.0),
            (NumericType::Date, "1969-12-31", -DAY),
            (NumericType::Date, "2024-02-29", 1709164800000.0),
            (NumericType::Month, "1969-12", -1.0),
            (NumericType::Week, "1970-W01", -3.0 * DAY),
            (NumericType::Week, "2020-W53", 1609113600000.0),
            (NumericType::Time, "12:34:56.789", 45296789.0),
            (NumericType::DateTime, "1970-01-01T00:01", 60000.0),
        ] {
            assert_eq!(kind.parse(value), Some(number), "{value}");
            assert_eq!(kind.format(number), value);
        }
        assert_eq!(NumericType::Date.parse("1900-02-29"), None);
        assert_eq!(NumericType::Week.parse("2021-W53"), None);
        assert_eq!(NumericType::Time.parse("24:00"), None);
        assert_eq!(NumericType::Time.parse("12:00:00.0001"), None);
    }
}
