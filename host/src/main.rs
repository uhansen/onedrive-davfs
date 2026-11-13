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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
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

mod credentials;

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
const OUTBOUND_IDLE_LIMIT: usize = 8;
const OUTBOUND_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const OUTBOUND_CONNECTION_LIFETIME: Duration = Duration::from_secs(60);

struct PerfTimer {
    phase: &'static str,
    class: &'static str,
    start: Option<Instant>,
}

impl PerfTimer {
    fn new(phase: &'static str, class: &'static str) -> Self {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled = *ENABLED
            .get_or_init(|| std::env::var("ONEDRIVE_PERF_LOG").is_ok_and(|value| value == "1"));
        Self {
            phase,
            class,
            start: enabled.then(Instant::now),
        }
    }
}

impl Drop for PerfTimer {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            info!(
                phase = self.phase,
                class = self.class,
                elapsed_us = start.elapsed().as_micros() as u64,
                "onedrive performance"
            );
        }
    }
}

type ConnectionIo = Pin<Box<dyn Future<Output = std::result::Result<(), hyper::Error>> + Send>>;

struct PooledConnection {
    sender: hyper::client::conn::http1::SendRequest<WasiBody>,
    io: ConnectionIo,
    peer: SocketAddr,
    created: Instant,
}

struct IdleConnection {
    host: String,
    idle_since: Instant,
    connection: PooledConnection,
}

// Connections are driven only by the WASI request/IO futures, never by detached
// tasks. Only completely consumed responses can return their connection here.
struct OutboundClient {
    tls: Arc<rustls::ClientConfig>,
    idle: Mutex<Vec<IdleConnection>>,
    idle_per_host: usize,
}

impl OutboundClient {
    fn new() -> Self {
        // 0 disables reuse; tuning cannot grow the pool beyond its global cap.
        let idle_per_host = std::env::var("ONEDRIVE_HTTP_POOL_IDLE_PER_HOST")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1)
            .min(4);
        Self {
            tls: Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore {
                        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
                    })
                    .with_no_client_auth(),
            ),
            idle: Mutex::new(Vec::new()),
            idle_per_host,
        }
    }

    async fn take(&self, host: &str, peer: SocketAddr) -> Option<PooledConnection> {
        let mut connection = {
            let mut idle = self.idle.lock().unwrap();
            idle.retain(|entry| {
                entry.idle_since.elapsed() < OUTBOUND_IDLE_TIMEOUT
                    && entry.connection.created.elapsed() < OUTBOUND_CONNECTION_LIFETIME
                    && !entry.connection.sender.is_closed()
            });
            let index = idle
                .iter()
                .position(|entry| entry.host == host && entry.connection.peer == peer)?;
            idle.swap_remove(index).connection
        };
        // Check for idle peer closure before assigning a request. Never replay a
        // failed request: even an apparently failed mutation may have succeeded.
        let ready = std::future::poll_fn(|cx| {
            let open = connection.io.as_mut().poll(cx).is_pending() && connection.sender.is_ready();
            std::task::Poll::Ready(open)
        })
        .await;
        ready.then_some(connection)
    }

    fn put(&self, host: String, connection: PooledConnection) {
        if self.idle_per_host == 0
            || connection.sender.is_closed()
            || connection.created.elapsed() >= OUTBOUND_CONNECTION_LIFETIME
        {
            return;
        }
        let mut idle = self.idle.lock().unwrap();
        idle.retain(|entry| {
            entry.idle_since.elapsed() < OUTBOUND_IDLE_TIMEOUT
                && entry.connection.created.elapsed() < OUTBOUND_CONNECTION_LIFETIME
                && !entry.connection.sender.is_closed()
        });
        if idle.len() < OUTBOUND_IDLE_LIMIT
            && idle.iter().filter(|entry| entry.host == host).count() < self.idle_per_host
        {
            idle.push(IdleConnection {
                host,
                idle_since: Instant::now(),
                connection,
            });
        }
    }
}

const GUEST_ENV_NAMES: &[&str] = &[
    "ONEDRIVE_CLIENT_ID",
    "ONEDRIVE_TENANT_ID",
    "ONEDRIVE_DRIVE_BASE",
    "ONEDRIVE_SCOPES",
    "ONEDRIVE_BASIC_AUTH_SECRET",
    "ONEDRIVE_INDEX_ENABLED",
    "ONEDRIVE_SYNC_MAX_PAGES",
    "ONEDRIVE_SYNC_BUDGET_MS",
    "ONEDRIVE_MAX_UPLOAD_BYTES",
    "ONEDRIVE_PERF_LOG",
];

#[derive(Clone, Debug)]
struct HostConfig {
    component_path: PathBuf,
    state_dir: PathBuf,
    account_dir: Option<PathBuf>,
    listen_addr: SocketAddr,
}

impl HostConfig {
    fn parse() -> Result<Self> {
        let mut component_path = None;
        let mut state_dir = None;
        let mut account_dir = None;
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
                "--account-dir" => {
                    account_dir = Some(PathBuf::from(
                        args.next()
                            .ok_or_else(|| anyhow!("missing value for --account-dir"))?,
                    ));
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
                        "Usage: onedrive-davfs-host --component <path> --state-dir <path> [--account-dir <path>] [--addr {DEFAULT_ADDR}]\n\
                         Environment:\n\
                         ONEDRIVE_PERF_LOG=1: privacy-safe stage timings (off by default)\n\
                         ONEDRIVE_HTTP_POOL_IDLE_PER_HOST=0..4: idle connections per host (default 1; 0 disables reuse; global cap 8)\n\
                         ONEDRIVE_CACHED_DAV_READS=1: allow cached HEAD/PROPFIND while busy (off by default; cache misses return 503 until the next sync)"
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
        let state_dir = state_dir.canonicalize()?;
        let account_dir = account_dir.map(|path| path.canonicalize()).transpose()?;
        if let Some(account_dir) = account_dir.as_ref() {
            if !account_dir.is_dir()
                || account_dir.starts_with(&state_dir)
                || state_dir.starts_with(account_dir)
            {
                bail!("--account-dir must be separate from and not nested in per-library state");
            }
        }

        Ok(Self {
            component_path,
            state_dir,
            account_dir,
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

#[derive(Clone)]
struct HttpPolicyHooks {
    policy: OutboundPolicy,
    read_only: bool,
    client: Arc<OutboundClient>,
    #[cfg(test)]
    mock_peer: Option<SocketAddr>,
}

impl HttpPolicyHooks {
    fn new(policy: OutboundPolicy, client: Arc<OutboundClient>) -> Self {
        Self {
            policy,
            read_only: false,
            client,
            #[cfg(test)]
            mock_peer: None,
        }
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
        let client = self.client.clone();
        let read_only = self.read_only;
        #[cfg(test)]
        let mock_peer = self.mock_peer;
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
            if read_only {
                return Err(WasiHttpError::HttpRequestDenied);
            }
            #[cfg(test)]
            if let Some(peer) = mock_peer {
                policy.validate_uri(request.uri())?;
                let (response, io) = tests::send_mock_request(client, peer, request).await?;
                return Ok((
                    response,
                    Box::new(io)
                        as Box<dyn Future<Output = std::result::Result<(), WasiHttpError>> + Send>,
                ));
            }
            let (response, io) =
                send_restricted_request(client, policy, request, effective_options).await?;
            Ok((
                response,
                Box::new(io)
                    as Box<dyn Future<Output = std::result::Result<(), WasiHttpError>> + Send>,
            ))
        })
    }
}

async fn send_restricted_request(
    client: Arc<OutboundClient>,
    policy: OutboundPolicy,
    req: Request<WasiBody>,
    options: RequestOptions,
) -> std::result::Result<
    (
        Response<WasiBody>,
        impl Future<Output = std::result::Result<(), WasiHttpError>> + Send,
    ),
    WasiHttpError,
> {
    let _outbound_timer = PerfTimer::new("outbound_headers", "outbound");
    let validated = policy.validate_uri(req.uri())?;

    let connect_timeout = options.connect_timeout.unwrap_or(OUTBOUND_CONNECT_TIMEOUT);
    let first_byte_timeout = options
        .first_byte_timeout
        .unwrap_or(OUTBOUND_FIRST_BYTE_TIMEOUT);
    let between_bytes_timeout = options
        .between_bytes_timeout
        .unwrap_or(OUTBOUND_BETWEEN_BYTES_TIMEOUT);

    // Revalidate DNS on every request, including reuse. An established TLS
    // connection is reused only when its original public peer still matches.
    let connect_addr = {
        let _timer = PerfTimer::new("dns", "outbound");
        resolve_public_addr(&validated.host, validated.port, connect_timeout).await?
    };

    let connection = if let Some(connection) = client.take(&validated.host, connect_addr).await {
        let _timer = PerfTimer::new("connection_reused", "outbound");
        connection
    } else {
        let stream = {
            let _timer = PerfTimer::new("tcp", "outbound");
            tokio::time::timeout(connect_timeout, TcpStream::connect(connect_addr))
                .await
                .map_err(|_| WasiHttpError::ConnectionTimeout)?
                .map_err(WasiHttpError::Connect)?
        };
        let connector = tokio_rustls::TlsConnector::from(client.tls.clone());
        let server_name = rustls::pki_types::ServerName::try_from(validated.host.clone())
            .map_err(WasiHttpError::from)?;
        let tls_stream = {
            let _timer = PerfTimer::new("tls", "outbound");
            tokio::time::timeout(connect_timeout, connector.connect(server_name, stream))
                .await
                .map_err(|_| WasiHttpError::ConnectionTimeout)?
                .map_err(WasiHttpError::Tls)?
        };
        let (sender, io) = tokio::time::timeout(
            connect_timeout,
            hyper::client::conn::http1::Builder::new().handshake(TokioIo::new(tls_stream)),
        )
        .await
        .map_err(|_| WasiHttpError::ConnectionTimeout)?
        .map_err(WasiHttpError::from)?;
        PooledConnection {
            sender,
            io: Box::pin(io),
            peer: connect_addr,
            created: Instant::now(),
        }
    };
    send_on_connection(
        client,
        validated.host,
        connection,
        req,
        first_byte_timeout,
        between_bytes_timeout,
    )
    .await
}

async fn send_on_connection(
    client: Arc<OutboundClient>,
    host: String,
    connection: PooledConnection,
    mut req: Request<WasiBody>,
    first_byte_timeout: Duration,
    between_bytes_timeout: Duration,
) -> std::result::Result<
    (
        Response<WasiBody>,
        impl Future<Output = std::result::Result<(), WasiHttpError>> + Send,
    ),
    WasiHttpError,
> {
    let PooledConnection {
        mut sender,
        io,
        peer,
        created,
    } = connection;

    *req.uri_mut() = http::Uri::builder()
        .path_and_query(
            req.uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/"),
        )
        .build()
        .map_err(|_| WasiHttpError::HttpRequestUriInvalid)?;

    let (body_done, body_finished) = oneshot::channel();
    let send = async {
        struct IncomingResponseBody {
            incoming: hyper::body::Incoming,
            timeout: tokio::time::Interval,
            done: Option<oneshot::Sender<()>>,
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
                    std::task::Poll::Ready(None) => {
                        if let Some(done) = self.done.take() {
                            let _ = done.send(());
                        }
                        std::task::Poll::Ready(None)
                    }
                    std::task::Poll::Ready(Some(Err(err))) => {
                        self.done.take();
                        let err = if err.is_timeout() {
                            WasiHttpError::HttpResponseTimeout
                        } else {
                            WasiHttpError::from(err)
                        };
                        std::task::Poll::Ready(Some(Err(err)))
                    }
                    std::task::Poll::Ready(Some(Ok(frame))) => {
                        self.timeout.reset();
                        if self.incoming.is_end_stream() {
                            if let Some(done) = self.done.take() {
                                let _ = done.send(());
                            }
                        }
                        std::task::Poll::Ready(Some(Ok(frame)))
                    }
                    std::task::Poll::Pending => match Pin::new(&mut self.timeout).poll_tick(cx) {
                        std::task::Poll::Ready(_) => {
                            self.done.take();
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

        let _timer = PerfTimer::new("http_headers", "outbound");
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
            let mut done = Some(body_done);
            if incoming.is_end_stream() {
                let _ = done.take().unwrap().send(());
            }
            let body = IncomingResponseBody {
                incoming,
                timeout,
                done,
            };
            body.boxed_unsync()
        }))
    };

    let mut send = Box::pin(send);
    let mut conn = Some(io);
    let response = std::future::poll_fn(|cx| match send.as_mut().poll(cx) {
        std::task::Poll::Ready(Ok(resp)) => std::task::Poll::Ready(Ok(resp)),
        std::task::Poll::Ready(Err(err)) => std::task::Poll::Ready(Err(err)),
        std::task::Poll::Pending => {
            let Some(fut) = conn.as_mut() else {
                return std::task::Poll::Pending;
            };
            match fut.as_mut().poll(cx) {
                std::task::Poll::Ready(Ok(())) => {
                    conn = None;
                    send.as_mut().poll(cx)
                }
                std::task::Poll::Ready(Err(err)) => {
                    std::task::Poll::Ready(Err(WasiHttpError::from(err)))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    })
    .await?;
    drop(send);

    Ok((response, async move {
        let _timer = PerfTimer::new("outbound_body", "outbound");
        if let Some(mut io) = conn.take() {
            let ready = {
                let reusable = async {
                    if body_finished.await.is_err() {
                        return false;
                    }
                    sender.ready().await.is_ok()
                };
                tokio::pin!(reusable);
                tokio::select! {
                    ready = &mut reusable => ready,
                    result = &mut io => {
                        result.map_err(WasiHttpError::from)?;
                        return Ok(());
                    }
                }
            };
            if ready {
                client.put(
                    host,
                    PooledConnection {
                        sender,
                        io,
                        peer,
                        created,
                    },
                );
            }
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

struct QueueWaiter<'a>(&'a AtomicUsize);

impl Drop for QueueWaiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn acquire_execution_slot(
    execution_slot: &Arc<Semaphore>,
    queued_waiters: &AtomicUsize,
) -> Result<RequestAdmission> {
    match execution_slot.clone().try_acquire_owned() {
        Ok(permit) => Ok(RequestAdmission { permit }),
        Err(_) => {
            let queued = queued_waiters.fetch_add(1, Ordering::AcqRel) + 1;
            let _waiter = QueueWaiter(queued_waiters);
            if queued > QUEUE_LIMIT {
                bail!("request queue full");
            }
            let permit = execution_slot
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| anyhow!("execution semaphore closed"))?;
            Ok(RequestAdmission { permit })
        }
    }
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
    read_slots: Arc<Semaphore>,
    queued_waiters: AtomicUsize,
    outbound: Arc<OutboundClient>,
    cached_dav_reads: bool,
    #[cfg(test)]
    mock_peer: Option<SocketAddr>,
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
            read_slots: Arc::new(Semaphore::new(2)),
            queued_waiters: AtomicUsize::new(0),
            outbound: Arc::new(OutboundClient::new()),
            cached_dav_reads: std::env::var("ONEDRIVE_CACHED_DAV_READS")
                .is_ok_and(|value| value == "1"),
            #[cfg(test)]
            mock_peer: None,
        }
    }

    async fn admit(&self) -> Result<RequestAdmission> {
        acquire_execution_slot(&self.execution_slot, &self.queued_waiters).await
    }

    fn build_store(&self, read_only: bool) -> Result<Store<HostState>> {
        let mut wasi = WasiCtxBuilder::new();
        wasi.allow_blocking_current_thread(true);
        for (name, value) in self.guest_env.entries.iter() {
            wasi.env(name, value);
        }
        wasi.preopened_dir(
            &self.config.state_dir,
            "/state",
            if read_only {
                FsPerms::ReadOnly
            } else {
                FsPerms::ReadWrite
            },
        )
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

        let mut hooks = HttpPolicyHooks::new(OutboundPolicy, self.outbound.clone());
        hooks.read_only = read_only;
        #[cfg(test)]
        {
            hooks.mock_peer = self.mock_peer;
        }
        let state = HostState {
            table: ResourceTable::new(),
            wasi: wasi.build(),
            http,
            hooks,
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
        mut req: Request<Incoming>,
    ) -> Result<Response<HyperOutgoingBody>> {
        req.headers_mut().remove("x-onedrive-cache-only");
        let class = if is_local_read(&req, self.cached_dav_reads) {
            "local_read"
        } else {
            "serialized"
        };
        let queue_timer = PerfTimer::new("queue", class);
        let read_candidate = is_local_read(&req, self.cached_dav_reads);
        let (admission, read_only) = if read_candidate {
            match self.execution_slot.clone().try_acquire_owned() {
                Ok(permit) => (Ok(RequestAdmission { permit }), false),
                Err(_) => {
                    if req.headers().contains_key("authorization")
                        && !credentials::mirror_current(
                            &self.config.state_dir,
                            self.config.account_dir.as_deref(),
                        )?
                    {
                        return Ok(plain_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "account credentials are changing; retry the request",
                        ));
                    }
                    let admission = self
                        .read_slots
                        .clone()
                        .try_acquire_owned()
                        .map(|permit| RequestAdmission { permit })
                        .map_err(|_| anyhow!("read request slots full"));
                    (admission, true)
                }
            }
        } else {
            (self.admit().await, false)
        };
        let admission = match admission {
            Ok(admission) => admission,
            Err(_) => {
                return Ok(plain_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "request queue full",
                ));
            }
        };
        drop(queue_timer);

        let credential_timer = PerfTimer::new("credential_acquire", class);
        let mut credentials = if read_only {
            req.headers_mut()
                .insert("x-onedrive-cache-only", http::HeaderValue::from_static("1"));
            None
        } else {
            Some(
                credentials::CredentialLease::acquire(
                    &self.config.state_dir,
                    self.config.account_dir.as_deref(),
                )
                .await?,
            )
        };
        drop(credential_timer);
        let store_timer = PerfTimer::new("store", class);
        let mut store = self.build_store(read_only)?;
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
        drop(store_timer);

        let task = tokio::spawn(async move {
            let _permit = _permit;
            let execute_timer = PerfTimer::new("execute", class);
            let proxy = pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| anyhow!("failed to instantiate component: {e}"))?;
            let result = proxy
                .wasi_http_incoming_handler()
                .call_handle(store, req, out)
                .await
                .map_err(|e| anyhow!("component request failed: {e}"));
            drop(execute_timer);
            if let Some(credentials) = credentials.as_mut() {
                let _timer = PerfTimer::new("credential_publish", class);
                credentials.publish()?;
            }
            result?;
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

fn is_local_read<B>(req: &Request<B>, cached_dav_reads: bool) -> bool {
    // The guest must treat cache-only metadata misses as misses, not network
    // fallbacks. Read-only preopens and HTTP hooks enforce that boundary too.
    (cached_dav_reads
        && (req.method() == http::Method::HEAD || req.method().as_str() == "PROPFIND"))
        || req.method() == http::Method::GET
            && ((req
                .headers()
                .get("x-onedrive-plugin")
                .is_some_and(|value| value == "1")
                && matches!(req.uri().path(), "/_status" | "/_search" | "/_tree"))
                || (req.uri().path() == "/" && !req.headers().contains_key("authorization")))
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
    let _state_owner = credentials::lock_state(&config.state_dir)?;
    let _upload_directory = credentials::prepare_upload_directory(&config.state_dir)?;
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
                tasks.retain(|task: &tokio::task::JoinHandle<()>| !task.is_finished());
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
    fn only_local_cached_apis_and_bare_probes_use_the_read_lane() {
        for path in ["/_status", "/_search?q=fixture", "/_tree?path=/"] {
            let request = Request::builder()
                .uri(path)
                .header("x-onedrive-plugin", "1")
                .header("authorization", "Basic fixture")
                .body(())
                .unwrap();
            assert!(is_local_read(&request, false));
            assert!(is_local_read(&request, true));
        }
        assert!(is_local_read(
            &Request::builder().uri("/").body(()).unwrap(),
            false,
        ));
        for (method, path, plugin, authorized) in [
            ("POST", "/_status", true, true),
            ("GET", "/_libraries", true, true),
            ("GET", "/_resolve", true, true),
            ("GET", "/_status", false, true),
            ("GET", "/", false, true),
            ("GET", "/document", false, false),
            ("GET", "/document", false, true),
            ("PUT", "/document", false, true),
        ] {
            let mut request = Request::builder().method(method).uri(path);
            if plugin {
                request = request.header("x-onedrive-plugin", "1");
            }
            if authorized {
                request = request.header("authorization", "Basic fixture");
            }
            let request = request.body(()).unwrap();
            assert!(!is_local_read(&request, false));
            assert!(!is_local_read(&request, true));
        }
    }

    #[test]
    fn cached_dav_metadata_reads_require_explicit_opt_in() {
        for method in ["HEAD", "PROPFIND"] {
            for path in ["/", "/document", "/_status"] {
                for plugin in [false, true] {
                    let mut request = Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", "Basic fixture");
                    if plugin {
                        request = request.header("x-onedrive-plugin", "1");
                    }
                    let request = request.body(()).unwrap();
                    assert!(!is_local_read(&request, false));
                    assert!(is_local_read(&request, true));
                }
            }
        }
    }

    fn test_client(idle_per_host: usize) -> Arc<OutboundClient> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut client = OutboundClient::new();
        client.idle_per_host = idle_per_host;
        Arc::new(client)
    }

    struct TestServer {
        peer: SocketAddr,
        accepted: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl TestServer {
        async fn start() -> Self {
            Self::with_handler(|request: Request<Incoming>| async move {
                let close = request.uri().path() == "/close";
                let bytes = request.into_body().collect().await?.to_bytes();
                let mut response = Response::new(Full::new(bytes));
                if close {
                    response.headers_mut().insert(
                        http::header::CONNECTION,
                        http::HeaderValue::from_static("close"),
                    );
                }
                Ok::<_, hyper::Error>(response)
            })
            .await
        }

        async fn with_handler<F, Fut>(handler: F) -> Self
        where
            F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = std::result::Result<Response<Full<Bytes>>, hyper::Error>>
                + Send
                + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let peer = listener.local_addr().unwrap();
            let accepted = Arc::new(AtomicUsize::new(0));
            let count = accepted.clone();
            let task = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            count.fetch_add(1, Ordering::Relaxed);
                            let handler = handler.clone();
                            connections.spawn(async move {
                                let _ = http1::Builder::new().serve_connection(
                                    TokioIo::new(stream),
                                    service_fn(handler),
                                ).await;
                            });
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            });
            Self {
                peer,
                accepted,
                task,
            }
        }

        async fn connection(&self) -> PooledConnection {
            let stream = TcpStream::connect(self.peer).await.unwrap();
            let (sender, io) = hyper::client::conn::http1::Builder::new()
                .handshake(TokioIo::new(stream))
                .await
                .unwrap();
            PooledConnection {
                sender,
                io: Box::pin(io),
                peer: self.peer,
                created: Instant::now(),
            }
        }

        async fn round_trip(&self, client: &Arc<OutboundClient>, body: WasiBody) -> Bytes {
            let connection = match client.take("graph.microsoft.com", self.peer).await {
                Some(connection) => connection,
                None => self.connection().await,
            };
            let request = Request::builder()
                .method("POST")
                .uri("https://graph.microsoft.com/echo?not-logged")
                .header("host", "graph.microsoft.com")
                .body(body)
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                let (response, io) = send_on_connection(
                    client.clone(),
                    "graph.microsoft.com".into(),
                    connection,
                    request,
                    Duration::from_secs(2),
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
                let (body, result) = tokio::join!(response.into_body().collect(), io);
                result.unwrap();
                body.unwrap().to_bytes()
            })
            .await
            .expect("request body and connection IO must make progress")
        }
    }

    fn test_body(bytes: &'static [u8]) -> WasiBody {
        Full::new(Bytes::from_static(bytes))
            .map_err(|never| match never {})
            .boxed_unsync()
    }

    pub(super) async fn send_mock_request(
        client: Arc<OutboundClient>,
        peer: SocketAddr,
        request: Request<WasiBody>,
    ) -> std::result::Result<
        (
            Response<WasiBody>,
            impl Future<Output = std::result::Result<(), WasiHttpError>> + Send,
        ),
        WasiHttpError,
    > {
        assert!(peer.ip().is_loopback());
        let host = request.uri().host().unwrap().to_string();
        let connection = match client.take(&host, peer).await {
            Some(connection) => connection,
            None => {
                let stream = TcpStream::connect(peer)
                    .await
                    .map_err(WasiHttpError::Connect)?;
                let (sender, io) = hyper::client::conn::http1::Builder::new()
                    .handshake(TokioIo::new(stream))
                    .await
                    .map_err(WasiHttpError::from)?;
                PooledConnection {
                    sender,
                    io: Box::pin(io),
                    peer,
                    created: Instant::now(),
                }
            }
        };
        send_on_connection(
            client,
            host,
            connection,
            request,
            Duration::from_secs(30),
            Duration::from_secs(30),
        )
        .await
    }

    #[derive(Default, Debug)]
    struct MockGraphState {
        sessions: usize,
        cancellations: usize,
        deltas: usize,
        chunks: Vec<(String, usize)>,
        bytes: usize,
        active: usize,
        max_active: usize,
        failures: Vec<&'static str>,
    }

    struct ComponentFixture {
        server: HostServer,
        _graph: TestServer,
        observed: Arc<Mutex<MockGraphState>>,
        root: PathBuf,
    }

    impl Drop for ComponentFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl ComponentFixture {
        async fn new(component_path: PathBuf, total: usize, fail_chunk: Option<usize>) -> Self {
            static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::current_dir()
                .unwrap()
                .join("target")
                .join(format!(
                    "component-http-{}-{}",
                    std::process::id(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed),
                ));
            std::fs::create_dir(&root).unwrap();
            credentials::prepare_upload_directory(&root).unwrap();
            std::fs::write(
                root.join("token.json"),
                br#"{"credential_id":"fixture-login","refresh_token":"fixture-refresh","access_token":"fixture-access","expires_at":4102444800}"#,
            ).unwrap();
            let observed = Arc::new(Mutex::new(MockGraphState::default()));
            let state = observed.clone();
            let uploads = root.join(".uploads");
            let graph = TestServer::with_handler(move |request| {
                let state = state.clone();
                let uploads = uploads.clone();
                async move {
                    let (parts, mut body) = request.into_parts();
                    let is_session = parts.uri.path() == "/upload-fixture";
                    let is_upload = is_session && parts.method == http::Method::PUT;
                    let offset = {
                        let mut state = state.lock().unwrap();
                        if is_session {
                            if !matches!(parts.method, http::Method::PUT | http::Method::DELETE)
                                || parts.headers.contains_key("authorization")
                                || parts.headers.get("host").unwrap() != "upload.up.1drv.com"
                            {
                                state.failures.push("upload leaked authorization or used wrong destination/method");
                            }
                            if is_upload {
                                state.active += 1;
                                state.max_active = state.max_active.max(state.active);
                            }
                        } else if parts.headers.get("authorization")
                            != Some(&http::HeaderValue::from_static("Bearer fixture-access"))
                        {
                            state.failures.push("Graph authorization missing");
                        }
                        state.bytes
                    };
                    let mut received = 0;
                    let mut valid = true;
                    while let Some(frame) = body.frame().await {
                        if let Ok(data) = frame?.into_data() {
                            if is_upload {
                                valid &= data.iter().enumerate().all(|(index, byte)| {
                                    *byte == ((offset + received + index) % 251) as u8
                                });
                            }
                            received += data.len();
                            tokio::task::yield_now().await;
                        }
                    }
                    let mut state = state.lock().unwrap();
                    let (status, json) = if is_upload {
                        let expected_len = (total - offset).min(10 * 1024 * 1024);
                        let expected_range = format!(
                            "bytes {offset}-{}/{}", offset + expected_len - 1, total,
                        );
                        let range = parts.headers.get("content-range")
                            .and_then(|value| value.to_str().ok()).unwrap_or("");
                        let length = parts.headers.get("content-length")
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.parse::<usize>().ok());
                        if !valid || received != expected_len || range != expected_range
                            || length != Some(expected_len)
                        {
                            state.failures.push("upload body, content length, or sequential content range mismatch");
                        }
                        if std::fs::read_dir(&uploads).unwrap().next().is_some() {
                            state.failures.push("upload spool was not unlinked");
                        }
                        state.active -= 1;
                        state.bytes += received;
                        state.chunks.push((range.to_string(), received));
                        if fail_chunk == Some(state.chunks.len()) {
                            (503, r#"{"error":{"code":"serviceUnavailable"}}"#.into())
                        } else if state.bytes == total {
                            (201, format!(
                                r#"{{"id":"fixture-file-id","name":"large.bin","size":{total},"file":{{}},"parentReference":{{"id":"fixture-root"}},"eTag":"fixture-etag"}}"#,
                            ))
                        } else {
                            (202, format!(r#"{{"nextExpectedRanges":["{}-"]}}"#, state.bytes))
                        }
                    } else if is_session && parts.method == http::Method::DELETE {
                        state.cancellations += 1;
                        (204, String::new())
                    } else if parts.uri.path().ends_with("/createUploadSession") {
                        state.sessions += 1;
                        (200, r#"{"uploadUrl":"https://upload.up.1drv.com/upload-fixture"}"#.into())
                    } else if parts.uri.path() == "/v1.0/me" {
                        (200, r#"{"id":"fixture-account"}"#.into())
                    } else if parts.uri.path() == "/v1.0/me/drive" {
                        (200, r#"{"id":"fixture-drive","name":"Fixture","driveType":"business","webUrl":"https://tenant.sharepoint.com/fixture"}"#.into())
                    } else if parts.uri.path() == "/v1.0/me/drive/root/delta" {
                        let items = if state.deltas == 0 {
                            r#"{"id":"fixture-root","name":"root","root":{},"folder":{}}"#
                        } else {
                            ""
                        };
                        state.deltas += 1;
                        (200, format!(
                            r#"{{"value":[{items}],"@odata.deltaLink":"https://graph.microsoft.com/v1.0/me/drive/root/delta?fixture=unchanged"}}"#,
                        ))
                    } else {
                        state.failures.push("unexpected Graph request");
                        (500, "{}".into())
                    };
                    Ok::<_, hyper::Error>(Response::builder().status(status)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Full::new(Bytes::from(json))).unwrap())
                }
            }).await;
            let engine = build_engine().unwrap();
            let pre = load_pre(&engine, &component_path).await.unwrap();
            let config = HostConfig {
                component_path,
                state_dir: root.clone(),
                account_dir: None,
                listen_addr: "127.0.0.1:0".parse().unwrap(),
            };
            let guest_env = GuestEnv {
                entries: Arc::new(vec![
                    (
                        "ONEDRIVE_BASIC_AUTH_SECRET".into(),
                        "abcdefghijklmnopqrstuvwxyz123456".into(),
                    ),
                    (
                        "ONEDRIVE_MAX_UPLOAD_BYTES".into(),
                        (128 * 1024 * 1024).to_string(),
                    ),
                    ("ONEDRIVE_PERF_LOG".into(), "1".into()),
                ]),
            };
            let mut server = HostServer::new(pre, engine, config, guest_env);
            server.mock_peer = Some(graph.peer);
            Self {
                server,
                _graph: graph,
                observed,
                root,
            }
        }

        async fn request(
            &self,
            method: &str,
            path: &str,
            body: Bytes,
        ) -> (StatusCode, http::HeaderMap, Bytes) {
            let mut request = Request::builder()
                .method(method)
                .uri(format!("http://localhost{path}"))
                .header(
                    "authorization",
                    "Basic dTphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejEyMzQ1Ng==",
                );
            if path == "/_sync" {
                request = request.header("x-onedrive-sync", "1");
            } else if path.starts_with("/_") {
                request = request.header("x-onedrive-plugin", "1");
            }
            let request = request.body(Full::new(body)).unwrap();
            let mut store = self.server.build_store(false).unwrap();
            let request = store
                .data_mut()
                .http()
                .new_incoming_request(Scheme::Http, request)
                .unwrap();
            let (sender, receiver) = oneshot::channel();
            let out = store
                .data_mut()
                .http()
                .new_response_outparam(sender)
                .unwrap();
            let execute = async {
                let proxy = self.server.pre.instantiate_async(&mut store).await.unwrap();
                proxy
                    .wasi_http_incoming_handler()
                    .call_handle(store, request, out)
                    .await
                    .unwrap();
            };
            let response = async {
                let response = receiver.await.unwrap().unwrap();
                let (parts, body) = response.into_parts();
                (
                    parts.status,
                    parts.headers,
                    body.collect().await.unwrap().to_bytes(),
                )
            };
            tokio::time::timeout(Duration::from_secs(120), async {
                let ((), response) = tokio::join!(execute, response);
                response
            })
            .await
            .expect("real WASI request/response body execution must not deadlock")
        }

        fn assert_no_spool(&self) {
            assert!(
                std::fs::read_dir(self.root.join(".uploads"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            #[cfg(target_os = "linux")]
            for descriptor in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
                if let Ok(target) = std::fs::read_link(descriptor.path()) {
                    assert!(
                        !target.starts_with(self.root.join(".uploads")),
                        "spool descriptor retained"
                    );
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ONEDRIVE_TEST_COMPONENT pointing to a freshly built WASIp2 guest"]
    async fn real_component_upload_and_unchanged_sync_body_progress() {
        const TOTAL: usize = 65 * 1024 * 1024 + 17;
        let component = PathBuf::from(
            std::env::var_os("ONEDRIVE_TEST_COMPONENT")
                .expect("select a freshly built guest using ONEDRIVE_TEST_COMPONENT"),
        );
        let _ = rustls::crypto::ring::default_provider().install_default();
        let fixture = ComponentFixture::new(component, TOTAL, None).await;
        let (status, _, body) = fixture.request("POST", "/_sync", Bytes::new()).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let payload = Bytes::from(
            (0..TOTAL)
                .map(|offset| (offset % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let started = Instant::now();
        let (status, _, body) = fixture.request("PUT", "/large.bin", payload).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}; mock state: {:?}",
            String::from_utf8_lossy(&body),
            fixture.observed.lock().unwrap(),
        );
        {
            let state = fixture.observed.lock().unwrap();
            assert!(state.failures.is_empty(), "{state:?}");
            assert_eq!(state.sessions, 1);
            assert_eq!(state.cancellations, 0);
            assert_eq!(state.bytes, TOTAL);
            assert_eq!(state.max_active, 1);
            assert_eq!(state.chunks.len(), 7);
            assert_eq!(state.chunks.last().unwrap().1, 5 * 1024 * 1024 + 17);
        }
        fixture.assert_no_spool();
        let (status, _, body) = fixture
            .request("GET", "/_tree?path=/large.bin", Bytes::new())
            .await;
        let metadata = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(status, StatusCode::OK, "{metadata}");
        assert!(
            metadata.contains(&format!("\"size\":{TOTAL}")),
            "{metadata}"
        );
        assert!(metadata.contains("\"etag\":\"fixture-etag\""), "{metadata}");
        assert!(metadata.contains("\"isDir\":false"), "{metadata}");
        assert!(metadata.contains("\"source\":\"index\""), "{metadata}");

        // Corrupt only the directory table, leaving header+metadata intact.
        // Unchanged ticks must succeed without decoding this full namespace.
        let path = fixture.root.join("index.bin");
        let mut snapshot = std::fs::read(&path).unwrap();
        let table = u64::from_le_bytes(snapshot[8..16].try_into().unwrap()) as usize;
        snapshot[table..table + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, &snapshot).unwrap();
        for _ in 0..2 {
            let (status, _, body) = fixture.request("POST", "/_sync", Bytes::new()).await;
            let report = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(status, StatusCode::OK, "{report}");
            assert!(
                report.contains("\"snapshot_load_skipped\":true"),
                "{report}"
            );
            for stage in [
                "binding_ms",
                "snapshot_load_ms",
                "delta_ms",
                "apply_ms",
                "snapshot_save_ms",
            ] {
                assert!(report.contains(&format!("\"{stage}\":")), "{report}");
            }
            assert_eq!(std::fs::read(&path).unwrap(), snapshot);
        }
        let (status, _, body) = fixture.request("GET", "/_status", Bytes::new()).await;
        let report = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(status, StatusCode::OK, "{report}");
        assert!(
            report.contains("\"snapshot_load_skipped\":true"),
            "{report}"
        );
        assert!(fixture.observed.lock().unwrap().failures.is_empty());
        println!(
            "real_wasi_upload bytes={TOTAL} chunks=7 elapsed_ms={} repeated_nochange_syncs=2 full_snapshot_loads=0",
            started.elapsed().as_millis()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ONEDRIVE_TEST_COMPONENT pointing to a freshly built WASIp2 guest"]
    async fn real_component_failed_chunk_cancels_without_replay_or_snapshot_mutation() {
        const TOTAL: usize = 25 * 1024 * 1024 + 17;
        let component = PathBuf::from(
            std::env::var_os("ONEDRIVE_TEST_COMPONENT")
                .expect("select a freshly built guest using ONEDRIVE_TEST_COMPONENT"),
        );
        let _ = rustls::crypto::ring::default_provider().install_default();
        let fixture = ComponentFixture::new(component, TOTAL, Some(2)).await;
        let (status, _, body) = fixture.request("POST", "/_sync", Bytes::new()).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let snapshot = std::fs::read(fixture.root.join("index.bin")).unwrap();
        let payload = Bytes::from(
            (0..TOTAL)
                .map(|offset| (offset % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (status, _, body) = fixture.request("PUT", "/large.bin", payload).await;
        let error = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{error}");
        assert!(
            error.contains("HTTP 503"),
            "{error}; {:?}",
            fixture.observed.lock().unwrap()
        );
        {
            let state = fixture.observed.lock().unwrap();
            assert!(state.failures.is_empty(), "{state:?}");
            assert_eq!(state.sessions, 1);
            assert_eq!(state.chunks.len(), 2);
            assert_eq!(state.bytes, 20 * 1024 * 1024);
            assert_eq!(state.cancellations, 1);
            assert_eq!(state.max_active, 1);
        }
        assert_eq!(
            std::fs::read(fixture.root.join("index.bin")).unwrap(),
            snapshot
        );
        fixture.assert_no_spool();
    }

    #[tokio::test]
    async fn pooled_requests_drive_streaming_bodies_and_reuse_connections() {
        struct StreamingBody {
            frame: usize,
            pending: bool,
            dropped: Arc<AtomicUsize>,
        }
        impl Body for StreamingBody {
            type Data = Bytes;
            type Error = WasiHttpError;

            fn poll_frame(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<std::result::Result<http_body::Frame<Bytes>, WasiHttpError>>>
            {
                if self.frame == 4 {
                    return std::task::Poll::Ready(None);
                }
                if !self.pending {
                    self.pending = true;
                    cx.waker().wake_by_ref();
                    return std::task::Poll::Pending;
                }
                self.pending = false;
                self.frame += 1;
                std::task::Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from(vec![
                    b'x';
                    32 * 1024
                ])))))
            }
        }
        impl Drop for StreamingBody {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let server = TestServer::start().await;
        let client = test_client(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let body = StreamingBody {
                frame: 0,
                pending: false,
                dropped: dropped.clone(),
            };
            let received = server.round_trip(&client, body.boxed_unsync()).await;
            assert_eq!(received.len(), 128 * 1024);
            assert!(received.iter().all(|byte| *byte == b'x'));
            assert_eq!(client.idle.lock().unwrap().len(), 1);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        assert_eq!(server.accepted.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn abandoned_responses_and_cancelled_io_are_not_reused() {
        let server = TestServer::start().await;
        let client = test_client(1);
        for cancel_io in [false, true] {
            let request = Request::builder()
                .uri("/echo")
                .body(test_body(b"body"))
                .unwrap();
            let (response, io) = send_on_connection(
                client.clone(),
                "graph.microsoft.com".into(),
                server.connection().await,
                request,
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert!(client.idle.lock().unwrap().is_empty());
            drop(response);
            if cancel_io {
                drop(io);
            } else {
                tokio::time::timeout(Duration::from_secs(2), io)
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(client.idle.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn peer_connection_close_is_not_pooled() {
        let server = TestServer::start().await;
        let client = test_client(1);
        let request = Request::builder()
            .uri("/close")
            .body(test_body(b"fixture"))
            .unwrap();
        let (response, io) = send_on_connection(
            client.clone(),
            "graph.microsoft.com".into(),
            server.connection().await,
            request,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let (body, result) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(response.into_body().collect(), io)
        })
        .await
        .unwrap();
        assert_eq!(body.unwrap().to_bytes(), "fixture");
        result.unwrap();
        assert!(client.idle.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_idle_connections_are_discarded_before_sending() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = listener.local_addr().unwrap();
        let stream = TcpStream::connect(peer).await.unwrap();
        let (remote, _) = listener.accept().await.unwrap();
        let (sender, io) = hyper::client::conn::http1::Builder::new()
            .handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let client = test_client(1);
        client.put(
            "graph.microsoft.com".into(),
            PooledConnection {
                sender,
                io: Box::pin(io),
                peer,
                created: Instant::now(),
            },
        );
        drop(remote);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(client.take("graph.microsoft.com", peer).await.is_none());
        assert!(client.idle.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pool_respects_host_peer_expiry_and_capacity_boundaries() {
        let server = TestServer::start().await;
        let client = test_client(1);
        client.put("graph.microsoft.com".into(), server.connection().await);
        client.put("graph.microsoft.com".into(), server.connection().await);
        assert_eq!(client.idle.lock().unwrap().len(), 1);
        assert!(
            client
                .take("login.microsoftonline.com", server.peer)
                .await
                .is_none()
        );
        let other_peer = SocketAddr::from(([127, 0, 0, 1], 1));
        assert!(
            client
                .take("graph.microsoft.com", other_peer)
                .await
                .is_none()
        );
        client.idle.lock().unwrap()[0].idle_since -= OUTBOUND_IDLE_TIMEOUT;
        assert!(
            client
                .take("graph.microsoft.com", server.peer)
                .await
                .is_none()
        );
        let mut old = server.connection().await;
        old.created -= OUTBOUND_CONNECTION_LIFETIME;
        client.put("graph.microsoft.com".into(), old);
        assert!(client.idle.lock().unwrap().is_empty());
        for index in 0..OUTBOUND_IDLE_LIMIT + 2 {
            client.put(
                format!("tenant{index}.sharepoint.com"),
                server.connection().await,
            );
        }
        assert_eq!(client.idle.lock().unwrap().len(), OUTBOUND_IDLE_LIMIT);
        let disabled = test_client(0);
        disabled.put("graph.microsoft.com".into(), server.connection().await);
        assert!(disabled.idle.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "isolated HTTP benchmark; run explicitly with --ignored --nocapture"]
    async fn benchmark_isolated_http_pool() {
        const REQUESTS: usize = 200;
        for idle_per_host in [0, 1] {
            let server = TestServer::start().await;
            let client = test_client(idle_per_host);
            let start = Instant::now();
            for _ in 0..REQUESTS {
                assert_eq!(
                    server.round_trip(&client, test_body(b"fixture")).await,
                    "fixture"
                );
            }
            let elapsed = start.elapsed();
            let connections = server.accepted.load(Ordering::Relaxed);
            assert_eq!(connections, if idle_per_host == 0 { REQUESTS } else { 1 });
            println!(
                "isolated_http pool_idle_per_host={idle_per_host} requests={REQUESTS} connections={connections} elapsed_us={}",
                elapsed.as_micros(),
            );
        }
    }

    #[tokio::test]
    async fn cancelled_waiters_do_not_exhaust_admission_queue() {
        let semaphore = Arc::new(Semaphore::new(1));
        let queued = AtomicUsize::new(0);
        let held = semaphore.clone().acquire_owned().await.unwrap();
        for _ in 0..QUEUE_LIMIT + 2 {
            let future = acquire_execution_slot(&semaphore, &queued);
            tokio::pin!(future);
            tokio::select! {
                biased;
                _ = &mut future => panic!("busy slot must wait"),
                _ = async {} => {}
            }
            assert_eq!(queued.load(Ordering::Acquire), 1);
        }
        assert_eq!(queued.load(Ordering::Acquire), 0);
        queued.store(QUEUE_LIMIT, Ordering::Release);
        assert!(acquire_execution_slot(&semaphore, &queued).await.is_err());
        assert_eq!(queued.load(Ordering::Acquire), QUEUE_LIMIT);
        queued.store(0, Ordering::Release);
        semaphore.close();
        assert!(acquire_execution_slot(&semaphore, &queued).await.is_err());
        assert_eq!(queued.load(Ordering::Acquire), 0);
        drop(held);
        let semaphore = Arc::new(Semaphore::new(1));
        assert!(acquire_execution_slot(&semaphore, &queued).await.is_ok());
        assert_eq!(queued.load(Ordering::Acquire), 0);
    }

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
