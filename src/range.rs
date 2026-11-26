use std::ops::Range;

#[derive(Clone, Copy, Debug)]
pub enum Requested {
    Whole,
    From(u64, Option<u64>),
    Suffix(u64),
}

impl Requested {
    pub fn parse(value: &str) -> Option<Self> {
        let (unit, spec) = value.split_once('=')?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return None;
        }
        let (start, end) = spec.split_once('-')?;
        if start.is_empty() {
            return Some(Self::Suffix(number(end)?));
        }
        let start = number(start)?;
        let end = if end.is_empty() {
            None
        } else {
            Some(number(end)?)
        };
        if end.is_some_and(|end| end < start) {
            return None;
        }
        Some(Self::From(start, end))
    }

    pub fn resolve(self, size: u64) -> Option<Range<u64>> {
        match self {
            Self::Whole => Some(0..size),
            Self::From(start, end) if start < size => {
                Some(start..end.map_or(size, |end| end.saturating_add(1).min(size)))
            }
            Self::Suffix(count) if count > 0 && size > 0 => Some(size.saturating_sub(count)..size),
            _ => None,
        }
    }

    pub fn upstream(self) -> String {
        match self {
            Self::Whole => "bytes=0-".into(),
            Self::From(start, Some(end)) => format!("bytes={start}-{end}"),
            Self::From(start, None) => format!("bytes={start}-"),
            Self::Suffix(count) => format!("bytes=-{count}"),
        }
    }
}

pub fn upstream(range: &Range<u64>) -> String {
    format!("bytes={}-{}", range.start, range.end - 1)
}

pub fn content_range(value: &str) -> Option<(Range<u64>, u64)> {
    let (unit, value) = value.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (span, size) = value.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let size = number(size)?;
    let start = number(start)?;
    let end = number(end)?;
    (start <= end && end < size).then(|| (start..end + 1, size))
}

fn number(value: &str) -> Option<u64> {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ranges_resolve_to_half_open_spans() {
        for (header, size, expected) in [
            ("bytes=0-0", 10, Some(0..1)),
            ("BYTES=6-", 10, Some(6..10)),
            ("bytes=8-99", 10, Some(8..10)),
            ("bytes=-3", 10, Some(7..10)),
            ("bytes=-30", 10, Some(0..10)),
            ("bytes=-0", 10, None),
            ("bytes=10-", 10, None),
            ("bytes=-1", 0, None),
            ("bytes=0-18446744073709551615", u64::MAX, Some(0..u64::MAX)),
        ] {
            assert_eq!(
                Requested::parse(header).unwrap().resolve(size),
                expected,
                "{header}"
            );
        }
    }

    #[test]
    fn rejects_malformed_or_multiple_request_ranges() {
        for header in [
            "items=0-1",
            "bytes=-",
            "bytes=5-4",
            "bytes=0-1,3-4",
            "bytes=+1-2",
            "bytes= 1-2",
            "bytes=18446744073709551616-",
        ] {
            assert!(Requested::parse(header).is_none(), "{header}");
        }
    }

    #[test]
    fn content_range_requires_a_known_size_greater_than_its_last_byte() {
        assert_eq!(content_range("bytes 4-7/8"), Some((4..8, 8)));
        assert_eq!(
            content_range("bytes 0-18446744073709551614/18446744073709551615"),
            Some((0..u64::MAX, u64::MAX))
        );
        for header in [
            "bytes 0-8/8",
            "bytes 2-1/8",
            "bytes 0-1/*",
            "bytes */8",
            "bytes 0-18446744073709551615/18446744073709551615",
        ] {
            assert!(content_range(header).is_none(), "{header}");
        }
    }
}
