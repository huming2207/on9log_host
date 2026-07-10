use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use axum::extract::{
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};

pub type LogSender = broadcast::Sender<String>;
pub type ControlSender = mpsc::Sender<ControlCommand>;
pub type ControlReceiver = mpsc::Receiver<ControlCommand>;

pub enum ControlCommand {
    Reset {
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetLines {
        dtr: Option<bool>,
        rts: Option<bool>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

pub struct WebConfig {
    pub bind: SocketAddr,
    pub port: String,
    pub baud: u32,
    pub logs: LogSender,
    pub control_tx: ControlSender,
}

#[derive(Clone)]
struct AppState {
    port: Arc<str>,
    baud: u32,
    started: Instant,
    logs: LogSender,
    control_tx: ControlSender,
    ws_clients: Arc<AtomicUsize>,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    port: String,
    baud: u32,
    uptime_ms: u128,
    websocket_clients: usize,
}

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
    message: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    ok: bool,
    error: String,
}

#[derive(Deserialize)]
struct LineRequest {
    dtr: Option<bool>,
    rts: Option<bool>,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ErrorResponse>)>;

struct EmbeddedAsset {
    path: &'static str,
    mime: &'static str,
    bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded_web.rs"));

pub fn channel() -> (LogSender, ControlSender, ControlReceiver) {
    let (logs, _) = broadcast::channel(1024);
    let (control_tx, control_rx) = mpsc::channel(16);
    (logs, control_tx, control_rx)
}

pub async fn spawn(cfg: WebConfig) -> std::io::Result<SocketAddr> {
    let state = AppState {
        port: Arc::from(cfg.port),
        baud: cfg.baud,
        started: Instant::now(),
        logs: cfg.logs,
        control_tx: cfg.control_tx,
        ws_clients: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/ws/logs", get(logs_ws))
        .route("/api/status", get(status))
        .route("/api/target/reset", post(reset_target))
        .route("/api/serial/lines", post(set_serial_lines))
        .route("/", get(index))
        .fallback(static_asset)
        .with_state(state);

    let listener = bind_with_port_fallback(cfg.bind).await?;
    let local_addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("on9log: web server stopped: {e}");
        }
    });
    Ok(local_addr)
}

async fn index() -> Response {
    embedded_response("index.html").unwrap_or_else(missing_assets_response)
}

async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path.starts_with("ws/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    if path.is_empty() {
        return embedded_response("index.html").unwrap_or_else(missing_assets_response);
    }

    embedded_response(path)
        .or_else(|| {
            if path
                .rsplit('/')
                .next()
                .is_some_and(|segment| segment.contains('.'))
            {
                None
            } else {
                embedded_response("index.html")
            }
        })
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

fn embedded_response(path: &str) -> Option<Response> {
    let asset = WEB_ASSETS.iter().find(|asset| asset.path == path)?;
    let mut response = asset.bytes.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    Some(response)
}

fn missing_assets_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        "on9log web UI is not bundled; build on9log-cli/web first",
    )
        .into_response()
}

async fn bind_with_port_fallback(mut addr: SocketAddr) -> std::io::Result<TcpListener> {
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == ErrorKind::AddrInUse && addr.port() < u16::MAX => {
                addr.set_port(addr.port() + 1);
            }
            Err(e) => return Err(e),
        }
    }
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        ok: true,
        port: state.port.to_string(),
        baud: state.baud,
        uptime_ms: state.started.elapsed().as_millis(),
        websocket_clients: state.ws_clients.load(Ordering::Relaxed),
    })
}

async fn logs_ws(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| stream_logs(socket, state))
}

async fn stream_logs(mut socket: WebSocket, state: AppState) {
    state.ws_clients.fetch_add(1, Ordering::Relaxed);
    let mut rx = state.logs.subscribe();
    loop {
        match rx.recv().await {
            Ok(line) => {
                if socket.send(Message::Text(line.into())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                let msg = format!("--- websocket skipped {skipped} old log message(s) ---");
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    state.ws_clients.fetch_sub(1, Ordering::Relaxed);
}

async fn reset_target(State(state): State<AppState>) -> ApiResult<OkResponse> {
    let (reply, wait) = oneshot::channel();
    state
        .control_tx
        .send(ControlCommand::Reset { reply })
        .await
        .map_err(|_| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "serial task is not running",
            )
        })?;
    await_control(wait).await?;
    Ok(Json(OkResponse {
        ok: true,
        message: "target reset requested".to_string(),
    }))
}

async fn set_serial_lines(
    State(state): State<AppState>,
    Json(req): Json<LineRequest>,
) -> ApiResult<OkResponse> {
    if req.dtr.is_none() && req.rts.is_none() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "request must include dtr or rts",
        ));
    }

    let (reply, wait) = oneshot::channel();
    state
        .control_tx
        .send(ControlCommand::SetLines {
            dtr: req.dtr,
            rts: req.rts,
            reply,
        })
        .await
        .map_err(|_| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "serial task is not running",
            )
        })?;
    await_control(wait).await?;
    Ok(Json(OkResponse {
        ok: true,
        message: "serial control lines updated".to_string(),
    }))
}

async fn await_control(
    wait: oneshot::Receiver<Result<(), String>>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match tokio::time::timeout(Duration::from_secs(3), wait).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, e)),
        Ok(Err(_)) => Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "serial task dropped control response",
        )),
        Err(_) => Err(api_error(
            StatusCode::GATEWAY_TIMEOUT,
            "timed out waiting for serial control",
        )),
    }
}

fn api_error(status: StatusCode, error: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            ok: false,
            error: error.into(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_fallback_skips_in_use_port() {
        let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let held_addr = held.local_addr().unwrap();
        if held_addr.port() == u16::MAX {
            return;
        }

        let fallback = bind_with_port_fallback(held_addr).await.unwrap();
        let fallback_addr = fallback.local_addr().unwrap();

        assert_eq!(fallback_addr.ip(), held_addr.ip());
        assert!(fallback_addr.port() > held_addr.port());
    }
}
