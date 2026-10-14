use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use http::{Request, Response, StatusCode, Uri};
use http_body::Body;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, lookup_host};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tracing::{error, info, warn};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::bindings::ProxyPre;
use wasmtime_wasi_http::p2::bindings::http::types::Scheme;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::{
    Error as WasiHttpError, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks,
    WasiHttpView,
};

const DEFAULT_ADDR: &str = "127.0.0.1:8765";
const QUEUE_LIMIT: usize = 8;
const FIELD_SIZE_LIMIT: usize = 64 * 1024;
const COMPONENT_MEMORY_LIMIT: usize = 3 * 1024 * 1024 * 1024;
const COMPONENT_TABLE_LIMIT: usize = 10_000;
const COMPONENT_TABLES_LIMIT: usize = 4;
const COMPONENT_INSTANCE_LIMIT: usize = 8;
const COMPONENT_MEMORIES_LIMIT: usize = 4;
const CPU_EPOCH_TICK: Duration = Duration::from_millis(100);
const CPU_EPOCH_DEADLINE_TICKS: u64 = 3000;
const OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const OUTBOUND_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(300);
const OUTBOUND_BETWEEN_BYTES_TIMEOUT: Duration = Duration::from_secs(300);

const GUEST_ENV_NAMES: &[&str] = &[
    "ONEDRIVE_CLIENT_ID",
    "ONEDRIVE_TENANT_ID",
    "ONEDRIVE_DRIVE_BASE",
    "ONEDRIVE_SCOPES",
    "ONEDRIVE_BASIC_AUTH_SECRET",
    "ONEDRIVE_INDEX_ENABLED",
    "ONEDRIVE_SYNC_MAX_PAGES",
    "ONEDRIVE_SYNC_BUDGET_MS",
];

#[derive(Clone, Debug)]
struct HostConfig {
    component_path: PathBuf,
    state_dir: PathBuf,
    listen_addr: SocketAddr,
}

impl HostConfig {
    fn parse() -> Result<Self> {
        let mut component_path = None;
        let mut state_dir = None;
        let mut listen_addr = DEFAULT_ADDR.parse::<SocketAddr>().unwrap();

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--component" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("missing value for --component"))?;
                    component_path = Some(PathBuf::from(value));
                }
                "--state-dir" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("missing value for --state-dir"))?;
                    state_dir = Some(PathBuf::from(value));
                }
                "--addr" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("missing value for --addr"))?;
                    listen_addr = value
                        .parse()
                        .with_context(|| format!("invalid --addr value: {value}"))?;
                }
                "--help" | "-h" => {
                    println!(
                        "Usage: onedrive-davfs-host --component <path> --state-dir <path> [--addr {DEFAULT_ADDR}]"
                    );
                    std::process::exit(0);
                }
                other => bail!("unrecognized argument: {other}"),
            }
        }

        let component_path =
            component_path.ok_or_else(|| anyhow!("missing required --component"))?;
        let state_dir = state_dir.ok_or_else(|| anyhow!("missing required --state-dir"))?;
        if !component_path.is_file() {
            bail!(
                "component does not exist or is not a file: {}",
                component_path.display()
            );
        }
        if !state_dir.is_dir() {
            bail!(
                "state dir does not exist or is not a directory: {}",
                state_dir.display()
            );
        }

        Ok(Self {
            component_path,
            state_dir,
            listen_addr,
        })
    }
}

#[derive(Clone)]
struct GuestEnv {
    entries: Arc<Vec<(String, String)>>,
}

impl GuestEnv {
    fn capture() -> Self {
        let mut entries = Vec::new();
        for name in GUEST_ENV_NAMES {
            if let Some(value) = std::env::var_os(name) {
                entries.push(((*name).to_string(), value.to_string_lossy().into_owned()));
            }
        }
        Self {
            entries: Arc::new(entries),
        }
    }
}

#[derive(Clone, Debug)]
struct OutboundPolicy;

impl OutboundPolicy {
    fn validate_uri(&self, uri: &Uri) -> Result<ValidatedAuthority, WasiHttpError> {
        let scheme = uri.scheme_str().unwrap_or("https");
        if scheme != "https" {
            return Err(WasiHttpError::HttpRequestDenied);
        }

        let authority = uri
            .authority()
            .ok_or(WasiHttpError::HttpRequestUriInvalid)?;
        let authority_text = authority.as_str();
        if authority_text.contains('@') {
            return Err(WasiHttpError::HttpRequestDenied);
        }

        let host = authority.host().to_ascii_lowercase();
        if host.parse::<IpAddr>().is_ok() {
            return Err(WasiHttpError::DestinationIpProhibited);
        }

        let port = authority.port_u16().unwrap_or(443);
        if port != 443 {
            return Err(WasiHttpError::DestinationIpProhibited);
        }

        if !is_allowed_host(&host) {
            return Err(WasiHttpError::HttpRequestDenied);
        }

        Ok(ValidatedAuthority { host, port })
    }
}

#[derive(Clone, Debug)]
struct ValidatedAuthority {
    host: String,
    port: u16,
}

fn is_allowed_host(host: &str) -> bool {
    host == "graph.microsoft.com"
        || host == "login.microsoftonline.com"
        || has_dns_suffix(host, "sharepoint.com")
        || has_dns_suffix(host, "sharepoint-df.com")
        || has_dns_suffix(host, "microsoftpersonalcontent.com")
        || has_dns_suffix(host, "1drv.com")
}

fn has_dns_suffix(host: &str, suffix: &str) -> bool {
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.octets()[0] == 0
        || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        || (ip.octets()[0] == 198 && ip.octets()[1] == 18)
        || (ip.octets()[0] == 198 && ip.octets()[1] == 19)
        || (ip.octets()[0] == 240))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unspecified()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0)
}

#[derive(Clone, Debug)]
struct HttpPolicyHooks {
    policy: OutboundPolicy,
}

impl HttpPolicyHooks {
    fn new(policy: OutboundPolicy) -> Self {
        Self { policy }
    }
}

impl WasiHttpHooks for HttpPolicyHooks {
    fn is_supported_scheme(&mut self, scheme: &http::uri::Scheme) -> bool {
        *scheme == http::uri::Scheme::HTTPS
    }

    fn send_request(
        &mut self,
        request: Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = std::result::Result<(), WasiHttpError>> + Send>,
    ) -> Box<
        dyn Future<
                Output = std::result::Result<
                    (
                        Response<WasiBody>,
                        Box<dyn Future<Output = std::result::Result<(), WasiHttpError>> + Send>,
                    ),
                    WasiHttpError,
                >,
            > + Send,
    > {
        let policy = self.policy.clone();
        let effective_options = RequestOptions {
            connect_timeout: options
                .and_then(|o| o.connect_timeout)
                .or(Some(OUTBOUND_CONNECT_TIMEOUT)),
            first_byte_timeout: options
                .and_then(|o| o.first_byte_timeout)
                .or(Some(OUTBOUND_FIRST_BYTE_TIMEOUT)),
            between_bytes_timeout: options
                .and_then(|o| o.between_bytes_timeout)
                .or(Some(OUTBOUND_BETWEEN_BYTES_TIMEOUT)),
        };
        _ = fut;
        Box::new(async move {
            let (response, io) =
                send_restricted_request(policy, request, effective_options).await?;
            Ok((
                response,
                Box::new(io)
                    as Box<dyn Future<Output = std::result::Result<(), WasiHttpError>> + Send>,
            ))
        })
    }
}

async fn send_restricted_request(
    policy: OutboundPolicy,
    mut req: Request<WasiBody>,
    options: RequestOptions,
) -> std::result::Result<
    (
        Response<WasiBody>,
        impl Future<Output = std::result::Result<(), WasiHttpError>> + Send,
    ),
    WasiHttpError,
> {
    let validated = policy.validate_uri(req.uri())?;

    let connect_timeout = options.connect_timeout.unwrap_or(OUTBOUND_CONNECT_TIMEOUT);
    let first_byte_timeout = options
        .first_byte_timeout
        .unwrap_or(OUTBOUND_FIRST_BYTE_TIMEOUT);
    let between_bytes_timeout = options
        .between_bytes_timeout
        .unwrap_or(OUTBOUND_BETWEEN_BYTES_TIMEOUT);

    let connect_addr =
        resolve_public_addr(&validated.host, validated.port, connect_timeout).await?;

    let stream = match tokio::time::timeout(connect_timeout, TcpStream::connect(connect_addr)).await
    {
        Ok(stream) => stream.map_err(WasiHttpError::Connect)?,
        Err(..) => return Err(WasiHttpError::ConnectionTimeout),
    };

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        })
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(validated.host.clone())
        .map_err(WasiHttpError::from)?;
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(WasiHttpError::Tls)?;

    let (mut sender, conn) = tokio::time::timeout(
        connect_timeout,
        hyper::client::conn::http1::Builder::new().handshake(TokioIo::new(tls_stream)),
    )
    .await
    .map_err(|_| WasiHttpError::ConnectionTimeout)?
    .map_err(WasiHttpError::from)?;

    *req.uri_mut() = http::Uri::builder()
        .path_and_query(
            req.uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/"),
        )
        .build()
        .map_err(|_| WasiHttpError::HttpRequestUriInvalid)?;

    let send = async move {
        struct IncomingResponseBody {
            incoming: hyper::body::Incoming,
            timeout: tokio::time::Interval,
        }

        impl Body for IncomingResponseBody {
            type Data = Bytes;
            type Error = WasiHttpError;

            fn poll_frame(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<
                Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>,
            > {
                match Pin::new(&mut self.as_mut().incoming).poll_frame(cx) {
                    std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
                    std::task::Poll::Ready(Some(Err(err))) => {
                        let err = if err.is_timeout() {
                            WasiHttpError::HttpResponseTimeout
                        } else {
                            WasiHttpError::from(err)
                        };
                        std::task::Poll::Ready(Some(Err(err)))
                    }
                    std::task::Poll::Ready(Some(Ok(frame))) => {
                        self.timeout.reset();
                        std::task::Poll::Ready(Some(Ok(frame)))
                    }
                    std::task::Poll::Pending => match Pin::new(&mut self.timeout).poll_tick(cx) {
                        std::task::Poll::Ready(_) => {
                            std::task::Poll::Ready(Some(Err(WasiHttpError::ConnectionReadTimeout)))
                        }
                        std::task::Poll::Pending => std::task::Poll::Pending,
                    },
                }
            }

            fn is_end_stream(&self) -> bool {
                self.incoming.is_end_stream()
            }

            fn size_hint(&self) -> http_body::SizeHint {
                self.incoming.size_hint()
            }
        }

        let res = tokio::time::timeout(first_byte_timeout, sender.send_request(req))
            .await
            .map_err(|_| WasiHttpError::ConnectionReadTimeout)?
            .map_err(WasiHttpError::from)?;

        let mut timeout = tokio::time::interval(if between_bytes_timeout == Duration::ZERO {
            Duration::new(0, 1)
        } else {
            between_bytes_timeout
        });
        timeout.reset();

        Ok(res.map(|incoming| {
            let body = IncomingResponseBody { incoming, timeout };
            body.boxed_unsync()
        }))
    };

    let mut send = Box::pin(send);
    let mut conn = Some(conn);
    let response = std::future::poll_fn(|cx| match send.as_mut().poll(cx) {
        std::task::Poll::Ready(Ok(resp)) => std::task::Poll::Ready(Ok(resp)),
        std::task::Poll::Ready(Err(err)) => std::task::Poll::Ready(Err(err)),
        std::task::Poll::Pending => {
            let Some(fut) = conn.as_mut() else {
                return std::task::Poll::Pending;
            };
            match Pin::new(fut).poll(cx) {
                std::task::Poll::Ready(Ok(())) => send.as_mut().poll(cx),
                std::task::Poll::Ready(Err(err)) => {
                    std::task::Poll::Ready(Err(WasiHttpError::from(err)))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    })
    .await?;

    Ok((response, async move {
        if let Some(conn) = conn.take() {
            conn.await.map_err(WasiHttpError::from)?;
        }
        Ok(())
    }))
}

async fn resolve_public_addr(
    host: &str,
    port: u16,
    timeout: Duration,
) -> std::result::Result<SocketAddr, WasiHttpError> {
    let resolved = tokio::time::timeout(timeout, lookup_host((host, port)))
        .await
        .map_err(|_| WasiHttpError::DnsTimeout)?
        .map_err(|_| WasiHttpError::DestinationNotFound)?;

    for addr in resolved {
        if is_public_ip(addr.ip()) {
            return Ok(addr);
        }
    }

    Err(WasiHttpError::DestinationIpProhibited)
}

struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: HttpPolicyHooks,
    limits: StoreLimits,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

struct RequestAdmission {
    permit: OwnedSemaphorePermit,
}

impl RequestAdmission {
    fn into_permit(self) -> OwnedSemaphorePermit {
        self.permit
    }
}

struct HostServer {
    pre: ProxyPre<HostState>,
    engine: Engine,
    config: HostConfig,
    guest_env: GuestEnv,
    execution_slot: Arc<Semaphore>,
    queued_waiters: AtomicUsize,
}

impl HostServer {
    fn new(
        pre: ProxyPre<HostState>,
        engine: Engine,
        config: HostConfig,
        guest_env: GuestEnv,
    ) -> Self {
        Self {
            pre,
            engine,
            config,
            guest_env,
            execution_slot: Arc::new(Semaphore::new(1)),
            queued_waiters: AtomicUsize::new(0),
        }
    }

    async fn admit(&self) -> Result<RequestAdmission> {
        match self.execution_slot.clone().try_acquire_owned() {
            Ok(permit) => Ok(RequestAdmission { permit }),
            Err(_) => {
                let queued = self.queued_waiters.fetch_add(1, Ordering::AcqRel) + 1;
                if queued > QUEUE_LIMIT {
                    self.queued_waiters.fetch_sub(1, Ordering::AcqRel);
                    bail!("request queue full");
                }
                let permit = self
                    .execution_slot
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow!("execution semaphore closed"))?;
                self.queued_waiters.fetch_sub(1, Ordering::AcqRel);
                Ok(RequestAdmission { permit })
            }
        }
    }

    fn build_store(&self) -> Result<Store<HostState>> {
        let mut wasi = WasiCtxBuilder::new();
        wasi.allow_blocking_current_thread(true);
        for (name, value) in self.guest_env.entries.iter() {
            wasi.env(name, value);
        }
        wasi.preopened_dir(&self.config.state_dir, "/state", FsPerms::ReadWrite)
            .map_err(|e| {
                anyhow!(
                    "failed to preopen state dir {}: {e}",
                    self.config.state_dir.display()
                )
            })?;

        let mut http = WasiHttpCtx::new();
        http.set_field_size_limit(FIELD_SIZE_LIMIT);

        let limits = StoreLimitsBuilder::new()
            .memory_size(COMPONENT_MEMORY_LIMIT)
            .table_elements(COMPONENT_TABLE_LIMIT)
            .instances(COMPONENT_INSTANCE_LIMIT)
            .memories(COMPONENT_MEMORIES_LIMIT)
            .tables(COMPONENT_TABLES_LIMIT)
            .trap_on_grow_failure(true)
            .build();

        let state = HostState {
            table: ResourceTable::new(),
            wasi: wasi.build(),
            http,
            hooks: HttpPolicyHooks::new(OutboundPolicy),
            limits,
        };

        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(CPU_EPOCH_DEADLINE_TICKS);
        store.epoch_deadline_trap();
        Ok(store)
    }

    async fn handle_request(
        self: Arc<Self>,
        req: Request<Incoming>,
    ) -> Result<Response<HyperOutgoingBody>> {
        let admission = match self.admit().await {
            Ok(admission) => admission,
            Err(_) => {
                return Ok(plain_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "request queue full",
                ));
            }
        };

        let mut store = self.build_store()?;
        let req = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, req)
            .map_err(|e| anyhow!("failed to map incoming request into wasi:http: {e}"))?;
        let (sender, receiver) = oneshot::channel();
        let out = store
            .data_mut()
            .http()
            .new_response_outparam(sender)
            .map_err(|e| anyhow!("failed to create wasi:http response outparam: {e}"))?;
        let pre = self.pre.clone();
        let _permit = admission.into_permit();

        let task = tokio::spawn(async move {
            let _permit = _permit;
            let proxy = pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| anyhow!("failed to instantiate component: {e}"))?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, req, out)
                .await
                .map_err(|e| anyhow!("component request failed: {e}"))?;
            Result::<()>::Ok(())
        });

        match receiver.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(code)) => {
                warn!("guest returned wasi:http error code: {code:?}");
                Ok(plain_response(
                    StatusCode::BAD_GATEWAY,
                    "component returned wasi:http error",
                ))
            }
            Err(_) => match task.await {
                Ok(Ok(())) => Ok(plain_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "guest never produced an HTTP response",
                )),
                Ok(Err(err)) => {
                    warn!("guest request failed: {err:#}");
                    Ok(plain_response(
                        StatusCode::BAD_GATEWAY,
                        "guest request failed",
                    ))
                }
                Err(err) => {
                    warn!("guest task join failed: {err}");
                    Ok(plain_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "guest task failed",
                    ))
                }
            },
        }
    }
}

fn plain_response(status: StatusCode, message: &str) -> Response<HyperOutgoingBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(CONTENT_LENGTH, message.len().to_string())
        .body(
            Full::new(Bytes::copy_from_slice(message.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("building plain response")
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    Engine::new(&config).map_err(|e| anyhow!("failed to initialize wasmtime engine: {e}"))
}

async fn load_pre(engine: &Engine, component_path: &Path) -> Result<ProxyPre<HostState>> {
    let component = Component::from_file(engine, component_path).map_err(|e| {
        anyhow!(
            "failed to compile component {}: {e}",
            component_path.display()
        )
    })?;
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| anyhow!("failed to add WASI p2 interfaces to linker: {e}"))?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
        .map_err(|e| anyhow!("failed to add WASI HTTP interfaces to linker: {e}"))?;
    let pre = linker
        .instantiate_pre(&component)
        .map_err(|e| anyhow!("failed to pre-instantiate component: {e}"))?;
    ProxyPre::new(pre).map_err(|e| anyhow!("component does not implement wasi:http/proxy: {e}"))
}

async fn epoch_ticker(engine: Engine) {
    let mut interval = tokio::time::interval(CPU_EPOCH_TICK);
    loop {
        interval.tick().await;
        engine.increment_epoch();
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = sigterm.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("failed to install Ctrl-C handler")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .without_time()
        .init();

    let config = HostConfig::parse()?;
    let engine = build_engine()?;
    let pre = load_pre(&engine, &config.component_path).await?;
    let guest_env = GuestEnv::capture();
    let server = Arc::new(HostServer::new(
        pre,
        engine.clone(),
        config.clone(),
        guest_env,
    ));
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;

    info!(
        addr = %config.listen_addr,
        component = %config.component_path.display(),
        state_dir = %config.state_dir.display(),
        queue_limit = QUEUE_LIMIT,
        memory_limit_bytes = COMPONENT_MEMORY_LIMIT,
        "onedrive-davfs host listening"
    );

    let ticker = tokio::spawn(epoch_ticker(engine));
    let mut tasks = Vec::new();

    loop {
        tokio::select! {
            signal = shutdown_signal() => {
                signal?;
                info!("shutdown requested; stopping accept loop");
                break;
            }
            accept = listener.accept() => {
                let (client, addr) = accept.context("failed to accept connection")?;
                let server = server.clone();
                tasks.push(tokio::spawn(async move {
                    if let Err(err) = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(
                            TokioIo::new(client),
                            service_fn(move |req| {
                                let server = server.clone();
                                async move {
                                    match server.handle_request(req).await {
                                        Ok(resp) => Ok::<_, std::convert::Infallible>(resp),
                                        Err(err) => {
                                            error!("request handling failed: {err:#}");
                                            Ok::<_, std::convert::Infallible>(plain_response(
                                                StatusCode::INTERNAL_SERVER_ERROR,
                                                "internal host error",
                                            ))
                                        }
                                    }
                                }
                            }),
                        )
                        .await
                    {
                        warn!("connection {addr} ended with error: {err}");
                    }
                }));
            }
        }
    }

    drop(listener);
    ticker.abort();
    for task in tasks {
        let _ = task.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_policy_allows_known_microsoft_hosts() {
        let policy = OutboundPolicy;
        for uri in [
            "https://graph.microsoft.com/v1.0/me/drive",
            "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            "https://tenant.sharepoint.com/sites/x/_layouts/download.aspx?id=1",
            "https://foo.microsoftpersonalcontent.com/personal/x/_layouts/15/download.aspx",
            "https://upload.up.1drv.com/up/fe6986",
        ] {
            let uri: Uri = uri.parse().unwrap();
            assert!(policy.validate_uri(&uri).is_ok(), "{uri}");
        }
    }

    #[test]
    fn outbound_policy_rejects_non_https_and_bad_authorities() {
        let policy = OutboundPolicy;
        for uri in [
            "http://graph.microsoft.com/v1.0/me/drive",
            "https://127.0.0.1/x",
            "https://graph.microsoft.com:444/x",
            "https://user@graph.microsoft.com/x",
            "https://evil.example/x",
        ] {
            let uri: Uri = uri.parse().unwrap();
            assert!(policy.validate_uri(&uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn public_ip_filter_blocks_private_and_special_ranges() {
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1))));
        assert!(!is_public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_public_ip("fc00::1".parse().unwrap()));
        assert!(!is_public_ip("fe80::1".parse().unwrap()));
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
