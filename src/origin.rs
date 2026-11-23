use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use http::{Method, Request, Response, Uri, header};
use http_body_util::Empty;
use hyper::{body::Incoming, client::conn::http1};
use hyper_util::rt::TokioIo;
use rustls::{ClientConfig, pki_types::ServerName};
#[cfg(not(target_os = "android"))]
use rustls_platform_verifier::ConfigVerifierExt;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, lookup_host},
    time::timeout,
};
use tokio_rustls::TlsConnector;

pub const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct FinalTarget(pub String);

pub struct Origin {
    hosts: HashSet<String>,
    allow_private: bool,
    tls: TlsConnector,
}

impl Origin {
    pub fn new(hosts: HashSet<String>, allow_private: bool) -> Result<Self> {
        #[cfg(not(target_os = "android"))]
        let tls = ClientConfig::with_platform_verifier()?;
        #[cfg(target_os = "android")]
        let tls = ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            ))
            .with_no_client_auth();
        Ok(Self {
            hosts,
            allow_private,
            tls: TlsConnector::from(Arc::new(tls)),
        })
    }

    pub fn validate(&self, source: &str) -> Result<Uri> {
        let uri: Uri = source.parse().context("invalid source URI")?;
        ensure!(
            matches!(uri.scheme_str(), Some("http" | "https")),
            "invalid source scheme"
        );
        ensure!(!source.contains(['#', '\\']), "invalid source URI");
        let authority = uri.authority().context("missing source authority")?;
        ensure!(!authority.as_str().contains('@'), "userinfo is forbidden");
        ensure!(
            self.hosts.contains(&host(&uri)?.to_ascii_lowercase()),
            "source host is not allowed"
        );
        Ok(uri)
    }

    pub async fn fetch(
        &self,
        source: &str,
        method: Method,
        range: Option<&str>,
    ) -> Result<Response<Incoming>> {
        let mut source = source.to_owned();
        for hop in 0..=5 {
            let started = Instant::now();
            let uri = self.validate(&source)?;
            let mut response = timeout(TIMEOUT, self.send(&uri, method.clone(), range))
                .await
                .context("origin headers timed out")?
                .context("origin request failed")?;
            let status = response.status().as_u16();
            let elapsed_ms = started.elapsed().as_millis();
            if response.status().is_client_error() || response.status().is_server_error() {
                tracing::warn!(
                    status,
                    hop,
                    range,
                    elapsed_ms,
                    "origin returned an error status"
                );
            } else {
                tracing::debug!(status, hop, range, elapsed_ms, "origin response headers");
            }
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                response
                    .extensions_mut()
                    .insert(FinalTarget(crate::cache::key(&source)));
                return Ok(response);
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .context("redirect without Location")?
                .to_str()?;
            source = if location.starts_with("https://") || location.starts_with("http://") {
                location.to_owned()
            } else {
                url::Url::parse(&source)?.join(location)?.to_string()
            };
        }
        bail!("too many redirects")
    }

    async fn send(
        &self,
        uri: &Uri,
        method: Method,
        range: Option<&str>,
    ) -> Result<Response<Incoming>> {
        let host = host(uri)?;
        let https = uri.scheme_str() == Some("https");
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });

        // Resolve once and connect directly to an approved address.
        let addresses = lookup_host((host, port))
            .await
            .context("resolve origin host")?
            .filter(|address| self.allow_private || public(address.ip()))
            .collect::<Vec<SocketAddr>>();
        ensure!(!addresses.is_empty(), "no permitted origin address");
        let stream = TcpStream::connect(addresses.as_slice())
            .await
            .context("connect to origin")?;
        let target = uri.path_and_query().map_or("/", |value| value.as_str());
        let target = if target.starts_with('?') {
            format!("/{target}")
        } else {
            target.to_owned()
        };
        let mut request = Request::builder()
            .method(method)
            .uri(target)
            .header(
                header::HOST,
                uri.authority().context("missing authority")?.as_str(),
            )
            .header(header::ACCEPT_ENCODING, "identity");
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let request = request.body(Empty::new())?;
        if https {
            let name = ServerName::try_from(host.to_owned())?;
            send(
                self.tls
                    .connect(name, stream)
                    .await
                    .context("origin TLS handshake")?,
                request,
            )
            .await
        } else {
            send(stream, request).await
        }
    }
}

async fn send<T>(stream: T, request: Request<Empty<bytes::Bytes>>) -> Result<Response<Incoming>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(connection);
    Ok(sender.send_request(request).await?)
}

fn host(uri: &Uri) -> Result<&str> {
    Ok(uri
        .host()
        .context("missing host")?
        .trim_start_matches('[')
        .trim_end_matches(']'))
}

fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !matches!(
                (a, b, c),
                (0 | 10 | 127 | 224..=255, _, _)
                    | (100, 64..=127, _)
                    | (169, 254, _)
                    | (172, 16..=31, _)
                    | (192, 0, 0 | 2)
                    | (192, 88, 99)
                    | (192, 168, _)
                    | (198, 18 | 19, _)
                    | (198, 51, 100)
                    | (203, 0, 113)
            )
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public(IpAddr::V4(v4));
            }
            let [first, second, ..] = ip.segments();
            // Global unicast only, excluding special-use allocations and transition ranges.
            first & 0xe000 == 0x2000
                && !(first == 0x2001 && (second < 0x0200 || second == 0x0db8))
                && first != 0x2002
                && first & 0xfff0 != 0x3ff0
        }
    }
}
