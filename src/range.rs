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
