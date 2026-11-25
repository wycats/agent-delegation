mod discovery;
mod registry;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use include_dir::{Dir, include_dir};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::watch};

static ASSETS: Dir = include_dir!("$CARGO_MANIFEST_DIR/ui/build");
const IDLE: Duration = Duration::from_secs(15 * 60);

struct Session {
    path: PathBuf,
    host: String,
    origin: String,
    authorization: String,
    activity: watch::Sender<bool>,
}

type Shared = Arc<Session>;

pub(super) async fn run(
    path: PathBuf,
    open_browser: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let token = uuid::Uuid::new_v4();
    let url = format!("{origin}/#{token}");
    let (activity, mut changes) = watch::channel(false);
    let session = Arc::new(Session {
        path: std::path::absolute(path)?,
        host,
        origin,
        authorization: format!("Bearer {token}"),
        activity,
    });
    let app = Router::new()
        .route("/api/targets", get(list).post(save))
        .route("/api/discover", post(discover))
        .route("/api/check", post(check))
        .route("/api/close", post(close))
        .fallback(get(asset))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(from_fn_with_state(session.clone(), guard))
        .with_state(session);
    println!("{url}");
    std::io::stdout().flush()?;
    if open_browser && webbrowser::open(&url).is_err() {
        eprintln!("Open the URL above in your browser.");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // An authenticated request restarts the idle timer. Close is sticky.
            while let Ok(Ok(())) = tokio::time::timeout(IDLE, changes.changed()).await {
                if *changes.borrow_and_update() {
                    break;
                }
            }
        })
        .await?;
    Ok(())
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn authorized(headers: &HeaderMap, session: &Session, method: &Method, api: bool) -> bool {
    if headers.get(header::HOST).map(|v| v.as_bytes()) != Some(session.host.as_bytes()) {
        return false;
    }
    if !api {
        return true;
    }
    let origin = headers.get(header::ORIGIN);
    if origin.is_some_and(|v| v.as_bytes() != session.origin.as_bytes())
        || (method == Method::POST && origin.is_none())
    {
        return false;
    }
    headers.get(header::AUTHORIZATION).map(|v| v.as_bytes())
        == Some(session.authorization.as_bytes())
}

async fn guard(State(session): State<Shared>, request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = if authorized(request.headers(), &session, request.method(), api) {
        if api {
            session.activity.send_if_modified(|closed| !*closed);
        }
        next.run(request).await
    } else {
        error(
            StatusCode::FORBIDDEN,
            "Open settings from the URL printed by the CLI.",
        )
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    response
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some(file) = ASSETS.get_file(path) else {
        return error(StatusCode::NOT_FOUND, "Unknown settings resource.");
    };
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, mime)], file.contents()).into_response()
}

fn result(value: Result<Value, String>) -> Response {
    match value {
        Ok(value) => Json(value).into_response(),
        Err(msg) if msg.starts_with("conflict:") => error(StatusCode::CONFLICT, msg),
        Err(msg) => error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn list(State(session): State<Shared>) -> Response {
    match tokio::task::spawn_blocking(move || registry::list(&session.path)).await {
        Ok(value) => result(value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read settings.",
        ),
    }
}

async fn save(State(session): State<Shared>, Json(update): Json<Value>) -> Response {
    match tokio::task::spawn_blocking(move || registry::save(&session.path, update)).await {
        Ok(value) => result(value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save settings.",
        ),
    }
}

async fn discover(Json(options): Json<Value>) -> Response {
    result(discovery::discover(options).await)
}

#[derive(Deserialize)]
struct Check {
    name: String,
}

async fn check(State(session): State<Shared>, Json(Check { name }): Json<Check>) -> Response {
    result(discovery::check(&session.path, &name).await)
}

async fn close(State(session): State<Shared>) -> Json<Value> {
    session.activity.send_replace(true);
    Json(json!({}))
}
