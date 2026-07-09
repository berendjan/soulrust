//! The Connect API edge: serves `soulrust.api.v1` over Connect / gRPC /
//! gRPC-Web (and JSON) for the web frontend and external apps.
//!
//! Unlike the htmx [`web_bridge`](crate::components::web_bridge), which renders
//! HTML, this exposes a typed RPC surface. It runs an axum/hyper server on its
//! own thread (a dedicated `tokio` runtime, like the peer reactor) and reads a
//! lock-free **snapshot** of bus state that its synchronous bus handlers keep
//! up to date — so an RPC handler never blocks on a bus round-trip, it just
//! reads the latest view.
//!
//! `tokio` here is the proto-deps copy (the one axum is built against), aliased
//! to `tokio_api` in BUILD.bazel so it doesn't collide with the peer reactor's
//! `tokio`.
//!
//! This edge also serves the React single-page app: the Vite bundle is embedded
//! at build time ([`crate::web_assets_gen`]) and served from the same port as
//! the Connect API, so in production the browser talks to one origin (no CORS,
//! no dev proxy). The Connect service owns its `POST /soulrust.api.v1.*` paths;
//! everything else is static assets.

use std::sync::{Arc, Mutex};

use rust_messenger::traits;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::routing::get;
use connectrpc::{RequestContext, Response, Router, ServiceRequest, ServiceResult};

use crate::web_assets_gen::WEB_ASSETS;
use soulrust_proto::api::soulrust::api::v1::{GetStatusRequest, GetStatusResponse};
use soulrust_proto::api_connect::soulrust::api::v1::{StatusService, StatusServiceExt};

use crate::config::AppContext;
use crate::messages::{ConfigChanged, EnumValue, HandlerId, SessionEvent, SessionEventKind};

/// Default address for the Connect API. Distinct from the htmx UI's port so the
/// two edges can run side by side during the migration.
const DEFAULT_API_ADDR: &str = "127.0.0.1:5031";

/// The view of app state the API serves, updated by the bus handlers below and
/// read (cloned) by the async RPC handlers.
#[derive(Debug, Clone, Default)]
struct StatusSnapshot {
    logged_in: bool,
    username: String,
    greeting: String,
    own_ip: String,
    /// Not yet wired to a source (peer_net owns the share index); always 0 until
    /// a share-count broadcast exists. Kept so the API response shape is stable.
    shared_files: u32,
}

pub struct ApiServer {
    addr: String,
    snapshot: Arc<Mutex<StatusSnapshot>>,
}

impl ApiServer {
    pub fn new<W: traits::core::Writer>(ctx: &AppContext, _writer: &W) -> Self {
        let snapshot = StatusSnapshot {
            username: ctx.config.server.username.clone(),
            ..StatusSnapshot::default()
        };
        ApiServer {
            addr: DEFAULT_API_ADDR.to_owned(),
            snapshot: Arc::new(Mutex::new(snapshot)),
        }
    }
}

impl traits::core::Handler for ApiServer {
    type Id = HandlerId;
    const ID: HandlerId = HandlerId::ApiServer;

    fn on_start<W: traits::core::Writer>(&mut self, _writer: &W) {
        let addr = self.addr.clone();
        let snapshot = self.snapshot.clone();
        std::thread::Builder::new()
            .name("soulrust-api".into())
            .spawn(move || serve(addr, snapshot))
            .expect("spawning api-server thread");
    }
}

impl traits::core::Handle<SessionEvent> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &SessionEvent, _writer: &W) {
        let mut snap = self.snapshot.lock().unwrap();
        match message.kind {
            EnumValue::Known(SessionEventKind::SessionLoggedIn) => {
                snap.logged_in = true;
                snap.greeting = message.greeting.clone();
                snap.own_ip = message.own_ip.clone();
            }
            EnumValue::Known(SessionEventKind::SessionConnecting)
            | EnumValue::Known(SessionEventKind::SessionLoginFailed)
            | EnumValue::Known(SessionEventKind::SessionDisconnected) => {
                snap.logged_in = false;
            }
            // Search/protocol notes don't affect the status view.
            _ => {}
        }
    }
}

impl traits::core::Handle<ConfigChanged> for ApiServer {
    fn handle<W: traits::core::Writer>(&mut self, message: &ConfigChanged, _writer: &W) {
        // Keep the API's reported username in step with config edits (seeded at
        // construction, it would otherwise report the startup username forever).
        self.snapshot.lock().unwrap().username =
            crate::config::config_from_proto(&message.config).server.username;
    }
}

/// The async service implementation. Holds only the shared snapshot; every
/// handler reads a clone of it, so the service is `Send + Sync + 'static`
/// without any bus `Writer`.
struct StatusApi {
    snapshot: Arc<Mutex<StatusSnapshot>>,
}

impl StatusService for StatusApi {
    // Returning the concrete `Response<GetStatusResponse>` is more specific than
    // the trait's `impl Encodable<…>` return — the intended, documented pattern
    // (see the connectrpc eliza example).
    #[allow(refining_impl_trait)]
    async fn get_status(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetStatusRequest>,
    ) -> ServiceResult<GetStatusResponse> {
        let snap = self.snapshot.lock().unwrap().clone();
        Response::ok(GetStatusResponse {
            logged_in: snap.logged_in,
            username: snap.username,
            greeting: snap.greeting,
            own_ip: snap.own_ip,
            shared_files: snap.shared_files,
            ..Default::default()
        })
    }
}

/// Build the Connect router and serve it over plaintext HTTP via axum, with a
/// permissive CORS layer so browser (Connect-Web / gRPC-Web) clients can call
/// it. Blocks the calling thread on its own tokio runtime.
fn serve(addr: String, snapshot: Arc<Mutex<StatusSnapshot>>) {
    let runtime = match tokio_api::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("api server: cannot start runtime: {err}");
            return;
        }
    };
    runtime.block_on(async move {
        let service = Arc::new(StatusApi { snapshot });
        let router = service.register(Router::new());
        // The SPA is served from explicit GET routes; the Connect service (POST
        // to `/soulrust.api.v1.*`) is the fallback, so the two never collide.
        // CORS stays permissive as a dev convenience (the Vite dev server hits a
        // different origin); in production the SPA is same-origin so it is inert.
        let app = axum::Router::new()
            .route("/", get(serve_index))
            .route("/assets/{*path}", get(serve_asset))
            .fallback_service(router.into_axum_service())
            .layer(tower_http::cors::CorsLayer::permissive());

        let listener = match tokio_api::net::TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("api server: cannot bind {addr}: {err}");
                return;
            }
        };
        println!("soulrust Connect API + web UI listening on http://{addr}");
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("api server: stopped: {err}");
        }
    });
}

/// Serve the SPA entry document (`GET /`).
async fn serve_index() -> HttpResponse {
    asset_response("index.html").unwrap_or_else(|| {
        // Only happens if the frontend bundle wasn't embedded.
        (StatusCode::NOT_FOUND, "web UI not built").into_response()
    })
}

/// Serve a hashed static asset (`GET /assets/<path>`).
async fn serve_asset(Path(path): Path<String>) -> HttpResponse {
    asset_response(&format!("assets/{path}"))
        .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// Look an embedded asset up by its bundle-relative path and wrap it in a
/// response with a guessed content type. `None` if the path isn't embedded.
fn asset_response(path: &str) -> Option<HttpResponse> {
    let bytes = WEB_ASSETS.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)?;
    Some(
        HttpResponse::builder()
            .header(header::CONTENT_TYPE, content_type(path))
            .body(Body::from(bytes.to_vec()))
            .expect("static asset response"),
    )
}

/// Minimal extension → MIME mapping for the asset kinds Vite emits.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
