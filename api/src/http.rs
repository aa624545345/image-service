// Copyright 2022 Alibaba Cloud. All rights reserved.
// Copyright 2020 Ant Group. All rights reserved.
// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::io::{self, Error, ErrorKind, Result};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvError, SendError, Sender};
use std::sync::Arc;
use std::time::SystemTime;
use std::{fs, thread};

use dbs_uhttp::{Body, HttpServer, MediaType, Request, Response, ServerError, StatusCode, Version};
use http::uri::Uri;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use serde::Deserialize;
use serde_json::{Error as SerdeError, Value};
use url::Url;

use nydus_utils::metrics::IoStatsError;

use crate::http_endpoint_common::{
    EventsHandler, ExitHandler, MetricsBackendHandler, MetricsBlobcacheHandler, MountHandler,
    SendFuseFdHandler, StartHandler, TakeoverFuseFdHandler,
};
use crate::http_endpoint_v1::{
    FsBackendInfo, InfoHandler, MetricsFsAccessPatternHandler, MetricsFsFilesHandler,
    MetricsFsGlobalHandler, MetricsFsInflightHandler, HTTP_ROOT_V1,
};
use crate::http_endpoint_v2::{BlobObjectListHandlerV2, InfoV2Handler, HTTP_ROOT_V2};

const EXIT_TOKEN: Token = Token(usize::MAX);
const REQUEST_TOKEN: Token = Token(1);

/// Mount a filesystem.
#[derive(Clone, Deserialize, Debug)]
pub struct ApiMountCmd {
    /// Path to source of the filesystem.
    pub source: String,
    /// Type of filesystem.
    #[serde(default)]
    pub fs_type: String,
    /// Configuration for the filesystem.
    pub config: String,
    /// List of files to prefetch.
    #[serde(default)]
    pub prefetch_files: Option<Vec<String>>,
}

/// Umount a mounted filesystem.
#[derive(Clone, Deserialize, Debug)]
pub struct ApiUmountCmd {
    /// Path of mountpoint.
    pub mountpoint: String,
}

/// Set/update daemon configuration.
#[derive(Clone, Deserialize, Debug)]
pub struct DaemonConf {
    /// Logging level: Off, Error, Warn, Info, Debug, Trace.
    pub log_level: String,
}

/// Configuration information for a cached blob, corresponding to `storage::FactoryConfig`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobCacheEntryConfig {
    /// Identifier for the blob cache configuration: corresponding to `FactoryConfig::id`.
    #[serde(default)]
    pub id: String,
    /// Type of storage backend, corresponding to `FactoryConfig::BackendConfig::backend_type`.
    pub backend_type: String,
    /// Configuration for storage backend, corresponding to `FactoryConfig::BackendConfig::backend_config`.
    ///
    /// Possible value: `LocalFsConfig`, `RegistryOssConfig`.
    pub backend_config: Value,
    /// Type of blob cache, corresponding to `FactoryConfig::CacheConfig::cache_type`.
    ///
    /// Possible value: "fscache", "filecache".
    pub cache_type: String,
    /// Configuration for blob cache, corresponding to `FactoryConfig::CacheConfig::cache_config`.
    ///
    /// Possible value: `FileCacheConfig`, `FsCacheConfig`.
    pub cache_config: Value,
    /// Configuration for data prefetch.
    #[serde(default)]
    pub prefetch_config: BlobPrefetchConfig,
    /// Optional file path for metadata blobs.
    #[serde(default)]
    pub metadata_path: Option<String>,
}

/// Blob cache object type for nydus/rafs bootstrap blob.
pub const BLOB_CACHE_TYPE_BOOTSTRAP: &str = "bootstrap";
/// Blob cache object type for nydus/rafs data blob.
pub const BLOB_CACHE_TYPE_DATA_BLOB: &str = "datablob";

/// Configuration information for a cached blob.
#[derive(Debug, Deserialize, Serialize)]
pub struct BlobCacheEntry {
    /// Type of blob object, bootstrap or data blob.
    #[serde(rename = "type")]
    pub blob_type: String,
    /// Blob id.
    #[serde(rename = "id")]
    pub blob_id: String,
    /// Configuration information to generate blob cache object.
    #[serde(rename = "config")]
    pub blob_config: BlobCacheEntryConfig,
    /// Domain id for the blob, which is used to group cached blobs into management domains.
    #[serde(default)]
    pub domain_id: String,
    /// Deprecated: data prefetch configuration: BlobPrefetchConfig.
    #[serde(default)]
    pub fs_prefetch: Option<BlobPrefetchConfig>,
}

/// Configuration information for a list of cached blob objects.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BlobCacheList {
    /// List of blob configuration information.
    pub blobs: Vec<BlobCacheEntry>,
}

/// Identifier for cached blob objects.
///
/// Domains are used to control the blob sharing scope. All blobs associated with the same domain
/// will be shared/reused, but blobs associated with different domains are isolated.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BlobCacheObjectId {
    /// Domain identifier for the object.
    #[serde(default)]
    pub domain_id: String,
    /// Blob identifier for the object.
    #[serde(default)]
    pub blob_id: String,
}

/// Configuration information for blob data prefetching.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct BlobPrefetchConfig {
    /// Whether to enable blob data prefetching.
    pub enable: bool,
    /// Number of data prefetching working threads.
    pub threads_count: usize,
    /// The maximum size of a merged IO request.
    pub merging_size: usize,
    /// Network bandwidth rate limit in unit of Bytes and Zero means no limit.
    pub bandwidth_rate: u32,
}

fn default_work_dir() -> String {
    ".".to_string()
}

/// Configuration information for file cache.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileCacheConfig {
    /// Working directory to store state and cached files.
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    /// Deprecated: disable index mapping, keep it as false when possible.
    #[serde(default)]
    pub disable_indexed_map: bool,
}

impl FileCacheConfig {
    /// Get the working directory.
    pub fn get_work_dir(&self) -> Result<&str> {
        let path = fs::metadata(&self.work_dir)
            .or_else(|_| {
                fs::create_dir_all(&self.work_dir)?;
                fs::metadata(&self.work_dir)
            })
            .map_err(|e| {
                last_error!(format!(
                    "fail to stat filecache work_dir {}: {}",
                    self.work_dir, e
                ))
            })?;

        if path.is_dir() {
            Ok(&self.work_dir)
        } else {
            Err(enoent!(format!(
                "filecache work_dir {} is not a directory",
                self.work_dir
            )))
        }
    }
}

/// Configuration information for fscache.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FsCacheConfig {
    /// Working directory to store state and cached files.
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
}

impl FsCacheConfig {
    /// Get the working directory.
    pub fn get_work_dir(&self) -> Result<&str> {
        let path = fs::metadata(&self.work_dir)
            .or_else(|_| {
                fs::create_dir_all(&self.work_dir)?;
                fs::metadata(&self.work_dir)
            })
            .map_err(|e| {
                last_error!(format!(
                    "fail to stat fscache work_dir {}: {}",
                    self.work_dir, e
                ))
            })?;

        if path.is_dir() {
            Ok(&self.work_dir)
        } else {
            Err(enoent!(format!(
                "fscache work_dir {} is not a directory",
                self.work_dir
            )))
        }
    }
}

/// Configuration information for network proxy.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxyConfig {
    /// Access remote storage backend via P2P proxy, e.g. Dragonfly dfdaemon server URL.
    pub url: String,
    /// Endpoint of P2P proxy health checking.
    pub ping_url: String,
    /// Fallback to remote storage backend if P2P proxy ping failed.
    pub fallback: bool,
    /// Interval of P2P proxy health checking, in seconds.
    pub check_interval: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            ping_url: String::new(),
            fallback: true,
            check_interval: 5,
        }
    }
}

/// Generic configuration for storage backends.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RegistryOssConfig {
    /// Enable HTTP proxy for the read request.
    pub proxy: ProxyConfig,
    /// Skip SSL certificate validation for HTTPS scheme.
    pub skip_verify: bool,
    /// Drop the read request once http request timeout, in seconds.
    pub timeout: u64,
    /// Drop the read request once http connection timeout, in seconds.
    pub connect_timeout: u64,
    /// Retry count when read request failed.
    pub retry_limit: u8,
}

impl Default for RegistryOssConfig {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(),
            skip_verify: false,
            timeout: 5,
            connect_timeout: 5,
            retry_limit: 0,
        }
    }
}

#[derive(Debug)]
pub enum ApiRequest {
    /// Set daemon configuration.
    ConfigureDaemon(DaemonConf),
    /// Get daemon information.
    GetDaemonInfo,
    /// Get daemon global events.
    GetEvents,
    /// Stop the daemon.
    Exit,
    /// Start the daemon.
    Start,
    /// Send fuse fd to new daemon.
    SendFuseFd,
    /// Take over fuse fd from old daemon instance.
    TakeoverFuseFd,

    // Filesystem Related
    /// Mount a filesystem.
    Mount(String, ApiMountCmd),
    /// Remount a filesystem.
    Remount(String, ApiMountCmd),
    /// Unmount a filesystem.
    Umount(String),

    /// Get storage backend metrics.
    ExportBackendMetrics(Option<String>),
    /// Get blob cache metrics.
    ExportBlobcacheMetrics(Option<String>),

    // Nydus API v1 requests
    /// Get filesystem global metrics.
    ExportFsGlobalMetrics(Option<String>),
    /// Get filesystem access pattern log.
    ExportFsAccessPatterns(Option<String>),
    /// Get filesystem backend information.
    ExportFsBackendInfo(String),
    /// Get filesystem file metrics.
    ExportFsFilesMetrics(Option<String>, bool),
    /// Get information about filesystem inflight requests.
    ExportFsInflightMetrics,

    // Nydus API v2
    /// Get daemon information excluding filesystem backends.
    GetDaemonInfoV2,
    /// Create a blob cache entry
    CreateBlobObject(BlobCacheEntry),
    /// Get information about blob cache entries
    GetBlobObject(BlobCacheObjectId),
    /// Delete a blob cache entry
    DeleteBlobObject(BlobCacheObjectId),
}

/// Kinds for daemon related error messages.
#[derive(Debug)]
pub enum DaemonErrorKind {
    /// Service not ready yet.
    NotReady,
    /// Generic errors.
    Other(String),
    /// Message serialization/deserialization related errors.
    Serde(SerdeError),
    /// Unexpected event type.
    UnexpectedEvent(String),
    /// Can't upgrade the daemon.
    UpgradeManager,
    /// Unsupported requests.
    Unsupported,
}

/// Kinds for metrics related error messages.
#[derive(Debug)]
pub enum MetricsErrorKind {
    /// Generic daemon related errors.
    Daemon(DaemonErrorKind),
    /// Errors related to metrics implementation.
    Stats(IoStatsError),
}

/// Errors generated by/related to the API service, sent back through [`ApiResponse`].
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ApiError {
    /// Daemon internal error
    DaemonAbnormal(DaemonErrorKind),
    /// Failed to get events information
    Events(String),
    /// Failed to get metrics information
    Metrics(MetricsErrorKind),
    /// Failed to mount filesystem
    MountFilesystem(DaemonErrorKind),
    /// Failed to send request to the API service
    RequestSend(SendError<Option<ApiRequest>>),
    /// Unrecognized payload content
    ResponsePayloadType,
    /// Failed to receive response from the API service
    ResponseRecv(RecvError),
    /// Failed to send wakeup notification
    Wakeup(io::Error),
}

/// Specialized `std::result::Result` for API replies.
pub type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Serialize)]
pub enum ApiResponsePayload {
    /// Filesystem backend metrics.
    BackendMetrics(String),
    /// Blobcache metrics.
    BlobcacheMetrics(String),
    /// Daemon version, configuration and status information in json.
    DaemonInfo(String),
    /// No data is sent on the channel.
    Empty,
    /// Global error events.
    Events(String),

    /// Filesystem global metrics, v1.
    FsGlobalMetrics(String),
    /// Filesystem per-file metrics, v1.
    FsFilesMetrics(String),
    /// Filesystem access pattern trace log, v1.
    FsFilesPatterns(String),
    // Filesystem Backend Information, v1.
    FsBackendInfo(String),
    // Filesystem Inflight Requests, v1.
    FsInflightMetrics(String),

    /// List of blob objects, v2
    BlobObjectList(String),
}

/// Specialized version of [`std::result::Result`] for value returned by backend services.
pub type ApiResponse = std::result::Result<ApiResponsePayload, ApiError>;

/// HTTP error messages sent back to the clients.
///
/// The `HttpError` object will be sent back to client with `format!("{:?}", http_error)`.
/// So unfortunately it implicitly becomes parts of the API, please keep it stable.
#[derive(Debug)]
pub enum HttpError {
    // Daemon common related errors
    /// Invalid HTTP request
    BadRequest,
    /// Failed to configure the daemon.
    Configure(ApiError),
    /// Failed to query information about daemon.
    DaemonInfo(ApiError),
    /// Failed to query global events.
    Events(ApiError),
    /// No handler registered for HTTP request URI
    NoRoute,
    /// Failed to parse HTTP request message body
    ParseBody(SerdeError),
    /// Query parameter is missed from the HTTP request.
    QueryString(String),

    /// Failed to mount filesystem.
    Mount(ApiError),
    /// Failed to remount filesystem.
    Upgrade(ApiError),

    // Metrics related errors
    /// Failed to get backend metrics.
    BackendMetrics(ApiError),
    /// Failed to get blobcache metrics.
    BlobcacheMetrics(ApiError),

    // Filesystem related errors (v1)
    /// Failed to get filesystem backend information
    FsBackendInfo(ApiError),
    /// Failed to get filesystem per-file metrics.
    FsFilesMetrics(ApiError),
    /// Failed to get global metrics.
    GlobalMetrics(ApiError),
    /// Failed to get information about inflight request
    InflightMetrics(ApiError),
    /// Failed to get filesystem file access trace.
    Pattern(ApiError),

    // Blob cache management related errors (v2)
    /// Failed to create blob object
    CreateBlobObject(ApiError),
    /// Failed to delete blob object
    DeleteBlobObject(ApiError),
    /// Failed to list existing blob objects
    GetBlobObjects(ApiError),
}

/// Specialized version of [`std::result::Result`] for value returned by [`EndpointHandler`].
pub type HttpResult = std::result::Result<Response, HttpError>;

#[derive(Serialize, Debug)]
struct ErrorMessage {
    code: String,
    message: String,
}

impl From<ErrorMessage> for Vec<u8> {
    fn from(msg: ErrorMessage) -> Self {
        // Safe to unwrap since `ErrorMessage` must succeed in serialization
        serde_json::to_vec(&msg).unwrap()
    }
}

/// Get query parameter with `key` from the HTTP request.
pub fn extract_query_part(req: &Request, key: &str) -> Option<String> {
    // Splicing req.uri with "http:" prefix might look weird, but since it depends on
    // crate `Url` to generate query_pairs HashMap, which is working on top of Url not Uri.
    // Better that we can add query part support to Micro-http in the future. But
    // right now, below way makes it easy to obtain query parts from uri.
    let http_prefix = format!("http:{}", req.uri().get_abs_path());
    let url = Url::parse(&http_prefix)
        .map_err(|e| {
            error!("api: can't parse request {:?}", e);
            e
        })
        .ok()?;

    for (k, v) in url.query_pairs() {
        if k == key {
            trace!("api: got query param {}={}", k, v);
            return Some(v.into_owned());
        }
    }
    None
}

/// Parse HTTP request body.
pub(crate) fn parse_body<'a, F: Deserialize<'a>>(b: &'a Body) -> std::result::Result<F, HttpError> {
    serde_json::from_slice::<F>(b.raw()).map_err(HttpError::ParseBody)
}

/// Translate ApiError message to HTTP status code.
pub(crate) fn translate_status_code(e: &ApiError) -> StatusCode {
    match e {
        ApiError::DaemonAbnormal(kind) | ApiError::MountFilesystem(kind) => match kind {
            DaemonErrorKind::NotReady => StatusCode::ServiceUnavailable,
            DaemonErrorKind::Unsupported => StatusCode::NotImplemented,
            DaemonErrorKind::UnexpectedEvent(_) => StatusCode::BadRequest,
            _ => StatusCode::InternalServerError,
        },
        ApiError::Metrics(MetricsErrorKind::Stats(IoStatsError::NoCounter)) => StatusCode::NotFound,
        _ => StatusCode::InternalServerError,
    }
}

/// Generate a successful HTTP response message.
pub(crate) fn success_response(body: Option<String>) -> Response {
    if let Some(body) = body {
        let mut r = Response::new(Version::Http11, StatusCode::OK);
        r.set_body(Body::new(body));
        r
    } else {
        Response::new(Version::Http11, StatusCode::NoContent)
    }
}

/// Generate a HTTP error response message with status code and error message.
pub(crate) fn error_response(error: HttpError, status: StatusCode) -> Response {
    let mut response = Response::new(Version::Http11, status);
    let err_msg = ErrorMessage {
        code: "UNDEFINED".to_string(),
        message: format!("{:?}", error),
    };
    response.set_body(Body::new(err_msg));
    response
}

/// Trait for HTTP endpoints to handle HTTP requests.
pub trait EndpointHandler: Sync + Send {
    /// Handles an HTTP request.
    ///
    /// The main responsibilities of the handlers includes:
    /// - parse and validate incoming request message
    /// - send the request to subscriber
    /// - wait response from the subscriber
    /// - generate HTTP result
    fn handle_request(
        &self,
        req: &Request,
        kicker: &dyn Fn(ApiRequest) -> ApiResponse,
    ) -> HttpResult;
}

/// Struct to route HTTP requests to corresponding registered endpoint handlers.
pub struct HttpRoutes {
    /// routes is a hash table mapping endpoint URIs to their endpoint handlers.
    pub routes: HashMap<String, Box<dyn EndpointHandler + Sync + Send>>,
}

macro_rules! endpoint_v1 {
    ($path:expr) => {
        format!("{}{}", HTTP_ROOT_V1, $path)
    };
}

macro_rules! endpoint_v2 {
    ($path:expr) => {
        format!("{}{}", HTTP_ROOT_V2, $path)
    };
}

lazy_static! {
    /// HTTP_ROUTES contain all the nydusd HTTP routes.
    pub static ref HTTP_ROUTES: HttpRoutes = {
        let mut r = HttpRoutes {
            routes: HashMap::new(),
        };

        // Common
        r.routes.insert(endpoint_v1!("/daemon/events"), Box::new(EventsHandler{}));
        r.routes.insert(endpoint_v1!("/daemon/exit"), Box::new(ExitHandler{}));
        r.routes.insert(endpoint_v1!("/daemon/start"), Box::new(StartHandler{}));
        r.routes.insert(endpoint_v1!("/daemon/fuse/sendfd"), Box::new(SendFuseFdHandler{}));
        r.routes.insert(endpoint_v1!("/daemon/fuse/takeover"), Box::new(TakeoverFuseFdHandler{}));
        r.routes.insert(endpoint_v1!("/mount"), Box::new(MountHandler{}));
        r.routes.insert(endpoint_v1!("/metrics/backend"), Box::new(MetricsBackendHandler{}));
        r.routes.insert(endpoint_v1!("/metrics/blobcache"), Box::new(MetricsBlobcacheHandler{}));

        // Nydus API, v1
        r.routes.insert(endpoint_v1!("/daemon"), Box::new(InfoHandler{}));
        r.routes.insert(endpoint_v1!("/daemon/backend"), Box::new(FsBackendInfo{}));
        r.routes.insert(endpoint_v1!("/metrics"), Box::new(MetricsFsGlobalHandler{}));
        r.routes.insert(endpoint_v1!("/metrics/files"), Box::new(MetricsFsFilesHandler{}));
        r.routes.insert(endpoint_v1!("/metrics/inflight"), Box::new(MetricsFsInflightHandler{}));
        r.routes.insert(endpoint_v1!("/metrics/pattern"), Box::new(MetricsFsAccessPatternHandler{}));

        // Nydus API, v2
        r.routes.insert(endpoint_v2!("/daemon"), Box::new(InfoV2Handler{}));
        r.routes.insert(endpoint_v2!("/blobs"), Box::new(BlobObjectListHandlerV2{}));

        r
    };
}

fn kick_api_server(
    api_notifier: Option<Arc<Waker>>,
    to_api: &Sender<Option<ApiRequest>>,
    from_api: &Receiver<ApiResponse>,
    request: ApiRequest,
) -> ApiResponse {
    to_api.send(Some(request)).map_err(ApiError::RequestSend)?;
    if let Some(waker) = api_notifier {
        waker.wake().map_err(ApiError::Wakeup)?;
    }
    from_api.recv().map_err(ApiError::ResponseRecv)?
}

// Example:
// <-- GET /
// --> GET / 200 835ms 746b

fn trace_api_begin(request: &dbs_uhttp::Request) {
    info!("<--- {:?} {:?}", request.method(), request.uri());
}

fn trace_api_end(response: &dbs_uhttp::Response, method: dbs_uhttp::Method, recv_time: SystemTime) {
    let elapse = SystemTime::now().duration_since(recv_time);
    info!(
        "---> {:?} Status Code: {:?}, Elapse: {:?}, Body Size: {:?}",
        method,
        response.status(),
        elapse,
        response.content_length()
    );
}

fn exit_api_server(api_notifier: Option<Arc<Waker>>, to_api: &Sender<Option<ApiRequest>>) {
    if to_api.send(None).is_err() {
        error!("failed to send stop request api server");
        return;
    }
    if let Some(waker) = api_notifier {
        let _ = waker
            .wake()
            .map_err(|_e| error!("failed to send notify api server for exit"));
    }
}

fn handle_http_request(
    request: &Request,
    api_notifier: Option<Arc<Waker>>,
    to_api: &Sender<Option<ApiRequest>>,
    from_api: &Receiver<ApiResponse>,
) -> Response {
    let begin_time = SystemTime::now();
    trace_api_begin(request);

    // Micro http should ensure that req path is legal.
    let uri_parsed = request.uri().get_abs_path().parse::<Uri>();
    let mut response = match uri_parsed {
        Ok(uri) => match HTTP_ROUTES.routes.get(uri.path()) {
            Some(route) => route
                .handle_request(request, &|r| {
                    kick_api_server(api_notifier.clone(), to_api, from_api, r)
                })
                .unwrap_or_else(|err| error_response(err, StatusCode::BadRequest)),
            None => error_response(HttpError::NoRoute, StatusCode::NotFound),
        },
        Err(e) => {
            error!("Failed parse URI, {}", e);
            error_response(HttpError::BadRequest, StatusCode::BadRequest)
        }
    };
    response.set_server("Nydus API");
    response.set_content_type(MediaType::ApplicationJson);

    trace_api_end(&response, request.method(), begin_time);

    response
}

/// Start a HTTP server to serve API requests.
///
/// Start a HTTP server parsing http requests and send to nydus API server a concrete
/// request to operate nydus or fetch working status.
/// The HTTP server sends request by `to_api` channel and wait for response from `from_api` channel.
pub fn start_http_thread(
    path: &str,
    api_notifier: Option<Arc<Waker>>,
    to_api: Sender<Option<ApiRequest>>,
    from_api: Receiver<ApiResponse>,
) -> Result<(thread::JoinHandle<Result<()>>, Arc<Waker>)> {
    // Try to remove existed unix domain socket
    std::fs::remove_file(path).unwrap_or_default();
    let socket_path = PathBuf::from(path);

    let mut poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), EXIT_TOKEN)?);
    let waker2 = waker.clone();
    let mut server = HttpServer::new(socket_path).map_err(|e| {
        if let ServerError::IOError(e) = e {
            e
        } else {
            Error::new(ErrorKind::Other, format!("{:?}", e))
        }
    })?;
    poll.registry().register(
        &mut SourceFd(&server.epoll().as_raw_fd()),
        REQUEST_TOKEN,
        Interest::READABLE,
    )?;

    let thread = thread::Builder::new()
        .name("nydus-http-server".to_string())
        .spawn(move || {
            // Must start the server successfully or just die by panic
            server.start_server().unwrap();
            info!("http server started");

            let mut events = Events::with_capacity(100);
            let mut do_exit = false;
            loop {
                match poll.poll(&mut events, None) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        error!("http server poll events failed, {}", e);
                        exit_api_server(api_notifier, &to_api);
                        return Err(e);
                    }
                    Ok(_) => {}
                }

                for event in &events {
                    match event.token() {
                        EXIT_TOKEN => do_exit = true,
                        REQUEST_TOKEN => match server.requests() {
                            Ok(request_vec) => {
                                for server_request in request_vec {
                                    let reply = server_request.process(|request| {
                                        handle_http_request(
                                            request,
                                            api_notifier.clone(),
                                            &to_api,
                                            &from_api,
                                        )
                                    });
                                    // Ignore error when sending response
                                    server.respond(reply).unwrap_or_else(|e| {
                                        error!("HTTP server error on response: {}", e)
                                    });
                                }
                            }
                            Err(e) => {
                                error!("HTTP server error on retrieving incoming request: {}", e);
                            }
                        },
                        _ => unreachable!("unknown poll token."),
                    }
                }

                if do_exit {
                    exit_api_server(api_notifier, &to_api);
                    break;
                }
            }

            info!("http-server thread exits");
            // Keep the Waker alive to match the lifetime of the poll loop above
            drop(waker2);
            Ok(())
        })?;

    Ok((thread, waker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use vmm_sys_util::tempfile::TempFile;

    #[test]
    fn test_blob_prefetch_config() {
        let config = BlobPrefetchConfig::default();
        assert!(!config.enable);
        assert_eq!(config.threads_count, 0);
        assert_eq!(config.merging_size, 0);
        assert_eq!(config.bandwidth_rate, 0);

        let content = r#"{
            "enable": true,
            "threads_count": 2,
            "merging_size": 4,
            "bandwidth_rate": 5
        }"#;
        let config: BlobPrefetchConfig = serde_json::from_str(content).unwrap();
        assert!(config.enable);
        assert_eq!(config.threads_count, 2);
        assert_eq!(config.merging_size, 4);
        assert_eq!(config.bandwidth_rate, 5);
    }

    #[test]
    fn test_file_cache_config() {
        let config: FileCacheConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(&config.work_dir, ".");
        assert!(!config.disable_indexed_map);

        let config: FileCacheConfig =
            serde_json::from_str("{\"work_dir\":\"/tmp\",\"disable_indexed_map\":true}").unwrap();
        assert_eq!(&config.work_dir, "/tmp");
        assert!(config.get_work_dir().is_ok());
        assert!(config.disable_indexed_map);

        let config: FileCacheConfig =
            serde_json::from_str("{\"work_dir\":\"/proc/mounts\",\"disable_indexed_map\":true}")
                .unwrap();
        assert!(config.get_work_dir().is_err());
    }

    #[test]
    fn test_fs_cache_config() {
        let config: FsCacheConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(&config.work_dir, ".");

        let config: FileCacheConfig = serde_json::from_str("{\"work_dir\":\"/tmp\"}").unwrap();
        assert_eq!(&config.work_dir, "/tmp");
        assert!(config.get_work_dir().is_ok());

        let config: FileCacheConfig =
            serde_json::from_str("{\"work_dir\":\"/proc/mounts\"}").unwrap();
        assert!(config.get_work_dir().is_err());
    }

    #[test]
    fn test_blob_cache_entry() {
        let content = r#"{
            "type": "bootstrap",
            "id": "blob1",
            "config": {
                "id": "cache1",
                "backend_type": "localfs",
                "backend_config": {},
                "cache_type": "fscache",
                "cache_config": {},
                "prefetch_config": {
                    "enable": true,
                    "threads_count": 2,
                    "merging_size": 4,
                    "bandwidth_rate": 5
                },
                "metadata_path": "/tmp/metadata1"
            },
            "domain_id": "domain1",
            "fs_prefetch": {
                "enable": true,
                "threads_count": 2,
                "merging_size": 4,
                "bandwidth_rate": 5
            }
        }"#;
        let config: BlobCacheEntry = serde_json::from_str(content).unwrap();
        assert_eq!(&config.blob_type, BLOB_CACHE_TYPE_BOOTSTRAP);
        assert_eq!(&config.blob_id, "blob1");
        assert_eq!(&config.domain_id, "domain1");
        assert_eq!(&config.blob_config.id, "cache1");
        assert_eq!(&config.blob_config.backend_type, "localfs");
        assert_eq!(&config.blob_config.cache_type, "fscache");
        assert!(config.blob_config.cache_config.is_object());
        assert!(config.blob_config.prefetch_config.enable);
        assert_eq!(config.blob_config.prefetch_config.threads_count, 2);
        assert_eq!(config.blob_config.prefetch_config.merging_size, 4);
        assert_eq!(
            config.blob_config.metadata_path.as_ref().unwrap().as_str(),
            "/tmp/metadata1"
        );
        assert!(config.fs_prefetch.is_some());

        let content = r#"{
            "type": "bootstrap",
            "id": "blob1",
            "config": {
                "id": "cache1",
                "backend_type": "localfs",
                "backend_config": {},
                "cache_type": "fscache",
                "cache_config": {},
                "metadata_path": "/tmp/metadata1"
            },
            "domain_id": "domain1"
        }"#;
        let config: BlobCacheEntry = serde_json::from_str(content).unwrap();
        assert!(!config.blob_config.prefetch_config.enable);
        assert_eq!(config.blob_config.prefetch_config.threads_count, 0);
        assert_eq!(config.blob_config.prefetch_config.merging_size, 0);
        assert!(config.fs_prefetch.is_none());
    }

    #[test]
    fn test_registry_oss_config() {
        let content = r#"{
            "proxy": {
                "url": "http://proxy.com",
                "ping_url": "http://proxy.com/ping",
                "fallback": true,
                "check_interval": 10
            },
            "skip_verify": true,
            "timeout": 60,
            "connect_timeout": 10,
            "retry_limit": 3
        }"#;
        let config: RegistryOssConfig = serde_json::from_str(content).unwrap();
        assert!(config.skip_verify);
        assert_eq!(config.timeout, 60);
        assert_eq!(config.connect_timeout, 10);
        assert_eq!(config.retry_limit, 3);
        assert_eq!(&config.proxy.url, "http://proxy.com");
        assert_eq!(&config.proxy.ping_url, "http://proxy.com/ping");
        assert!(config.proxy.fallback);
        assert_eq!(config.proxy.check_interval, 10);
    }

    #[test]
    fn test_http_api_routes_v1() {
        assert!(HTTP_ROUTES.routes.get("/api/v1/daemon").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/daemon/events").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/daemon/backend").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/daemon/start").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/daemon/exit").is_some());
        assert!(HTTP_ROUTES
            .routes
            .get("/api/v1/daemon/fuse/sendfd")
            .is_some());
        assert!(HTTP_ROUTES
            .routes
            .get("/api/v1/daemon/fuse/takeover")
            .is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/mount").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/metrics").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/metrics/files").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/metrics/pattern").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/metrics/backend").is_some());
        assert!(HTTP_ROUTES
            .routes
            .get("/api/v1/metrics/blobcache")
            .is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v1/metrics/inflight").is_some());
    }

    #[test]
    fn test_http_api_routes_v2() {
        assert!(HTTP_ROUTES.routes.get("/api/v2/daemon").is_some());
        assert!(HTTP_ROUTES.routes.get("/api/v2/blobs").is_some());
    }

    #[test]
    fn test_kick_api_server() {
        let (to_api, from_route) = channel();
        let (to_route, from_api) = channel();
        let request = ApiRequest::GetDaemonInfo;
        let thread =
            thread::spawn(
                move || match kick_api_server(None, &to_api, &from_api, request) {
                    Err(reply) => matches!(reply, ApiError::ResponsePayloadType),
                    Ok(_) => panic!("unexpected reply message"),
                },
            );
        let req2 = from_route.recv().unwrap();
        matches!(req2.as_ref().unwrap(), ApiRequest::GetDaemonInfo);
        let reply: ApiResponse = Err(ApiError::ResponsePayloadType);
        to_route.send(reply).unwrap();
        thread.join().unwrap();

        let (to_api, from_route) = channel();
        let (to_route, from_api) = channel();
        drop(to_route);
        let request = ApiRequest::GetDaemonInfo;
        assert!(kick_api_server(None, &to_api, &from_api, request).is_err());
        drop(from_route);
        let request = ApiRequest::GetDaemonInfo;
        assert!(kick_api_server(None, &to_api, &from_api, request).is_err());
    }

    #[test]
    fn test_extract_query_part() {
        let req = Request::try_from(
            b"GET http://localhost/api/v1/daemon?arg1=test HTTP/1.0\r\n\r\n",
            None,
        )
        .unwrap();
        let arg1 = extract_query_part(&req, "arg1").unwrap();
        assert_eq!(arg1, "test");
        assert!(extract_query_part(&req, "arg2").is_none());
    }

    #[test]
    fn test_start_http_thread() {
        let tmpdir = TempFile::new().unwrap();
        let path = tmpdir.as_path().to_str().unwrap();
        let (to_api, from_route) = channel();
        let (_to_route, from_api) = channel();
        let (thread, waker) = start_http_thread(path, None, to_api, from_api).unwrap();
        waker.wake().unwrap();

        let msg = from_route.recv().unwrap();
        assert!(msg.is_none());
        let _ = thread.join().unwrap();
    }

    #[test]
    fn test_common_config() {
        let config = RegistryOssConfig::default();

        assert_eq!(config.timeout, 5);
        assert_eq!(config.connect_timeout, 5);
        assert_eq!(config.retry_limit, 0);
        assert_eq!(config.proxy.check_interval, 5);
        assert!(config.proxy.fallback);
        assert_eq!(config.proxy.ping_url, "");
        assert_eq!(config.proxy.url, "");
    }
}
