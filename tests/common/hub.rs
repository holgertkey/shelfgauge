//! A hub on a loopback port and libraries that use it, for the hub client tests.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::UserData;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::hub::config::HubConfig;
use shelfgauge::hub::{Hub, HubState};
use shelfgauge::hub_client::store as hub_store;
use shelfgauge::library::Library;
use shelfgauge::profiles::ProfileId;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

pub const HOST: &str = "127.0.0.1:8742";
pub const ORIGIN: &str = "http://127.0.0.1:8742";

/// A hub served on a loopback port (invite-only, as by default).
pub struct TestHub {
    pub _dir: tempfile::TempDir,
    pub state: HubState,
    /// The same hub for direct API calls (what a phone would do).
    pub app: Router,
    pub url: String,
}

impl TestHub {
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut config = HubConfig::default();
        config.server.port = port;
        let state = HubState::new(Hub::with_config(dir.path(), config).unwrap());
        tokio::spawn(shelfgauge::hub::serve(
            listener,
            state.clone(),
            std::future::pending(),
        ));
        let app = shelfgauge::hub::router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
        Self {
            _dir: dir,
            state,
            app,
            url: format!("http://127.0.0.1:{port}"),
        }
    }

    pub fn invite(&self) -> String {
        let (invite, _) = self
            .state
            .hub()
            .invite(shelfgauge::annotations::now())
            .unwrap();
        invite.as_str().to_owned()
    }

    /// A direct API call: status and JSON body.
    pub async fn api(
        &self,
        method: Method,
        route: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/{route}"))
            .header(header::HOST, self.url.trim_start_matches("http://"));
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    pub fn device_count(&self) -> usize {
        self.state.hub().accounts().unwrap()[0].usage.devices as usize
    }
}

/// A scanned library whose `[hub] url` points at `hub_url`.
pub struct Lib {
    pub _dir: tempfile::TempDir,
    pub userdata: Arc<UserData>,
    pub router: Router,
    pub scan: Arc<BackgroundScan>,
    /// The session token sent with requests ([`Lib::act_as`]).
    pub session: std::sync::Mutex<Option<String>>,
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.scan.shutdown();
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }
}

impl Lib {
    pub fn new(hub_url: &str, name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("book.txt"), super::txt_gutenberg()).unwrap();
        let mut conn = open_writer(&library.db_path()).unwrap();
        Scanner {
            library: &library,
            exclude: &[],
            registry: &Registry::with_builtin_formats(),
            reread: false,
            limits: ReadLimits::default(),
            threads: 1,
            progress: &ScanProgress::default(),
            cancel: &AtomicBool::new(false),
        }
        .run(&mut conn)
        .unwrap();
        let userdata = Arc::new(UserData::open(&library.userdata_path()).unwrap());
        let exporter = Exporter::new(library.annotations_dir(), "http://127.0.0.1:8742/");
        let mut config = Config::default();
        config.hub.url = hub_url.to_owned();
        config.hub.instance_name = name.to_owned();
        config.validate().unwrap();
        let pool = open_read_pool(&library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
        let state = AppState::new(library.clone(), &config, pool, Arc::clone(&scan))
            .with_annotations(Arc::clone(&userdata), exporter);
        Self {
            _dir: dir,
            userdata,
            router: super::local(web::router(state)),
            scan,
            session: std::sync::Mutex::new(None),
        }
    }

    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    /// Sends the next requests with this session token (`None`: without one).
    pub fn act_as(&self, token: Option<&str>) {
        *self.session.lock().unwrap() = token.map(str::to_owned);
    }

    fn with_session(
        &self,
        mut request: axum::http::request::Builder,
    ) -> axum::http::request::Builder {
        if let Some(token) = &*self.session.lock().unwrap() {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        request
    }

    pub async fn get(&self, uri: &str) -> Reply {
        self.send(
            self.with_session(Request::get(uri).header(header::HOST, HOST))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    pub async fn post(&self, uri: &str, form: &[(&str, &str)]) -> Reply {
        let body = form
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        self.send(
            self.with_session(
                Request::post(uri)
                    .header(header::HOST, HOST)
                    .header(header::ORIGIN, ORIGIN)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            )
            .body(Body::from(body))
            .unwrap(),
        )
        .await
    }

    pub fn account(&self) -> Option<hub_store::HubAccount> {
        self.account_of(ProfileId::OWNER)
    }

    pub fn account_of(&self, profile: ProfileId) -> Option<hub_store::HubAccount> {
        hub_store::account(&self.userdata.lock(), profile).unwrap()
    }

    /// Registers the owner profile with `invite` and returns its recovery code text.
    pub async fn register(&self, invite: &str) -> String {
        let reply = self
            .post(
                "/profile/hub",
                &[("action", "register"), ("invite", invite)],
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        assert_eq!(reply.location(), "/profile/hub/recovery");
        let page = self.get("/profile/hub/recovery").await;
        assert_eq!(page.status, StatusCode::OK);
        recovery_code(&page.body)
    }
}

pub fn urlencode(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// The recovery code on its page.
pub fn recovery_code(body: &str) -> String {
    let start = body.find("SG1-").expect("the page shows the code");
    body[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

/// The pairing link on its page: `(secret, account key text)`.
pub fn pairing_link(body: &str, hub_url: &str) -> (String, String) {
    let prefix = format!("{hub_url}/pair#");
    let start = body.find(&prefix).expect("the page shows the link") + prefix.len();
    let fragment: String = body[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let (secret, key) = fragment.split_once('.').unwrap();
    (secret.to_owned(), key.to_owned())
}
