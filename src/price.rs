//! BTC/USD price for marking a payment to market.
//!
//! The price is fetched only when a payment is ready to settle, from Coinbase,
//! Bitstamp and Gemini, and combined by a fixed rule (see [`aggregate`]).
//! Prices are whole US cents per BTC, parsed from decimal strings with no
//! floating point anywhere.
//!
//! Endpoints and fields, checked against live responses:
//!
//! - Coinbase `GET https://api.exchange.coinbase.com/products/BTC-USD/ticker`:
//!   `price` (decimal string, last trade) and `time` (ISO 8601 UTC, up to
//!   nanosecond fraction).
//! - Bitstamp `GET https://www.bitstamp.net/api/v2/ticker/btcusd/`: `last`
//!   (decimal string) and `timestamp` (string, unix seconds).
//! - Gemini `GET https://api.gemini.com/v1/pubticker/btcusd`: `last` (decimal
//!   string) and `volume.timestamp` (number, unix milliseconds, aligned to the
//!   minute). The Gemini timestamp is undocumented and coarse, so it only
//!   bounds the age of the quote to about a minute.

use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;

pub const DEFAULT_MAX_AGE_SECS: u64 = 120;
/// 2%, in basis points.
pub const DEFAULT_MAX_SPREAD_BPS: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// A quote whose timestamp is further than this from now is excluded.
    pub max_age_secs: u64,
    /// With two usable quotes, wait if they differ by more than this many basis
    /// points of the lower one.
    pub max_spread_bps: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            max_spread_bps: DEFAULT_MAX_SPREAD_BPS,
        }
    }
}

/// One source's quote: whole cents per BTC and when the source says it was taken
/// (unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    pub cents: u64,
    pub observed_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    Price { cents: u64, sources: usize },
    /// Fewer than two usable prices. Do not issue on one feed.
    TooFewSources { usable: usize },
    /// Two usable prices that differ by more than the allowed spread.
    Disagree { low: u64, high: u64 },
}

/// Pure combination rule over the usable (already filtered) prices.
///
/// - three: the median
/// - two: their average (rounded down), unless they differ by more than
///   `max_spread_bps` of the lower one, in which case wait
/// - one or none: wait
pub fn aggregate(usable: &[u64], max_spread_bps: u64) -> Aggregate {
    let mut sorted = usable.to_vec();
    sorted.sort_unstable();
    match sorted.as_slice() {
        [_, mid, _] => Aggregate::Price {
            cents: *mid,
            sources: 3,
        },
        [low, high] => {
            let spread = u128::from(high - low) * 10_000;
            let allowed = u128::from(*low) * u128::from(max_spread_bps);
            if spread > allowed {
                Aggregate::Disagree {
                    low: *low,
                    high: *high,
                }
            } else {
                Aggregate::Price {
                    cents: ((u128::from(*low) + u128::from(*high)) / 2) as u64,
                    sources: 2,
                }
            }
        }
        other => Aggregate::TooFewSources {
            usable: other.len(),
        },
    }
}

/// Keep the quotes whose timestamp is within `max_age_secs` of `now`, in either
/// direction (a timestamp far in the future means a clock problem). Returns the
/// prices and, for each excluded source, why.
pub fn usable(
    reports: &[(&'static str, Result<Quote, String>)],
    now: u64,
    max_age_secs: u64,
) -> (Vec<u64>, Vec<(&'static str, String)>) {
    let mut prices = Vec::new();
    let mut excluded = Vec::new();
    for (name, report) in reports {
        match report {
            Err(e) => excluded.push((*name, e.clone())),
            Ok(q) if now.abs_diff(q.observed_at) > max_age_secs => excluded.push((
                *name,
                format!(
                    "stale: quote time {} is {} s from now",
                    q.observed_at,
                    now.abs_diff(q.observed_at)
                ),
            )),
            Ok(q) => prices.push(q.cents),
        }
    }
    (prices, excluded)
}

/// What the whole lookup concluded, with the per-source detail for the log.
#[derive(Debug)]
pub struct Lookup {
    pub aggregate: Aggregate,
    pub excluded: Vec<(&'static str, String)>,
}

pub fn combine(
    reports: &[(&'static str, Result<Quote, String>)],
    now: u64,
    config: Config,
) -> Lookup {
    let (prices, excluded) = usable(reports, now, config.max_age_secs);
    Lookup {
        aggregate: aggregate(&prices, config.max_spread_bps),
        excluded,
    }
}

/// Parse a non-negative decimal string such as `"81672.5"` into whole cents,
/// dropping anything past the second decimal. No floating point.
pub fn parse_cents(s: &str) -> Result<u64, String> {
    let bad = || format!("not a decimal price: {s:?}");
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    let digits = |x: &str| x.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(frac) {
        return Err(bad());
    }
    let mut cents_part = frac.bytes().take(2).map(|b| u64::from(b - b'0'));
    let tens = cents_part.next().unwrap_or(0);
    let ones = cents_part.next().unwrap_or(0);
    let cents = whole
        .parse::<u64>()
        .ok()
        .and_then(|w| w.checked_mul(100))
        .and_then(|w| w.checked_add(tens * 10 + ones))
        .ok_or_else(bad)?;
    if cents == 0 {
        return Err(format!("price is zero: {s:?}"));
    }
    Ok(cents)
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fraction]Z` (UTC) into unix seconds. The fraction
/// may have any number of digits and is dropped.
pub fn parse_iso8601_utc(s: &str) -> Result<u64, String> {
    let bad = || format!("not an ISO 8601 UTC time: {s:?}");
    let b = s.as_bytes();
    if !s.is_ascii() || b.len() < 20 || *b.last().unwrap() != b'Z' {
        return Err(bad());
    }
    let num = |from: usize, to: usize| -> Result<u64, String> {
        let part = &s[from..to];
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().map_err(|_| bad())
        } else {
            Err(bad())
        }
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return Err(bad());
    }
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if b[19] == b'.' {
        let frac = &s[20..s.len() - 1];
        if frac.is_empty() || !frac.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
    } else if b.len() != 20 {
        return Err(bad());
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return Err(bad());
    }
    let days = days_from_civil(year as i64, month as i64, day as i64);
    if days < 0 {
        return Err(bad());
    }
    Ok(days as u64 * 86_400 + hour * 3600 + minute * 60 + second)
}

// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Deserialize)]
struct CoinbaseTicker {
    price: String,
    time: String,
}

#[derive(Deserialize)]
struct BitstampTicker {
    last: String,
    timestamp: String,
}

#[derive(Deserialize)]
struct GeminiTicker {
    last: String,
    volume: GeminiVolume,
}

#[derive(Deserialize)]
struct GeminiVolume {
    timestamp: u64,
}

fn json<'a, T: Deserialize<'a>>(body: &'a str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| format!("unexpected response: {e}"))
}

pub fn parse_coinbase(body: &str) -> Result<Quote, String> {
    let t: CoinbaseTicker = json(body)?;
    Ok(Quote {
        cents: parse_cents(&t.price)?,
        observed_at: parse_iso8601_utc(&t.time)?,
    })
}

pub fn parse_bitstamp(body: &str) -> Result<Quote, String> {
    let t: BitstampTicker = json(body)?;
    Ok(Quote {
        cents: parse_cents(&t.last)?,
        observed_at: t
            .timestamp
            .parse()
            .map_err(|_| format!("bad timestamp: {:?}", t.timestamp))?,
    })
}

pub fn parse_gemini(body: &str) -> Result<Quote, String> {
    let t: GeminiTicker = json(body)?;
    Ok(Quote {
        cents: parse_cents(&t.last)?,
        observed_at: t.volume.timestamp / 1000,
    })
}

type Parser = fn(&str) -> Result<Quote, String>;

const SOURCES: [(&str, &str, Parser); 3] = [
    (
        "coinbase",
        "https://api.exchange.coinbase.com/products/BTC-USD/ticker",
        parse_coinbase,
    ),
    (
        "bitstamp",
        "https://www.bitstamp.net/api/v2/ticker/btcusd/",
        parse_bitstamp,
    ),
    (
        "gemini",
        "https://api.gemini.com/v1/pubticker/btcusd",
        parse_gemini,
    ),
];

/// Fetch all three tickers. A failure of one source is reported for that source
/// and does not stop the others.
pub fn fetch_all() -> Vec<(&'static str, Result<Quote, String>)> {
    let client = match Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("psp-mint")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return SOURCES
                .iter()
                .map(|(name, _, _)| (*name, Err(format!("http client: {e}"))))
                .collect();
        }
    };
    SOURCES
        .iter()
        .map(|(name, url, parse)| {
            let body = client
                .get(*url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.text())
                .map_err(|e| format!("request failed: {e}"));
            (*name, body.and_then(|b| parse(&b)))
        })
        .collect()
}

/// Asks for the price at most once per run, and only when first needed. A
/// failed lookup is remembered too, so one run never hits the exchanges twice.
pub struct Lazy<F: FnMut() -> Result<u64, String>> {
    fetch: F,
    cached: Option<Result<u64, String>>,
}

impl<F: FnMut() -> Result<u64, String>> Lazy<F> {
    pub fn new(fetch: F) -> Self {
        Lazy {
            fetch,
            cached: None,
        }
    }

    pub fn get(&mut self) -> Result<u64, String> {
        if self.cached.is_none() {
            self.cached = Some((self.fetch)());
        }
        self.cached.clone().expect("just set")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(cents: &[u64]) -> Aggregate {
        aggregate(cents, DEFAULT_MAX_SPREAD_BPS)
    }

    #[test]
    fn three_prices_take_the_median() {
        for order in [[1, 2, 3], [3, 1, 2], [2, 3, 1]] {
            let v: Vec<u64> = order.iter().map(|x| x * 1_000_000).collect();
            assert_eq!(p(&v), Aggregate::Price { cents: 2_000_000, sources: 3 });
        }
    }

    #[test]
    fn three_prices_ignore_an_outlier_even_a_wild_one() {
        assert_eq!(
            p(&[8_000_000, 8_010_000, 1]),
            Aggregate::Price { cents: 8_000_000, sources: 3 }
        );
    }

    #[test]
    fn two_close_prices_are_averaged() {
        assert_eq!(
            p(&[8_000_000, 8_100_000]),
            Aggregate::Price { cents: 8_050_000, sources: 2 }
        );
        assert_eq!(
            p(&[8_100_001, 8_000_000]),
            Aggregate::Price { cents: 8_050_000, sources: 2 }
        );
    }

    #[test]
    fn two_prices_exactly_two_percent_apart_are_averaged() {
        // 2% of 10_000_000 is 200_000
        assert_eq!(
            p(&[10_000_000, 10_200_000]),
            Aggregate::Price { cents: 10_100_000, sources: 2 }
        );
    }

    #[test]
    fn two_prices_over_two_percent_apart_wait() {
        assert_eq!(
            p(&[10_000_000, 10_200_001]),
            Aggregate::Disagree { low: 10_000_000, high: 10_200_001 }
        );
    }

    #[test]
    fn the_spread_limit_is_configurable() {
        assert_eq!(
            aggregate(&[10_000_000, 10_100_000], 50),
            Aggregate::Disagree { low: 10_000_000, high: 10_100_000 }
        );
        assert_eq!(
            aggregate(&[10_000_000, 10_100_000], 100),
            Aggregate::Price { cents: 10_050_000, sources: 2 }
        );
    }

    #[test]
    fn one_or_no_price_waits() {
        assert_eq!(p(&[8_000_000]), Aggregate::TooFewSources { usable: 1 });
        assert_eq!(p(&[]), Aggregate::TooFewSources { usable: 0 });
    }

    #[test]
    fn large_prices_do_not_overflow() {
        assert_eq!(
            aggregate(&[u64::MAX, u64::MAX - 1], DEFAULT_MAX_SPREAD_BPS),
            Aggregate::Price { cents: u64::MAX - 1, sources: 2 }
        );
    }

    fn q(cents: u64, observed_at: u64) -> Result<Quote, String> {
        Ok(Quote { cents, observed_at })
    }

    #[test]
    fn errored_and_stale_sources_are_excluded() {
        let now = 1_000_000;
        let reports = vec![
            ("coinbase", q(8_000_000, now - 10)),
            ("bitstamp", Err("request failed".to_string())),
            ("gemini", q(8_010_000, now - 121)),
        ];
        let (prices, excluded) = usable(&reports, now, 120);
        assert_eq!(prices, vec![8_000_000]);
        assert_eq!(excluded.len(), 2);
        assert_eq!(excluded[0].0, "bitstamp");
        assert_eq!(excluded[1].0, "gemini");
    }

    #[test]
    fn a_quote_exactly_at_the_age_limit_is_kept() {
        let (prices, _) = usable(&[("a", q(5, 1000 - 120))], 1000, 120);
        assert_eq!(prices, vec![5]);
    }

    #[test]
    fn a_quote_dated_far_in_the_future_is_excluded() {
        let (prices, excluded) = usable(&[("a", q(5, 1000 + 121))], 1000, 120);
        assert!(prices.is_empty());
        assert_eq!(excluded.len(), 1);
    }

    #[test]
    fn the_age_limit_is_configurable() {
        let reports = vec![("a", q(5, 900)), ("b", q(5, 990))];
        assert_eq!(usable(&reports, 1000, 120).0.len(), 2);
        assert_eq!(usable(&reports, 1000, 30).0.len(), 1);
    }

    #[test]
    fn two_stale_sources_leave_one_and_so_wait() {
        let now = 1_000_000;
        let reports = vec![
            ("coinbase", q(8_000_000, now)),
            ("bitstamp", q(8_000_000, now - 500)),
            ("gemini", q(8_000_000, now - 500)),
        ];
        let lookup = combine(&reports, now, Config::default());
        assert_eq!(lookup.aggregate, Aggregate::TooFewSources { usable: 1 });
        assert_eq!(lookup.excluded.len(), 2);
    }

    #[test]
    fn parses_decimal_strings_to_cents() {
        assert_eq!(parse_cents("81672.5"), Ok(8_167_250));
        assert_eq!(parse_cents("81672.50"), Ok(8_167_250));
        assert_eq!(parse_cents("81672"), Ok(8_167_200));
        assert_eq!(parse_cents("81672.12345"), Ok(8_167_212));
        assert_eq!(parse_cents("81672.99999"), Ok(8_167_299));
        assert_eq!(parse_cents("0.05"), Ok(5));
        assert_eq!(parse_cents("81672."), Ok(8_167_200));
    }

    #[test]
    fn rejects_bad_decimal_strings() {
        for s in ["", ".5", "-1.00", "+1.00", "1e5", "1,000.00", "abc", "1.2.3", "0", "0.00", " 1", "1.0x"] {
            assert!(parse_cents(s).is_err(), "{s:?} was accepted");
        }
        assert!(parse_cents("99999999999999999999.00").is_err());
        assert!(parse_cents("184467440737095516.16").is_err());
    }

    #[test]
    fn parses_iso8601_with_any_fraction_length() {
        // 2026-10-14T17:09:14Z is unix 1791997754
        assert_eq!(parse_iso8601_utc("2026-10-14T17:09:14Z"), Ok(1_791_997_754));
        assert_eq!(parse_iso8601_utc("2026-10-14T17:09:14.4Z"), Ok(1_791_997_754));
        assert_eq!(parse_iso8601_utc("2026-10-14T17:09:14.493778Z"), Ok(1_791_997_754));
        assert_eq!(
            parse_iso8601_utc("2026-10-14T17:09:14.493778247Z"),
            Ok(1_791_997_754)
        );
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_iso8601_utc("2000-02-29T00:00:00Z"), Ok(951_782_400));
    }

    #[test]
    fn rejects_bad_times() {
        for s in [
            "",
            "2026-10-14",
            "2026-10-14T17:09:14",
            "2026-10-14 17:09:14Z",
            "2026-10-14T17:09:14.Z",
            "2026-10-14T17:09:14+00:00",
            "2026-13-14T17:09:14Z",
            "2026-10-14T25:09:14Z",
            "2026-10-14T17:09:14xZ",
        ] {
            assert!(parse_iso8601_utc(s).is_err(), "{s:?} was accepted");
        }
    }

    // Bodies shaped like the live responses (same fields and string forms),
    // with representative values.
    const COINBASE_BODY: &str = r#"{"ask":"81673.55","bid":"81673.54","volume":"5321.12345678","trade_id":1234567,"price":"81673.55","size":"0.00012345","time":"2026-10-14T17:09:14.493778247Z","rfq_volume":"1.5"}"#;
    const BITSTAMP_BODY: &str = r#"{"high":"82000.00","last":"81670.12","timestamp":"1791997755","bid":"81669.00","vwap":"81500.50","volume":"1234.56789012","low":"80900.00","ask":"81671.00","open":"81100.00","open_24":"81050.00","percent_change_24":"0.76","side":"1","pair":"BTC/USD"}"#;
    const GEMINI_BODY: &str = r#"{"bid":"81669.50","ask":"81672.25","volume":{"BTC":"123.45678901","USD":"10080000.12","timestamp":1791997740000},"last":"81670.00000"}"#;

    #[test]
    fn parses_the_three_tickers() {
        assert_eq!(
            parse_coinbase(COINBASE_BODY),
            Ok(Quote { cents: 8_167_355, observed_at: 1_791_997_754 })
        );
        assert_eq!(
            parse_bitstamp(BITSTAMP_BODY),
            Ok(Quote { cents: 8_167_012, observed_at: 1_791_997_755 })
        );
        assert_eq!(
            parse_gemini(GEMINI_BODY),
            Ok(Quote { cents: 8_167_000, observed_at: 1_791_997_740 })
        );
    }

    #[test]
    fn missing_or_malformed_fields_are_errors() {
        assert!(parse_coinbase(r#"{"time":"2026-10-14T17:09:14Z"}"#).is_err());
        assert!(parse_coinbase(r#"{"price":"1.00"}"#).is_err());
        assert!(parse_bitstamp(r#"{"last":"1.00","timestamp":"soon"}"#).is_err());
        assert!(parse_bitstamp(r#"{"last":"","timestamp":"1791997755"}"#).is_err());
        assert!(parse_gemini(r#"{"last":"1.00","volume":{}}"#).is_err());
        assert!(parse_gemini(r#"{"last":"1.00"}"#).is_err());
        assert!(parse_gemini("not json").is_err());
        assert!(parse_gemini(r#"{"message":"rate limited"}"#).is_err());
    }

    #[test]
    fn lazy_fetches_once_and_only_when_asked() {
        let mut calls = 0;
        {
            let mut lazy = Lazy::new(|| {
                calls += 1;
                Ok(5)
            });
            assert_eq!(lazy.get(), Ok(5));
            assert_eq!(lazy.get(), Ok(5));
        }
        assert_eq!(calls, 1);

        let mut never = 0;
        {
            let _lazy = Lazy::new(|| {
                never += 1;
                Ok(5)
            });
        }
        assert_eq!(never, 0);
    }

    #[test]
    fn lazy_remembers_a_failure_too() {
        let mut calls = 0;
        {
            let mut lazy = Lazy::new(|| {
                calls += 1;
                Err("down".to_string())
            });
            assert!(lazy.get().is_err());
            assert!(lazy.get().is_err());
        }
        assert_eq!(calls, 1);
    }
}
