use axum::{extract::State, routing::get, Json, Router};
use dashmap::DashMap;
use humansize::{format_size, BINARY};
use serde::Serialize;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    bandwidth: Arc<AtomicU64>,
    site_visits: Arc<DashMap<String, u64>>,
}

#[derive(Serialize)]
struct Metrics {
    bandwidth_usage: String,
    top_sites: Vec<SiteMetric>,
}

#[derive(Serialize)]
struct SiteMetric {
    url: String,
    visits: u64,
}

async fn metrics_handler(State(state): State<AppState>) -> Json<Metrics> {
    let bandwidth = state.bandwidth.load(Ordering::Relaxed);
    let mut top_sites: Vec<_> = state
        .site_visits
        .iter()
        .map(|entry| SiteMetric {
            url: entry.key().clone(),
            visits: *entry.value(),
        })
        .collect();

    top_sites.sort_by(|a, b| b.visits.cmp(&a.visits));
    top_sites.truncate(10);

    info!(
        bandwidth_bytes = bandwidth,
        num_sites = top_sites.len(),
        "Metrics accessed"
    );

    Json(Metrics {
        bandwidth_usage: format_size(bandwidth, BINARY),
        top_sites,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let state = AppState {
        bandwidth: Arc::new(AtomicU64::new(0)),
        site_visits: Arc::new(DashMap::new()),
    };

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = TcpListener::bind(addr).await?;
    info!(address = %addr, "Starting proxy server");

    axum::serve(listener, app).await?;
    Ok(())
}
