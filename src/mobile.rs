use std::{
    collections::HashSet,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use ring::hmac;
use tokio::{net::TcpListener, runtime::Runtime, task::JoinHandle};

use crate::{cache::Cache, origin::Origin, playback::Playback};

static INSTANCE: Mutex<Option<Instance>> = Mutex::new(None);

struct Instance {
    _runtime: Runtime,
    server: JoinHandle<std::io::Result<()>>,
    playback: Arc<Playback>,
    port: u16,
}

fn start(dir: PathBuf, capacity: u64, hosts: &str) -> Result<()> {
    let mut instance = INSTANCE.lock().unwrap();
    ensure!(instance.is_none(), "proxy is already started");
    let hosts: HashSet<_> = hosts
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    ensure!(!hosts.is_empty(), "source hosts are required");
    ensure!(dir.is_absolute(), "cache path must be absolute");
    let origin = Origin::new(hosts, false)?;
    let mut playback = Playback::new(Cache::open_recoverable(dir, capacity), origin);
    playback.signing_key = Some(
        hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("could not generate playback signing key"))?,
    );
    let playback = Arc::new(playback);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = runtime.block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))?;
    let port = listener.local_addr()?.port();
    let server = runtime.spawn(crate::serve(listener, playback.clone()));
    *instance = Some(Instance {
        _runtime: runtime,
        server,
        playback,
        port,
    });
    Ok(())
}

fn playback_url(source: &str) -> Result<String> {
    let instance = INSTANCE.lock().unwrap();
    let instance = instance.as_ref().context("proxy is closed")?;
    ensure!(!instance.server.is_finished(), "proxy stopped unexpectedly");
    instance.playback.origin.validate(source)?;
    let key = instance.playback.signing_key.as_ref().unwrap();
    Ok(format!(
        "http://127.0.0.1:{}/v1/{}",
        instance.port,
        sign(key, source)
    ))
}

pub(crate) fn sign(key: &hmac::Key, source: &str) -> String {
    let tag = hmac::sign(key, source.as_bytes());
    let signature = tag
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            use std::fmt::Write;
            write!(text, "{byte:02x}").unwrap();
            text
        });
    format!("{signature}/{source}")
}

pub(crate) fn verify<'a>(key: &hmac::Key, path: &'a str) -> Option<&'a str> {
    let (signature, source) = path.split_once('/')?;
    if signature.len() != 64 || !signature.is_ascii() {
        return None;
    }
    let mut tag = [0; 32];
    for (index, byte) in tag.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&signature[index * 2..index * 2 + 2], 16).ok()?;
    }
    hmac::verify(key, source.as_bytes(), &tag).ok()?;
    Some(source)
}

fn close() {
    // Keep startup serialized until the previous runtime has released its cache.
    let mut instance = INSTANCE.lock().unwrap();
    drop(instance.take());
}

mod ffi {
    use super::*;
    use std::{
        ffi::{CStr, CString, c_char},
        ptr,
    };

    unsafe fn input<'a>(value: *const c_char) -> Result<&'a str> {
        ensure!(!value.is_null(), "missing string");
        Ok(unsafe { CStr::from_ptr(value) }.to_str()?)
    }

    fn output(value: String) -> *mut c_char {
        CString::new(value)
            .expect("output contains no NUL")
            .into_raw()
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn rpc_start(
        directory: *const c_char,
        capacity: u64,
        hosts: *const c_char,
    ) -> *mut c_char {
        let result = (|| {
            start(unsafe { input(directory)? }.into(), capacity, unsafe {
                input(hosts)?
            })
        })();
        match result {
            Ok(()) => ptr::null_mut(),
            Err(error) => output(format!("{error:#}").replace('\0', "\\0")),
        }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn rpc_playback_url(source: *const c_char) -> *mut c_char {
        unsafe { input(source) }
            .and_then(playback_url)
            .map(output)
            .unwrap_or(ptr::null_mut())
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn rpc_download_start(source: *const c_char, bandwidth: u64) -> *mut c_char {
        let result = (|| -> Result<String> {
            let source = unsafe { input(source)? };
            let instance = INSTANCE.lock().unwrap();
            let instance = instance.as_ref().context("proxy is closed")?;
            ensure!(!instance.server.is_finished(), "proxy stopped unexpectedly");
            let _context = instance._runtime.enter();
            let (status, progress) = instance
                .playback
                .start_download(source, (bandwidth != 0).then_some(bandwidth))?;
            let mut response = serde_json::to_value(progress)?;
            response["conflict"] = (status == http::StatusCode::CONFLICT).into();
            Ok(response.to_string())
        })();
        result.map(output).unwrap_or(ptr::null_mut())
    }

    fn download_progress(source: &str, cancel: bool) -> Option<String> {
        let instance = INSTANCE.lock().unwrap();
        let instance = instance.as_ref()?;
        let progress = instance.playback.download_progress(source, cancel)?;
        serde_json::to_string(&progress).ok()
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn rpc_download_progress(source: *const c_char) -> *mut c_char {
        unsafe { input(source) }
            .ok()
            .and_then(|source| download_progress(source, false))
            .map(output)
            .unwrap_or(ptr::null_mut())
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn rpc_download_cancel(source: *const c_char) -> *mut c_char {
        unsafe { input(source) }
            .ok()
            .and_then(|source| download_progress(source, true))
            .map(output)
            .unwrap_or(ptr::null_mut())
    }

    #[unsafe(no_mangle)]
    extern "C" fn rpc_close() {
        close();
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn rpc_free_string(value: *mut c_char) {
        if !value.is_null() {
            drop(unsafe { CString::from_raw(value) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_signature_covers_the_exact_source() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[1; 32]);
        let other_key = hmac::Key::new(hmac::HMAC_SHA256, &[2; 32]);
        let source = "https://example.com//movie.mp4?token=a%2Fb&expires=123";
        let signature: String = hmac::sign(&key, source.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = format!("{signature}/{source}");
        assert_eq!(verify(&key, &path), Some(source));
        assert_eq!(verify(&other_key, &path), None);
        assert_eq!(verify(&key, &path.replace("%2F", "%2f")), None);
        assert_eq!(verify(&key, source), None);
    }
}
