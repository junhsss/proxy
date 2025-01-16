use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::Response,
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use std::sync::OnceLock;

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

use tokio::{
    net::{TcpListener, TcpStream},
    signal,
};
use tower::{Service, ServiceExt};
use tracing_subscriber::EnvFilter;

use hyper::server::conn::http1;

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

#[async_trait::async_trait]
pub trait MetricsService: Send + Sync + 'static + std::fmt::Debug {
    async fn get_metrics(&self) -> Metrics;
    fn update_bandwidth(&self, bytes: u64);
    fn update_site_visit(&self, domain: String);
}

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
    pub fn format_final_metrics(&self) -> String {
        let bandwidth = self.bandwidth.load(Ordering::Relaxed);
        let mut top_sites: Vec<_> = self
            .site_visits
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();

        top_sites.sort_by(|a, b| b.1.cmp(&a.1));

        let mut output = "\nFinal Metrics:\n============\n".to_string();
        output.push_str(&format!(
            "Total Bandwidth Usage: {}\n",
            format_size(bandwidth, BINARY)
        ));
        output.push_str("\nMost Visited Sites:\n");

        for (idx, (site, visits)) in top_sites.iter().take(10).enumerate() {
            output.push_str(&format!("{}. {} - {} visits\n", idx + 1, site, visits));
        }

        output
    }
}

impl Default for DefaultMetricsService {
    fn default() -> Self {
        Self::new()
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
    let state = AppState::new(metrics_service.clone());

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state.clone());

    let tower_service = tower::service_fn(move |req: Request<Incoming>| {
        let app = app.clone();
        let state = state.clone();

        async move {
            let (parts, incoming_body) = req.into_parts();
            let stream = incoming_body.into_data_stream();
            let body = Body::from_stream(stream);
            let req = Request::from_parts(parts, body);

            if req.uri().authority().is_none() {
                return app.oneshot(req).await.map_err(|err| match err {});
            }

            if let Err(auth_error) = authorize(&req) {
                return Ok(auth_error);
            }

            match *req.method() {
                Method::CONNECT => proxy_https(req, state).await,
                _ => proxy_http(req, state).await,
            }
        }
    });

    let hyper_service = hyper::service::service_fn(move |request: Request<Incoming>| {
        tower_service.clone().call(request)
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = TcpListener::bind(addr).await.unwrap();

    tracing::info!("Server started on {}", addr);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
    let shutdown_tx_clone = shutdown_tx.clone();

    tokio::spawn(async move {
        match signal::ctrl_c().await {
            Ok(()) => {
                tracing::info!("Shutdown signal received");
                let _ = shutdown_tx_clone.send(());
            }
            Err(err) => {
                tracing::error!("Failed to listen for shutdown signal: {}", err);
            }
        }
    });

    let server_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            let io = TokioIo::new(stream);
                            let hyper_service = hyper_service.clone();
                            let mut shutdown_rx = shutdown_tx.subscribe();

                            tokio::task::spawn(async move {
                                let server = http1::Builder::new()
                                    .preserve_header_case(true)
                                    .title_case_headers(true)
                                    .serve_connection(io, hyper_service)
                                    .with_upgrades();

                                tokio::select! {
                                    result = server => {
                                        if let Err(err) = result {
                                            tracing::error!("Connection error: {}", err);
                                        }
                                    }
                                    _ = shutdown_rx.recv() => {
                                        tracing::info!("Connection shutdown received");
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to accept connection: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("Server shutdown initiated");
                    break;
                }
            }
        }
    });

    server_task.await?;

    let final_metrics = metrics_service.get_metrics().await;
    println!("\nFinal Server Statistics:");
    println!("Total Bandwidth Usage: {}", final_metrics.bandwidth_usage);
    println!("\nMost Visited Sites:");
    for (index, site) in final_metrics.top_sites.iter().enumerate() {
        println!("{}. {} - {} visits", index + 1, site.url, site.visits);
    }

    Ok(())
}

#[tracing::instrument(skip(req))]
async fn proxy_https(req: Request<Body>, state: AppState) -> Result<Response<Body>, hyper::Error> {
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

    state.metrics_service.update_bandwidth(from_client);
    state.metrics_service.update_bandwidth(from_server);

    tracing::info!(
        "Transfer completed - Upload bytes: {}, Download bytes: {}, Total: {}",
        from_client,
        from_server,
        from_client + from_server
    );
    Ok(())
}

async fn proxy_http(req: Request<Body>, state: AppState) -> Result<Response<Body>, hyper::Error> {
    let uri = match req.uri().to_string() {
        ref uri if uri.starts_with("http://") => uri.to_string(),
        _ => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid proxy request"))
                .unwrap());
        }
    };

    let url = url::Url::parse(&uri).expect("Failed to parse URL");
    let host = url.host_str().expect("Failed to get host").to_string();
    let port = url.port().unwrap_or(80);
    let addr = format!("{}:{}", host, port);

    state.metrics_service.update_site_visit(host.to_string());

    let mut request_size = 0u64;
    request_size += req.method().as_str().len() as u64;
    request_size += req.uri().to_string().len() as u64;
    request_size += "HTTP/1.1\r\n".len() as u64;

    for (name, value) in req.headers() {
        request_size += name.as_str().len() as u64;
        request_size += value.len() as u64;
        request_size += 4; // ": " and "\r\n"
    }
    request_size += 2; // Final "\r\n"

    state.metrics_service.update_bandwidth(request_size);

    let server = TcpStream::connect(&addr)
        .await
        .expect("Failed to connect to server");

    let server = TokioIo::new(server);

    // Set up the connection
    let (mut sender, conn) = hyper::client::conn::http1::handshake(server).await?;

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            tracing::error!("Connection failed: {:?}", err);
        }
    });

    // Track request body
    let (parts, body) = req.into_parts();
    let state_clone = state.clone();
    let tracked_body = Body::from_stream(body.into_data_stream().map(move |chunk| {
        if let Ok(data) = &chunk {
            state_clone
                .metrics_service
                .update_bandwidth(data.len() as u64);
        }
        chunk
    }));
    let tracked_req = Request::from_parts(parts, tracked_body);

    // Send request and get response
    let response = sender.send_request(tracked_req).await?;

    // Track response metadata size
    let mut response_size = 0u64;
    response_size += "HTTP/1.1 ".len() as u64;
    response_size += response.status().as_str().len() as u64;
    response_size += 2; // "\r\n"

    for (name, value) in response.headers() {
        response_size += name.as_str().len() as u64;
        response_size += value.len() as u64;
        response_size += 4; // ": " and "\r\n"
    }
    response_size += 2; // Final "\r\n"

    state.metrics_service.update_bandwidth(response_size);

    // Track response body
    let (parts, body) = response.into_parts();
    let state_clone = state.clone();
    let tracked_body = Body::from_stream(body.into_data_stream().map(move |chunk| {
        if let Ok(data) = &chunk {
            state_clone
                .metrics_service
                .update_bandwidth(data.len() as u64);

            tracing::debug!(
                "Response chunk size: {} bytes, domain: {}",
                data.len(),
                host
            );
        }
        chunk
    }));

    Ok(Response::from_parts(parts, tracked_body))
}

// Authentication

static CREDENTIALS: OnceLock<DashMap<String, String>> = OnceLock::new();

fn get_credentials() -> &'static DashMap<String, String> {
    CREDENTIALS.get_or_init(|| {
        let map = DashMap::new();
        map.insert("admin".to_string(), "secret".to_string());
        map
    })
}

fn unauthorized_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
        .header("Proxy-Authenticate", "Basic realm=\"proxy\"")
        .body(Body::from("Proxy authentication required"))
        .unwrap()
}

fn decode_basic_auth(auth_str: &str) -> Option<(String, String)> {
    let encoded = auth_str.strip_prefix("Basic ")?.trim();

    let decoded = STANDARD.decode(encoded).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;

    let mut parts = credentials.splitn(2, ':');
    let username = parts.next()?.to_string();
    let password = parts.next()?.to_string();

    Some((username, password))
}

fn authorize(req: &Request<Body>) -> Result<(), Response<Body>> {
    let auth_str = req
        .headers()
        .get("Proxy-Authorization")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(unauthorized_response)?;

    let (username, provided_password) =
        decode_basic_auth(auth_str).ok_or_else(unauthorized_response)?;

    match get_credentials().get(&username) {
        Some(stored_password) if stored_password.as_str() == provided_password => Ok(()),
        _ => Err(unauthorized_response()),
    }
}
