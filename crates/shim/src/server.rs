//! Server bootstrap: identity discovery at startup, then the HTTP
//! server. Direct port of the Python `main()`.

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config::{Config, log};
use crate::gadget::{Gadget, discover_muse_name};
use crate::http::Shim;
use crate::model::MuseInfo;
use crate::replies::ReplyWait;

/// Resolve the model aliases at startup: discover each Muse's
/// name from its own socket, in parallel, each with the configured
/// discovery wait/poll. The primary (first socket) keeps the
/// current behavior: discovery failure falls back to
/// `SHIM_MODEL_ALIASES`, and the shim refuses to start only when
/// no alias at all remains. Any additional socket whose Muse
/// cannot be discovered is a startup error naming the socket. Two
/// sockets discovering the same Muse name, or a session-map Muse
/// key matching no discovered Muse, is a startup error too.
/// After discovery, each Muse's alias list is its discovered name
/// first, then `SHIM_MODEL_ALIASES` for the primary only.
pub async fn startup<G: Gadget + 'static>(
    config: &mut Config,
    gadgets: &[Arc<G>],
) -> Result<(), String> {
    let mut handles = Vec::new();
    for gadget in gadgets {
        let gadget = gadget.clone();
        let wait = config.discovery_wait;
        let poll = config.discovery_poll;
        handles.push(tokio::spawn(async move {
            discover_muse_name(&*gadget, wait, poll).await
        }));
    }
    let mut names: Vec<Option<String>> = Vec::new();
    for handle in handles {
        let name = handle
            .await
            .map_err(|e| format!("discovery task failed: {e}"))?;
        names.push(name);
    }
    let mut muses: Vec<MuseInfo> = Vec::new();
    for (index, (socket, name)) in config.gadget_sockets.iter().zip(names.iter()).enumerate() {
        let primary = index == 0;
        let Some(name) = name else {
            if primary {
                if config.model_aliases.is_empty() {
                    continue;
                }
                muses.push(MuseInfo {
                    key: config.model_aliases[0].to_ascii_lowercase(),
                    aliases: config.model_aliases.clone(),
                    socket_path: socket.clone(),
                });
                continue;
            }
            return Err(format!(
                "could not discover the Muse on socket {socket}: \
                 the service is not answering or not paired"
            ));
        };
        let key = name.to_ascii_lowercase();
        let mut aliases = vec![name.clone()];
        if primary {
            for alias in &config.model_aliases {
                if !aliases.iter().any(|a| a.eq_ignore_ascii_case(alias)) {
                    aliases.push(alias.clone());
                }
            }
        }
        muses.push(MuseInfo {
            key,
            aliases,
            socket_path: socket.clone(),
        });
        log(&format!("discovered Muse model `{name}` on {socket}"));
    }
    if muses.is_empty() {
        return Err(
            "no Muse model found: identity discovery failed and SHIM_MODEL_ALIASES is not set"
                .to_string(),
        );
    }
    // No alias may name two Muses: routing would be ambiguous.
    let mut seen: Vec<(String, String)> = Vec::new();
    for muse in &muses {
        for alias in &muse.aliases {
            let lowered = alias.to_ascii_lowercase();
            if let Some((other, _)) = seen.iter().find(|(a, _)| *a == lowered) {
                return Err(format!(
                    "alias {alias:?} names two Muses ({other} and {})",
                    muse.key
                ));
            }
            seen.push((lowered, muse.key.clone()));
        }
    }
    // A session-map Muse key must match a discovered Muse.
    for key in config.session_aliases.keys() {
        if !muses.iter().any(|muse| muse.key == *key) {
            return Err(format!(
                "SHIM_SESSION_ALIASES names Muse {key:?}, which no socket discovered"
            ));
        }
    }
    for key in config.session_prompts.keys() {
        if !muses.iter().any(|muse| muse.key == *key) {
            return Err(format!(
                "SHIM_SESSION_PROMPTS names Muse {key:?}, which no socket discovered"
            ));
        }
    }
    config.muses = muses;
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
        "listening on :{}, sockets={}, timeout={}s",
        shim.config.port,
        shim.config.gadget_sockets.join(","),
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

    async fn start(config: &mut Config, gadgets: Vec<FakeGadget>) -> Result<(), String> {
        let arcs: Vec<Arc<FakeGadget>> = gadgets.into_iter().map(Arc::new).collect();
        startup(config, &arcs).await
    }

    #[tokio::test]
    async fn startup_registers_discovered_name_first() {
        let mut config = Config {
            model_aliases: vec!["other".to_string()],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        start(&mut config, vec![fake_with_identity(Some("Muse"))])
            .await
            .unwrap();
        assert_eq!(config.muses.len(), 1);
        assert_eq!(config.muses[0].key, "muse");
        assert_eq!(config.muses[0].aliases, ["Muse", "other"]);
    }

    #[tokio::test]
    async fn startup_does_not_duplicate_discovered_name() {
        let mut config = Config {
            model_aliases: vec!["Muse".to_string()],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        start(&mut config, vec![fake_with_identity(Some("Muse"))])
            .await
            .unwrap();
        assert_eq!(config.muses[0].aliases, ["Muse"]);
    }

    #[tokio::test]
    async fn startup_falls_back_to_env_aliases_when_discovery_fails() {
        let mut config = Config {
            model_aliases: vec!["muse".to_string()],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        start(&mut config, vec![fake_with_identity(None)])
            .await
            .unwrap();
        assert_eq!(config.muses[0].aliases, ["muse"]);
        assert_eq!(config.muses[0].key, "muse");
    }

    #[tokio::test]
    async fn startup_refuses_start_without_aliases() {
        let mut config = Config {
            model_aliases: vec![],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let err = start(&mut config, vec![fake_with_identity(None)])
            .await
            .unwrap_err();
        assert!(err.contains("SHIM_MODEL_ALIASES"));
    }

    #[tokio::test]
    async fn startup_discovers_two_muses_in_socket_order() {
        let mut config = Config {
            gadget_sockets: vec![
                "/run/musegadget/musegadget.sock".to_string(),
                "/run/other/musegadget.sock".to_string(),
            ],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        start(
            &mut config,
            vec![
                fake_with_identity(Some("Muse")),
                fake_with_identity(Some("Other")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(config.muses.len(), 2);
        assert_eq!(config.muses[0].key, "muse");
        assert_eq!(config.muses[0].aliases, ["Muse"]);
        assert_eq!(config.muses[1].key, "other");
        assert_eq!(config.muses[1].aliases, ["Other"]);
        assert_eq!(config.muses[1].socket_path, "/run/other/musegadget.sock");
    }

    #[tokio::test]
    async fn startup_fails_when_second_socket_undiscovered() {
        let mut config = Config {
            gadget_sockets: vec![
                "/run/musegadget/musegadget.sock".to_string(),
                "/run/other/musegadget.sock".to_string(),
            ],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let err = start(
            &mut config,
            vec![fake_with_identity(Some("Muse")), fake_with_identity(None)],
        )
        .await
        .unwrap_err();
        assert!(err.contains("/run/other/musegadget.sock"), "{err}");
    }

    #[tokio::test]
    async fn startup_fails_on_duplicate_discovered_name() {
        let mut config = Config {
            gadget_sockets: vec![
                "/run/musegadget/musegadget.sock".to_string(),
                "/run/other/musegadget.sock".to_string(),
            ],
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let err = start(
            &mut config,
            vec![
                fake_with_identity(Some("Muse")),
                fake_with_identity(Some("MUSE")),
            ],
        )
        .await
        .unwrap_err();
        assert!(err.contains("two Muses"), "{err}");
    }

    #[tokio::test]
    async fn startup_fails_on_unknown_session_map_muse() {
        let mut config = Config {
            session_aliases: std::collections::HashMap::from([(
                "nope".to_string(),
                std::collections::HashMap::from([(
                    "alice".to_string(),
                    "12345678-1234-1234-1234-1234567890ab".to_string(),
                )]),
            )]),
            discovery_wait: Duration::from_secs(0),
            ..Config::test_default()
        };
        let err = start(&mut config, vec![fake_with_identity(Some("Muse"))])
            .await
            .unwrap_err();
        assert!(err.contains("nope"), "{err}");
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
