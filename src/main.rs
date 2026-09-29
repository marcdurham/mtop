mod api;
mod metrics;

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use api::AppState;
use metrics::{Collector, History, HistoryPoint};

struct Config {
    bind: SocketAddr,
    token: Option<String>,
    interval_secs: u64,
    history_secs: u64,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let bind = env_or("MTOP_BIND", "0.0.0.0:8787")
            .parse()
            .map_err(|e| format!("invalid MTOP_BIND: {e}"))?;
        let interval_secs: u64 = env_or("MTOP_INTERVAL_SECS", "5")
            .parse()
            .map_err(|e| format!("invalid MTOP_INTERVAL_SECS: {e}"))?;
        let history_secs: u64 = env_or("MTOP_HISTORY_SECS", "3600")
            .parse()
            .map_err(|e| format!("invalid MTOP_HISTORY_SECS: {e}"))?;
        if interval_secs == 0 {
            return Err("MTOP_INTERVAL_SECS must be > 0".into());
        }

        let token = std::env::var("MTOP_TOKEN").ok().filter(|t| !t.is_empty());
        let no_auth = matches!(
            std::env::var("MTOP_NO_AUTH").as_deref(),
            Ok("1" | "true" | "yes")
        );
        if token.is_none() && !no_auth {
            return Err("MTOP_TOKEN is not set (set MTOP_NO_AUTH=1 to run without auth)".into());
        }

        Ok(Self {
            bind,
            token,
            interval_secs,
            history_secs,
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(2);
        }
    };

    let mut collector = Collector::new();
    let first = collector.sample();
    let capacity = (config.history_secs / config.interval_secs).max(1) as usize;
    let mut history = History::new(capacity);
    history.push(HistoryPoint::from(&first));

    let state = Arc::new(AppState {
        latest: RwLock::new(Arc::new(first)),
        history: RwLock::new(history),
        token: config.token.clone(),
        interval_secs: config.interval_secs,
    });

    spawn_sampler(collector, state.clone(), Duration::from_secs(config.interval_secs));

    let listener = match tokio::net::TcpListener::bind(config.bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind {}: {e}", config.bind);
            std::process::exit(1);
        }
    };
    tracing::info!(
        "mtop v{} listening on {} (auth: {}, interval: {}s, history: {} samples)",
        env!("CARGO_PKG_VERSION"),
        config.bind,
        if config.token.is_some() { "bearer token" } else { "DISABLED" },
        config.interval_secs,
        capacity,
    );

    if let Err(e) = axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server error: {e}");
        std::process::exit(1);
    }
    tracing::info!("shut down");
}

/// sysinfo calls are blocking, so sampling runs on a dedicated OS thread.
fn spawn_sampler(mut collector: Collector, state: Arc<AppState>, interval: Duration) {
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                let snapshot = collector.sample();
                let point = HistoryPoint::from(&snapshot);
                *state.latest.write().expect("latest lock poisoned") = Arc::new(snapshot);
                state.history.write().expect("history lock poisoned").push(point);
            }
        })
        .expect("failed to spawn sampler thread");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
