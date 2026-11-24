use anyhow::{Context, Result, ensure};
use http::{HeaderMap, header};
use url::Url;

pub(crate) const MAX_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resource {
    Playlist,
    Media,
    Key,
}

impl Resource {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Media => "media",
            Self::Key => "key",
        }
    }

    pub(crate) fn from_path(path: &str) -> (Option<Self>, &str) {
        for resource in [Self::Playlist, Self::Media, Self::Key] {
            if let Some(source) = path
                .strip_prefix("hls/")
                .and_then(|path| path.strip_prefix(resource.name()))
                .and_then(|path| path.strip_prefix('/'))
            {
                return (Some(resource), source);
            }
        }
        (None, path)
    }
}

pub(crate) fn playlist_path(path: &str) -> bool {
    path.as_bytes()
        .last_chunk::<5>()
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".m3u8"))
}

pub(crate) fn playlist_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or("").trim();
            [
                "application/vnd.apple.mpegurl",
                "application/x-mpegurl",
                "audio/mpegurl",
                "audio/x-mpegurl",
            ]
            .iter()
            .any(|known| media_type.eq_ignore_ascii_case(known))
        })
}

pub(crate) fn resolve(base: &Url, reference: &str) -> Result<String> {
    ensure!(
        !reference.is_empty()
            && !reference.contains(['#', '\\', '"'])
            && !reference.contains("{$")
            && !reference.chars().any(char::is_whitespace),
        "invalid HLS resource reference"
    );
    if reference.starts_with("https://") || reference.starts_with("http://") {
        return Ok(reference.to_owned());
    }
    Ok(base.join(reference)?.into())
}

pub(crate) fn rewrite(
    text: &str,
    mut proxy: impl FnMut(Resource, &str) -> Result<String>,
) -> Result<String> {
    let mut lines = text.lines();
    ensure!(
        lines.next() == Some("#EXTM3U"),
        "invalid HLS playlist header"
    );
    let mut output = String::from("#EXTM3U\n");
    let mut end = false;
    let mut master = false;
    let mut variant_pending = false;
    let mut has_segments = false;
    for line in lines {
        ensure!(
            output.len() <= MAX_PLAYLIST_BYTES * 2,
            "rewritten HLS playlist too large"
        );
        let (tag, attributes) = line.split_once(':').unwrap_or((line, ""));
        let resource = match tag {
            "#EXT-X-ENDLIST" => {
                end = true;
                None
            }
            "#EXT-X-STREAM-INF" => {
                ensure!(!variant_pending, "missing HLS variant URI");
                master = true;
                variant_pending = true;
                None
            }
            "#EXT-X-MEDIA" | "#EXT-X-I-FRAME-STREAM-INF" => {
                master = true;
                Some(Resource::Playlist)
            }
            "#EXT-X-MAP" => Some(Resource::Media),
            "#EXT-X-KEY" | "#EXT-X-SESSION-KEY" => Some(Resource::Key),
            "#EXT-X-DEFINE"
            | "#EXT-X-PART"
            | "#EXT-X-PART-INF"
            | "#EXT-X-SERVER-CONTROL"
            | "#EXT-X-PRELOAD-HINT"
            | "#EXT-X-RENDITION-REPORT"
            | "#EXT-X-SKIP"
            | "#EXT-X-CONTENT-STEERING"
            | "#EXT-X-IMAGE-STREAM-INF" => anyhow::bail!("unsupported HLS tag"),
            "#EXT-X-DATERANGE" if attributes.contains("com.apple.hls.interstitial") => {
                anyhow::bail!("unsupported HLS interstitial")
            }
            _ => {
                ensure!(
                    !tag.starts_with("#EXT-X-") || !attributes.contains("URI="),
                    "unsupported HLS resource tag"
                );
                None
            }
        };
        if let Some(resource) = resource {
            output.push_str(tag);
            output.push(':');
            output.push_str(&rewrite_attributes(
                attributes,
                resource,
                tag == "#EXT-X-KEY",
                tag != "#EXT-X-MEDIA",
                &mut proxy,
            )?);
            output.push('\n');
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            output.push_str(line);
        } else {
            let resource = if std::mem::take(&mut variant_pending) {
                Resource::Playlist
            } else {
                has_segments = true;
                Resource::Media
            };
            output.push_str(&proxy(resource, line)?);
        }
        output.push('\n');
    }
    ensure!(
        output.len() <= MAX_PLAYLIST_BYTES * 2,
        "rewritten HLS playlist too large"
    );
    ensure!(!variant_pending, "missing HLS variant URI");
    ensure!(!master || !has_segments, "mixed HLS playlist types");
    ensure!(master || end, "HLS media playlist has no end marker");
    Ok(output)
}

fn rewrite_attributes(
    attributes: &str,
    resource: Resource,
    allow_none: bool,
    require_uri: bool,
    proxy: &mut impl FnMut(Resource, &str) -> Result<String>,
) -> Result<String> {
    let mut quoted = false;
    let mut output = Vec::new();
    let mut has_uri = false;
    let mut method = None;
    for attribute in attributes.split(|character| {
        if character == '"' {
            quoted = !quoted;
        }
        character == ',' && !quoted
    }) {
        let (name, value) = attribute.split_once('=').context("invalid HLS attribute")?;
        let name = name.trim();
        if name.eq_ignore_ascii_case("URI") {
            ensure!(!has_uri, "duplicate HLS URI attribute");
            has_uri = true;
            let uri = value
                .trim()
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .context("invalid HLS URI attribute")?;
            output.push(format!("URI=\"{}\"", proxy(resource, uri)?));
        } else {
            if resource == Resource::Key {
                match name {
                    "METHOD" => {
                        ensure!(method.replace(value).is_none(), "duplicate HLS key method");
                    }
                    "KEYFORMAT" => ensure!(value == "\"identity\"", "unsupported HLS key format"),
                    "KEYFORMATVERSIONS" => ensure!(value == "\"1\"", "unsupported HLS key version"),
                    _ => {}
                }
            }
            output.push(attribute.to_owned());
        }
    }
    ensure!(!quoted, "unterminated HLS attribute");
    if resource == Resource::Key {
        ensure!(
            (method == Some("AES-128") && has_uri)
                || (allow_none && method == Some("NONE") && output.len() == 1),
            "unsupported HLS key"
        );
    } else {
        ensure!(!require_uri || has_uri, "missing HLS URI attribute");
    }
    Ok(output.join(","))
}

pub(crate) fn available_playlist(
    canonical: &str,
    available: impl Fn(Resource, &str) -> bool,
) -> Result<Option<String>> {
    if uses_encryption(canonical) {
        return Ok(None);
    }

    if !is_master(canonical) {
        let mut ready = true;
        let _ = rewrite(canonical, |resource, source| {
            ready &= available(resource, source);
            Ok(source.to_owned())
        })?;
        return Ok(ready.then(|| canonical.to_owned()));
    }

    let mut output = String::from("#EXTM3U\n");
    let mut variants = Vec::new();
    let mut renditions = Vec::new();
    let mut lines = canonical.lines().skip(1);
    while let Some(line) = lines.next() {
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let source = lines.next().context("missing HLS variant URI")?;
            if available(Resource::Playlist, source) {
                variants.push((attributes, source));
            }
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            if attribute(attributes, "URI")
                .is_none_or(|source| available(Resource::Playlist, source))
            {
                renditions.push((line, attributes));
            }
        } else if !line.starts_with("#EXT-X-I-FRAME-STREAM-INF:") {
            output.push_str(line);
            output.push('\n');
        }
    }

    let has_group = |kind, group| {
        renditions.iter().any(|(_, attributes)| {
            attribute(attributes, "TYPE") == Some(kind)
                && attribute(attributes, "GROUP-ID") == Some(group)
        })
    };
    let mut count = 0;
    let mut groups = std::collections::HashSet::new();
    let mut streams = String::new();
    for (attributes, source) in variants {
        if attribute(attributes, "AUDIO").is_some_and(|group| !has_group("AUDIO", group)) {
            continue;
        }
        count += 1;
        let mut kept = Vec::new();
        for item in split_attributes(attributes) {
            let (name, value) = item
                .split_once('=')
                .context("invalid HLS variant attribute")?;
            if matches!(name, "AUDIO" | "SUBTITLES" | "VIDEO" | "CLOSED-CAPTIONS") {
                let group = value.trim_matches('"');
                if has_group(name, group) {
                    groups.insert((name, group));
                } else if name != "CLOSED-CAPTIONS" || value != "NONE" {
                    continue;
                }
            }
            kept.push(item);
        }
        streams.push_str("#EXT-X-STREAM-INF:");
        streams.push_str(&kept.join(","));
        streams.push('\n');
        streams.push_str(source);
        streams.push('\n');
    }
    if count == 0 {
        return Ok(None);
    }
    for (line, attributes) in renditions {
        if let (Some(kind), Some(group)) = (
            attribute(attributes, "TYPE"),
            attribute(attributes, "GROUP-ID"),
        ) && groups.contains(&(kind, group))
        {
            output.push_str(line);
            output.push('\n');
        }
    }
    output.push_str(&streams);
    Ok(Some(output))
}

pub(crate) fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    split_attributes(attributes).find_map(|item| {
        let (key, value) = item.split_once('=')?;
        (key.trim() == name).then(|| value.trim().trim_matches('"'))
    })
}

fn split_attributes(attributes: &str) -> impl Iterator<Item = &str> {
    let mut quoted = false;
    attributes.split(move |character| {
        if character == '"' {
            quoted = !quoted;
        }
        character == ',' && !quoted
    })
}

pub(crate) fn media_references(
    canonical: &str,
    mut write: impl FnMut(&str, Option<std::ops::Range<u64>>) -> Result<(String, bool)>,
) -> Result<String> {
    let mut output = String::new();
    let mut pending = None;
    let mut previous = None;
    for line in canonical.lines() {
        if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            ensure!(
                pending.replace(value).is_none(),
                "duplicate segment byte range"
            );
            continue;
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let source = attribute(attributes, "URI").context("missing initialization URI")?;
            let range = attribute(attributes, "BYTERANGE")
                .map(|value| byte_range(value, None))
                .transpose()?;
            let (uri, sliced) = write(source, range.clone())?;
            output.push_str("#EXT-X-MAP:");
            let mut rewritten = Vec::new();
            for item in split_attributes(attributes) {
                let (name, _) = item
                    .split_once('=')
                    .context("invalid initialization attribute")?;
                let item = match name.trim() {
                    "URI" => format!("URI=\"{uri}\""),
                    "BYTERANGE" if sliced => {
                        let range = range.as_ref().context("missing initialization range")?;
                        format!("BYTERANGE=\"{}@0\"", range.end - range.start)
                    }
                    _ => item.to_owned(),
                };
                rewritten.push(item);
            }
            output.push_str(&rewritten.join(","));
        } else if !line.is_empty() && !line.starts_with('#') {
            let range = pending
                .take()
                .map(|value| {
                    let start = previous
                        .filter(|(source, _)| *source == line)
                        .map(|(_, end)| end);
                    byte_range(value, start)
                })
                .transpose()?;
            let (uri, sliced) = write(line, range.clone())?;
            previous = range.as_ref().map(|span| (line, span.end));
            if let Some(range) = range {
                let offset = if sliced { 0 } else { range.start };
                output.push_str(&format!(
                    "#EXT-X-BYTERANGE:{}@{offset}\n",
                    range.end - range.start
                ));
            }
            output.push_str(&uri);
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    ensure!(pending.is_none(), "missing byte-range segment");
    Ok(output)
}

fn byte_range(value: &str, inherited: Option<u64>) -> Result<std::ops::Range<u64>> {
    let (length, offset) = value
        .split_once('@')
        .map_or((value, None), |(len, off)| (len, Some(off)));
    let length: u64 = length.parse()?;
    ensure!(length > 0, "empty HLS byte range");
    let start: u64 = if let Some(offset) = offset {
        offset.parse()?
    } else {
        inherited.context("missing HLS byte range offset")?
    };
    Ok(start
        ..start
            .checked_add(length)
            .context("HLS byte range overflow")?)
}

pub(crate) fn is_master(canonical: &str) -> bool {
    canonical.lines().any(|line| {
        line.starts_with("#EXT-X-STREAM-INF:")
            || line.starts_with("#EXT-X-MEDIA:")
            || line.starts_with("#EXT-X-I-FRAME-STREAM-INF:")
    })
}

pub(crate) fn uses_encryption(canonical: &str) -> bool {
    canonical.lines().any(|line| {
        line.starts_with("#EXT-X-SESSION-KEY:")
            || line.starts_with("#EXT-X-KEY:") && line != "#EXT-X-KEY:METHOD=NONE"
    })
}

pub(crate) struct Variant<'a> {
    pub(crate) source: &'a str,
    pub(crate) attributes: &'a str,
    pub(crate) bandwidth: u64,
}

pub(crate) fn variants(canonical: &str) -> Vec<Variant<'_>> {
    let mut variants = Vec::new();
    let mut lines = canonical.lines();
    while let Some(line) = lines.next() {
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:")
            && let Some(source) = lines.next()
            && let Some(bandwidth) =
                attribute(attributes, "BANDWIDTH").and_then(|value| value.parse().ok())
        {
            variants.push(Variant {
                source,
                attributes,
                bandwidth,
            });
        }
    }
    variants
}

pub(crate) fn default_audio<'a>(canonical: &'a str, variant: &str) -> Option<&'a str> {
    let group = attribute(variant, "AUDIO")?;
    canonical
        .lines()
        .filter_map(|line| line.strip_prefix("#EXT-X-MEDIA:"))
        .filter(|attributes| {
            attribute(attributes, "TYPE") == Some("AUDIO")
                && attribute(attributes, "GROUP-ID") == Some(group)
        })
        .map(|attributes| {
            (
                attribute(attributes, "DEFAULT") == Some("YES"),
                attribute(attributes, "URI"),
            )
        })
        .max_by_key(|(default, _)| *default)
        .and_then(|(_, source)| source)
}
