//! Server bootstrap: identity discovery at startup, then the HTTP
//! server. Direct port of the Python `main()`.

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use tokio::net::TcpListener;

use crate::config::{Config, log};
use crate::gadget::{Gadget, discover_muse_name};
use crate::http::Shim;
use crate::replies::ReplyWait;

/// Resolve the model aliases at startup: identity discovery first
/// (the discovered name goes first), the configured aliases as the
/// fallback. Refuses to start with no alias at all.
pub async fn startup<G: Gadget>(config: &mut Config, gadget: &G) -> Result<(), String> {
    let discovered = discover_muse_name(gadget, config.discovery_wait, config.discovery_poll).await;
    if let Some(name) = discovered {
        if !config.model_aliases.contains(&name) {
            config.model_aliases.insert(0, name.clone());
        }
        log(&format!("discovered Muse model `{name}`"));
    }
    if config.model_aliases.is_empty() {
        return Err(
            "no Muse model found: identity discovery failed and SHIM_MODEL_ALIASES is not set"
                .to_string(),
        );
    }
    Ok(())
}

/// Bind `addr` and serve the shim on it in the background; returns
/// the bound address. The accept loop runs until the process ends.
pub async fn bind_and_serve<G: Gadget + 'static, R: ReplyWait + 'static>(
    shim: Shim<G, R>,
    addr: SocketAddr,
) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let shim = shim.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let shim = shim.clone();
                    async move { Ok::<_, Infallible>(shim.handle(req).await) }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Ok(bound)
}

/// Serve the shim on `0.0.0.0:SHIM_PORT` until the process is killed.
pub async fn serve<G: Gadget + 'static, R: ReplyWait + 'static>(
    shim: Shim<G, R>,
) -> io::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], shim.config.port));
    bind_and_serve(shim.clone(), addr).await?;
    log(&format!(
        "listening on :{}, socket={}, timeout={}s",
        shim.config.port,
        shim.config.socket_path,
        shim.config.timeout.as_secs()
    ));
    if !shim.config.api_keys.is_empty() {
        log(&format!(
            "api key auth enabled ({} key(s))",
            shim.config.api_keys.len()
        ));
    }
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::gadget::fake::FakeGadget;
    use crate::http::Shim;
    use crate::replies::fake::FakeReplyWait;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn fake_with_identity(name: Option<&str>) -> FakeGadget {
        let mut gadget = FakeGadget::new();
        if let Some(name) = name {
            gadget.raw_replies = std::sync::Mutex::new(
                vec![Ok(json!({"ok": true, "identity": {"name": name}}))].into(),
            );
        } else {
            gadget.raw_replies =
                std::sync::Mutex::new(vec![Ok(json!({"ok": false, "error": "down"}))].into());
        }
        gadget
    }

    #[tokio::test]
    async fn startup_registers_discovered_name_first() {
        let mut config = Config {
            model_aliases: vec!["other".to_string()],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let gadget = fake_with_identity(Some("Tiro"));
        startup(&mut config, &gadget).await.unwrap();
        assert_eq!(config.model_aliases, ["Tiro", "other"]);
    }

    #[tokio::test]
    async fn startup_does_not_duplicate_discovered_name() {
        let mut config = Config {
            model_aliases: vec!["Tiro".to_string()],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let gadget = fake_with_identity(Some("Tiro"));
        startup(&mut config, &gadget).await.unwrap();
        assert_eq!(config.model_aliases, ["Tiro"]);
    }

    #[tokio::test]
    async fn startup_falls_back_to_env_aliases_when_discovery_fails() {
        let mut config = Config {
            model_aliases: vec!["tiro".to_string()],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let gadget = fake_with_identity(None);
        startup(&mut config, &gadget).await.unwrap();
        assert_eq!(config.model_aliases, ["tiro"]);
    }

    #[tokio::test]
    async fn startup_refuses_start_without_aliases() {
        let mut config = Config {
            model_aliases: vec![],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let gadget = fake_with_identity(None);
        let err = startup(&mut config, &gadget).await.unwrap_err();
        assert!(err.contains("SHIM_MODEL_ALIASES"));
    }

    #[tokio::test]
    async fn main_starts_server() {
        // The server binds and answers /healthz on an ephemeral port.
        let config = Config::test_default();
        let gadget = Arc::new(FakeGadget::new());
        let shim = Shim::new(config, gadget, Arc::new(FakeReplyWait::new()));
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let bound = bind_and_serve(shim, addr).await.unwrap();
        let mut stream = TcpStream::connect(bound).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    }
}
