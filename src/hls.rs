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
