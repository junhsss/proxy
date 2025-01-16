use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::Response,
    routing::get,
    Json, Router,
};
use dashmap::DashMap;
use humansize::{format_size, BINARY};
use hyper::{body::Incoming, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use tokio::net::{TcpListener, TcpStream};
use tower::{Service, ServiceExt};
use tracing_subscriber::EnvFilter;

use hyper::server::conn::http1;

// Metrics data structures
#[derive(Serialize)]
pub struct Metrics {
    bandwidth_usage: String,
    top_sites: Vec<SiteMetric>,
}

#[derive(Serialize)]
pub struct SiteMetric {
    url: String,
    visits: u64,
}

// Metrics service trait
#[async_trait::async_trait]
pub trait MetricsService: Send + Sync + 'static + std::fmt::Debug {
    async fn get_metrics(&self) -> Metrics;
    fn update_bandwidth(&self, bytes: u64);
    fn update_site_visit(&self, domain: String);
}

// Concrete implementation of MetricsService
#[derive(Debug)]
pub struct DefaultMetricsService {
    bandwidth: Arc<AtomicU64>,
    site_visits: Arc<DashMap<String, u64>>,
}

impl DefaultMetricsService {
    pub fn new() -> Self {
        Self {
            bandwidth: Arc::new(AtomicU64::new(0)),
            site_visits: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl MetricsService for DefaultMetricsService {
    async fn get_metrics(&self) -> Metrics {
        let bandwidth = self.bandwidth.load(Ordering::Relaxed);
        let mut top_sites: Vec<_> = self
            .site_visits
            .iter()
            .map(|entry| SiteMetric {
                url: entry.key().clone(),
                visits: *entry.value(),
            })
            .collect();

        top_sites.sort_by(|a, b| b.visits.cmp(&a.visits));
        top_sites.truncate(10);

        tracing::info!(
            "Metrics requested - Bandwidth: {} bytes, Top sites: {}",
            bandwidth,
            top_sites.len()
        );

        Metrics {
            bandwidth_usage: format_size(bandwidth, BINARY),
            top_sites,
        }
    }

    fn update_bandwidth(&self, bytes: u64) {
        self.bandwidth.fetch_add(bytes, Ordering::Relaxed);
    }

    fn update_site_visit(&self, domain: String) {
        self.site_visits
            .entry(domain)
            .and_modify(|visits| *visits += 1)
            .or_insert(1);
    }
}

#[derive(Debug, Clone)]
struct AppState {
    metrics_service: Arc<dyn MetricsService>,
}

impl AppState {
    pub fn new(metrics_service: Arc<dyn MetricsService>) -> Self {
        Self { metrics_service }
    }
}

#[axum::debug_handler]
async fn metrics_handler(State(state): State<AppState>) -> Json<Metrics> {
    let metrics = state.metrics_service.get_metrics().await;

    Json(metrics)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let metrics_service = Arc::new(DefaultMetricsService::new());

    let state = AppState::new(metrics_service);

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state.clone());

    let tower_service = tower::service_fn(move |req: Request<_>| {
        let app = app.clone();
        let state = state.clone();
        let req = req.map(Body::new);
        async move {
            match *req.method() {
                Method::CONNECT => proxy(req, state).await,
                _ => app.oneshot(req).await.map_err(|err| match err {}),
            }
        }
    });

    let hyper_service = hyper::service::service_fn(move |request: Request<Incoming>| {
        tower_service.clone().call(request)
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = TcpListener::bind(addr).await.unwrap();

    tracing::info!("Server started on {}", addr);

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let hyper_service = hyper_service.clone();
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, hyper_service)
                .with_upgrades()
                .await
            {
                tracing::error!("Connection error: {}", err);
            }
        });
    }
}

#[tracing::instrument(skip(req))]
async fn proxy(req: Request<Body>, state: AppState) -> Result<Response<Body>, hyper::Error> {
    let host_addr = match req.uri().authority().map(|auth| auth.to_string()) {
        Some(addr) => addr,
        None => {
            tracing::warn!(
                "Invalid CONNECT request - missing host address: {:?}",
                req.uri()
            );
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("CONNECT must be to a socket address"))
                .unwrap());
        }
    };

    let domain = host_addr
        .split(':')
        .next()
        .unwrap_or(&host_addr)
        .to_string();
    state.metrics_service.update_site_visit(domain);

    let state_clone = state.clone();

    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                if let Err(e) = tunnel(upgraded, host_addr, state_clone).await {
                    tracing::error!("Tunnel error: {}", e);
                }
            }
            Err(e) => tracing::error!("Connection upgrade failed: {}", e),
        }
    });

    Ok(Response::new(Body::empty()))
}

#[tracing::instrument(skip(upgraded))]
async fn tunnel(upgraded: Upgraded, addr: String, state: AppState) -> std::io::Result<()> {
    let mut server = TcpStream::connect(&addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    state
        .metrics_service
        .update_bandwidth(from_client + from_server);

    tracing::info!(
        "Transfer completed - Client bytes: {}, Server bytes: {}, Total: {}",
        from_client,
        from_server,
        from_client + from_server
    );
    Ok(())
}
