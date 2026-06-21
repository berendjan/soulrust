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

use std::sync::{Arc, Mutex};

use rust_messenger::traits;

use connectrpc::{RequestContext, Response, Router, ServiceRequest, ServiceResult};
use soulrust_proto::api::soulrust::api::v1::{GetStatusRequest, GetStatusResponse};
use soulrust_proto::api_connect::soulrust::api::v1::{StatusService, StatusServiceExt};

use crate::config::AppContext;
use crate::messages::{EnumValue, HandlerId, SessionEvent, SessionEventKind};

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
        let app = axum::Router::new()
            .fallback_service(router.into_axum_service())
            .layer(tower_http::cors::CorsLayer::permissive());

        let listener = match tokio_api::net::TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("api server: cannot bind {addr}: {err}");
                return;
            }
        };
        println!("soulrust Connect API listening on http://{addr}");
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("api server: stopped: {err}");
        }
    });
}
