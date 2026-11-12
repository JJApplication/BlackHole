use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use tokio::{
    fs as async_fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::{Mutex, RwLock},
};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

#[derive(Debug, Deserialize, Clone)]
struct Config {
    proxy: ProxyConfig,
    log: LogConfig,
    server: ServerConfig,
}

#[derive(Debug, Deserialize, Clone)]
struct ProxyConfig {
    enabled: bool,
    static_dir: String,
    cache_dir: String,
    /// 开启后，unpkg 下载可从半成品续传，并对已有文件响应单段 Range。
    #[serde(default)]
    resume: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct LogConfig {
    enabled: bool,
    level: String,
}

#[derive(Debug, Deserialize, Clone)]
struct ServerConfig {
    port: u16,
    host: String,
}

#[derive(Clone)]
struct AppState {
    config: Config,
    client: reqwest::Client,
    unpkg_regex: Regex,
    index_cache: Arc<RwLock<Option<String>>>,
    downloads: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
}

enum Fetched {
    Cached,
    Downloaded,
}

struct DownloadFail {
    status: StatusCode,
    message: String,
}

impl DownloadFail {
    fn upstream(status: reqwest::StatusCode) -> Self {
        Self {
            status: StatusCode::from_u16(status.as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            message: format!("unpkg returned error: {status}"),
        }
    }

    fn other(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn into_response(self) -> Response {
        error!("[Black Hole] {}", self.message);
        (self.status, self.message).into_response()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RangeOutcome {
    Full,
    Unsatisfiable,
    Single { start: u64, end: u64 },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 加载配置文件
    let config = load_config("config.toml").await?;

    // 初始化日志
    if config.log.enabled {
        let level = match config.log.level.as_str() {
            "trace" => tracing::Level::TRACE,
            "debug" => tracing::Level::DEBUG,
            "info" => tracing::Level::INFO,
            "warn" => tracing::Level::WARN,
            "error" => tracing::Level::ERROR,
            _ => tracing::Level::INFO,
        };

        tracing_subscriber::fmt().with_max_level(level).init();

        info!(
            "[Black Hole] Configuration loaded successfully: {:?}",
            config
        );
    }

    // 创建必要的目录
    create_dirs(&config).await?;

    // 创建HTTP客户端
    let client = reqwest::Client::new();

    // 编译正则表达式，支持scoped packages（@开头的包名）
    let unpkg_regex = Regex::new(r"^/static/(@?[^@/]+(?:/[^@/]+)?)@([^/]+)/(.+)$")?;

    // 创建应用状态
    let state = AppState {
        config: config.clone(),
        client,
        unpkg_regex,
        index_cache: Arc::new(RwLock::new(None)),
        downloads: Arc::new(Mutex::new(HashMap::new())),
    };

    // 创建路由
    let app = Router::new()
        .route("/static/*path", get(handle_static_request))
        .route("/", get(handle_index))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // 启动服务器
    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("[Black Hole] Starting server");
    info!("[Black Hole] Server started at http://{}", addr);
    info!(
        "[Black Hole] Proxy feature status: {}",
        config.proxy.enabled
    );
    info!(
        "[Black Hole] Resume feature status: {}",
        config.proxy.resume
    );

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn load_config(filename: &str) -> anyhow::Result<Config> {
    let content = async_fs::read_to_string(filename).await?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

async fn create_dirs(config: &Config) -> anyhow::Result<()> {
    let dirs = vec![&config.proxy.static_dir, &config.proxy.cache_dir];
    for dir in dirs {
        async_fs::create_dir_all(dir).await?;
    }
    Ok(())
}

async fn handle_index(State(state): State<AppState>) -> impl IntoResponse {
    // 首先检查缓存
    {
        let cache = state.index_cache.read().await;
        if let Some(cached_content) = cache.as_ref() {
            info!("[Black Hole] Using cached index.html");
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            return (StatusCode::OK, headers, cached_content.clone()).into_response();
        }
    }

    // 缓存中没有，从文件读取
    let index_path = PathBuf::from("ui").join("index.html");
    info!(
        "[Black Hole] Reading index.html from file: {:?}",
        index_path
    );

    match async_fs::read_to_string(&index_path).await {
        Ok(content) => {
            // 将内容存入缓存
            {
                let mut cache = state.index_cache.write().await;
                *cache = Some(content.clone());
            }

            info!("[Black Hole] Successfully read and cached index.html");
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            (StatusCode::OK, headers, content).into_response()
        }
        Err(e) => {
            error!("[Black Hole] Failed to read index.html: {}", e);
            (StatusCode::NOT_FOUND, "404 - index.html file not found").into_response()
        }
    }
}

async fn handle_static_request(
    Path(path): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let request_path = format!("/static/{}", path);
    info!("[Black Hole] Received request: {}", request_path);

    // 检查是否为unpkg格式
    if let Some(captures) = state.unpkg_regex.captures(&request_path) {
        let package_name = captures.get(1).unwrap().as_str();
        let version = captures.get(2).unwrap().as_str();
        let file_path = captures.get(3).unwrap().as_str();

        return handle_unpkg_request(&state, package_name, version, file_path, &headers).await;
    }

    // 本地静态文件请求
    handle_local_static_request(&state, &path, &headers).await
}

async fn handle_local_static_request(
    state: &AppState,
    file_path: &str,
    request_headers: &HeaderMap,
) -> Response {
    // 安全路径验证
    if !is_safe_path(file_path) {
        warn!("[Black Hole] Detected unsafe path access: {}", file_path);
        return (StatusCode::FORBIDDEN, "Forbidden: Unsafe path").into_response();
    }

    let local_path = PathBuf::from(&state.config.proxy.static_dir).join(file_path);

    // 验证解析后的路径是否在允许的目录内
    if !is_path_within_allowed_dirs(&local_path, &state.config.proxy.static_dir) {
        warn!(
            "[Black Hole] Detected directory traversal attack: {:?}",
            local_path
        );
        return (
            StatusCode::FORBIDDEN,
            "Forbidden: Outside allowed directory range",
        )
            .into_response();
    }

    info!("[Black Hole] Looking for local file: {:?}", local_path);

    if state.config.proxy.resume {
        if is_existing_file(&local_path).await {
            info!(
                "[Black Hole] Successfully returned local file: {}",
                file_path
            );
            return serve_file(&local_path, file_path, request_headers).await;
        }
        warn!("[Black Hole] File not found: {}", file_path);
        return (
            StatusCode::NOT_FOUND,
            format!("File not found: {}", file_path),
        )
            .into_response();
    }

    match async_fs::read(&local_path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();
            set_content_type(&mut headers, file_path);

            info!(
                "[Black Hole] Successfully returned local file: {}",
                file_path
            );
            (StatusCode::OK, headers, content).into_response()
        }
        Err(_) => {
            warn!("[Black Hole] File not found: {}", file_path);
            (
                StatusCode::NOT_FOUND,
                format!("File not found: {}", file_path),
            )
                .into_response()
        }
    }
}

async fn handle_unpkg_request(
    state: &AppState,
    package_name: &str,
    version: &str,
    file_path: &str,
    request_headers: &HeaderMap,
) -> Response {
    // 构建缓存路径，去掉版本号前的@符号以兼容Windows文件系统
    let safe_version = version.trim_start_matches('@');
    let cache_dir = PathBuf::from(&state.config.proxy.cache_dir)
        .join(package_name)
        .join(safe_version);
    let cached_file = cache_dir.join(file_path);

    info!(
        "[Black Hole] Checking cache file: {:?}",
        cached_file.display()
    );

    if state.config.proxy.resume {
        if is_existing_file(&cached_file).await {
            info!("[Black Hole] Using cached file: {:?}", cached_file);
            return serve_file(&cached_file, file_path, request_headers).await;
        }
    } else if let Ok(content) = async_fs::read(&cached_file).await {
        info!("[Black Hole] Using cached file: {:?}", cached_file);
        let mut headers = HeaderMap::new();
        set_content_type(&mut headers, file_path);
        return (StatusCode::OK, headers, content).into_response();
    }

    if !state.config.proxy.enabled {
        return (StatusCode::SERVICE_UNAVAILABLE, "Proxy service not enabled").into_response();
    }

    let unpkg_url = format!(
        "https://unpkg.com/{}@{}/{}",
        package_name, version, file_path
    );
    if !state.config.proxy.resume {
        return download_full(state, &cached_file, file_path, &unpkg_url).await;
    }

    match ensure_download(state, &cached_file, &unpkg_url).await {
        Ok(Fetched::Cached) => {
            info!("[Black Hole] Using cached file: {:?}", cached_file);
            serve_file(&cached_file, file_path, request_headers).await
        }
        Ok(Fetched::Downloaded) => {
            info!(
                "[Black Hole] Successfully downloaded and cached file: {}",
                file_path
            );
            serve_file(&cached_file, file_path, request_headers).await
        }
        Err(err) => err.into_response(),
    }
}

async fn download_full(
    state: &AppState,
    cached_file: &FsPath,
    file_path: &str,
    unpkg_url: &str,
) -> Response {
    info!("[Black Hole] Downloading from unpkg: {}", unpkg_url);

    match state.client.get(unpkg_url).send().await {
        Ok(response) => {
            if !response.status().is_success() {
                let status = response.status();
                error!("[Black Hole] unpkg returned error: {}", status);
                return (
                    StatusCode::from_u16(status.as_u16())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    format!("unpkg returned error: {}", status),
                )
                    .into_response();
            }

            match response.bytes().await {
                Ok(content) => {
                    // 创建缓存目录（包括文件的父目录）
                    if let Some(parent_dir) = cached_file.parent() {
                        if let Err(e) = async_fs::create_dir_all(parent_dir).await {
                            warn!("[Black Hole] Failed to create cache directory: {}", e);
                        }
                    }
                    // 保存到缓存
                    if let Err(e) = async_fs::write(cached_file, &content).await {
                        warn!("[Black Hole] Failed to save cache file: {}", e);
                    }

                    let mut headers = HeaderMap::new();
                    set_content_type(&mut headers, file_path);

                    info!(
                        "[Black Hole] Successfully downloaded and cached file: {}",
                        file_path
                    );
                    (StatusCode::OK, headers, content.to_vec()).into_response()
                }
                Err(e) => {
                    error!("[Black Hole] Failed to read response: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to read response: {}", e),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            error!("[Black Hole] Download failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Download failed: {}", e),
            )
                .into_response()
        }
    }
}

/// 同一缓存路径串行下载。半成品写在 `.part`，校验信息写在 `.part.meta`。
async fn ensure_download(
    state: &AppState,
    cached_file: &FsPath,
    unpkg_url: &str,
) -> Result<Fetched, DownloadFail> {
    let slot = {
        let mut downloads = state.downloads.lock().await;
        downloads
            .entry(cached_file.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };

    let guard = match slot.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            info!(
                "[Black Hole] Waiting for in-progress download: {:?}",
                cached_file
            );
            slot.lock().await
        }
    };

    let result = if is_existing_file(cached_file).await {
        Ok(Fetched::Cached)
    } else {
        download_resumable(state, cached_file, unpkg_url)
            .await
            .map(|()| Fetched::Downloaded)
    };

    drop(guard);
    drop(slot);
    release_download_slot(state, cached_file).await;
    result
}

async fn release_download_slot(state: &AppState, key: &FsPath) {
    let mut downloads = state.downloads.lock().await;
    if let Some(existing) = downloads.get(key) {
        if Arc::strong_count(existing) == 1 {
            downloads.remove(key);
        }
    }
}

async fn download_resumable(
    state: &AppState,
    cached_file: &FsPath,
    unpkg_url: &str,
) -> Result<(), DownloadFail> {
    let (part_path, meta_path) = partial_paths(cached_file);
    if let Some(parent) = cached_file.parent() {
        if let Err(e) = async_fs::create_dir_all(parent).await {
            return Err(DownloadFail::other(format!(
                "Failed to create cache directory: {e}"
            )));
        }
    }

    let mut force_full = false;
    for _ in 0..2 {
        let offset = if force_full {
            0
        } else {
            file_len(&part_path).await
        };
        let resuming = offset > 0;

        let mut request = state.client.get(unpkg_url);
        if resuming {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            if let Some(validator) = read_validator(&meta_path).await {
                request = request.header(reqwest::header::IF_RANGE, validator);
            }
            info!(
                "[Black Hole] Resuming download at byte {}: {}",
                offset, unpkg_url
            );
        } else {
            info!("[Black Hole] Downloading from unpkg: {}", unpkg_url);
        }

        let mut response = match request.send().await {
            Ok(response) => response,
            Err(e) => return Err(DownloadFail::other(format!("Download failed: {e}"))),
        };

        let status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && resuming {
            match head_content_length(&state.client, unpkg_url).await {
                Some(total) if total == offset => {
                    info!(
                        "[Black Hole] Partial file already complete: {:?}",
                        part_path
                    );
                    return promote_partial(&part_path, &meta_path, cached_file).await;
                }
                _ => {
                    warn!(
                        "[Black Hole] Range not satisfiable, restarting download: {}",
                        unpkg_url
                    );
                    let _ = async_fs::remove_file(&part_path).await;
                    let _ = async_fs::remove_file(&meta_path).await;
                    force_full = true;
                    continue;
                }
            }
        }

        if !status.is_success() {
            return Err(DownloadFail::upstream(status));
        }

        let (range_start, range_total) = content_range_bounds(response.headers());
        let content_length = response.content_length();
        let etag = header_str(response.headers(), &reqwest::header::ETAG);
        let last_modified = header_str(response.headers(), &reqwest::header::LAST_MODIFIED);
        let is_partial = status == reqwest::StatusCode::PARTIAL_CONTENT;

        if is_partial {
            if let Some(start) = range_start {
                if start != offset {
                    return Err(DownloadFail::other(format!(
                        "Unexpected Content-Range start {start}, expected {offset}"
                    )));
                }
            }
        }

        let restart = !is_partial;
        let mut file = prepare_part(&part_path, restart, offset).await?;
        if restart || etag.is_some() || last_modified.is_some() {
            write_validator(&meta_path, etag.as_deref(), last_modified.as_deref()).await;
        }

        let base = if restart { 0 } else { offset };
        let written = stream_into(&mut file, &mut response, base).await?;
        drop(file);

        let expected = if is_partial {
            range_total.or_else(|| content_length.map(|len| offset.saturating_add(len)))
        } else {
            content_length.or(range_total)
        };
        if let Some(expected) = expected {
            if written != expected {
                return Err(DownloadFail::other(format!(
                    "Incomplete download: got {written} bytes, expected {expected}"
                )));
            }
        }

        return promote_partial(&part_path, &meta_path, cached_file).await;
    }

    Err(DownloadFail::other("Download failed after retry"))
}

async fn prepare_part(
    path: &FsPath,
    restart: bool,
    offset: u64,
) -> Result<async_fs::File, DownloadFail> {
    let mut file = async_fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(path)
        .await
        .map_err(|e| DownloadFail::other(format!("Failed to open cache file: {e}")))?;

    if restart {
        file.set_len(0)
            .await
            .map_err(|e| DownloadFail::other(format!("Failed to reset cache file: {e}")))?;
        file.seek(SeekFrom::Start(0))
            .await
            .map_err(|e| DownloadFail::other(format!("Failed to seek cache file: {e}")))?;
    } else {
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| DownloadFail::other(format!("Failed to seek cache file: {e}")))?;
    }

    Ok(file)
}

async fn stream_into(
    file: &mut async_fs::File,
    response: &mut reqwest::Response,
    mut written: u64,
) -> Result<u64, DownloadFail> {
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if let Err(e) = file.write_all(&chunk).await {
                    let _ = file.flush().await;
                    return Err(DownloadFail::other(format!(
                        "Failed to write cache file: {e}"
                    )));
                }
                written = written.saturating_add(chunk.len() as u64);
            }
            Ok(None) => {
                file.flush()
                    .await
                    .map_err(|e| DownloadFail::other(format!("Failed to flush cache file: {e}")))?;
                return Ok(written);
            }
            Err(e) => {
                let _ = file.flush().await;
                return Err(DownloadFail::other(format!("Failed to read response: {e}")));
            }
        }
    }
}

async fn promote_partial(part: &FsPath, meta: &FsPath, dest: &FsPath) -> Result<(), DownloadFail> {
    if let Err(e) = async_fs::rename(part, dest).await {
        if is_existing_file(dest).await {
            async_fs::remove_file(dest).await.map_err(|err| {
                DownloadFail::other(format!("Failed to replace cache file: {err}"))
            })?;
            async_fs::rename(part, dest).await.map_err(|err| {
                DownloadFail::other(format!("Failed to rename cache file: {err}"))
            })?;
        } else {
            return Err(DownloadFail::other(format!(
                "Failed to rename cache file: {e}"
            )));
        }
    }
    let _ = async_fs::remove_file(meta).await;
    Ok(())
}

async fn head_content_length(client: &reqwest::Client, url: &str) -> Option<u64> {
    let response = client.head(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.content_length()
}

fn partial_paths(cached_file: &FsPath) -> (PathBuf, PathBuf) {
    let file_name = cached_file.file_name().unwrap_or_default();
    let mut part_name = file_name.to_os_string();
    part_name.push(".part");
    let mut meta_name = file_name.to_os_string();
    meta_name.push(".part.meta");
    (
        cached_file.with_file_name(part_name),
        cached_file.with_file_name(meta_name),
    )
}

fn header_str(
    headers: &reqwest::header::HeaderMap,
    name: &reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn content_range_bounds(headers: &reqwest::header::HeaderMap) -> (Option<u64>, Option<u64>) {
    let Some(value) = headers
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    else {
        return (None, None);
    };
    let Some(spec) = value.trim().strip_prefix("bytes") else {
        return (None, None);
    };
    let Some((range, total_text)) = spec.trim().split_once('/') else {
        return (None, None);
    };
    let total = match total_text.trim() {
        "*" => None,
        text => text.parse().ok(),
    };
    let start = if range.trim() == "*" {
        None
    } else {
        range
            .split_once('-')
            .and_then(|(start, _)| start.trim().parse().ok())
    };
    (start, total)
}

fn format_validator(etag: Option<&str>, last_modified: Option<&str>) -> Option<String> {
    if let Some(etag) = nonempty(etag) {
        Some(format!("etag: {etag}\n"))
    } else {
        nonempty(last_modified).map(|value| format!("last-modified: {value}\n"))
    }
}

fn parse_validator(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line
            .strip_prefix("etag: ")
            .or_else(|| line.strip_prefix("last-modified: "))
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

async fn read_validator(path: &FsPath) -> Option<String> {
    let text = async_fs::read_to_string(path).await.ok()?;
    parse_validator(&text)
}

async fn write_validator(path: &FsPath, etag: Option<&str>, last_modified: Option<&str>) {
    let Some(content) = format_validator(etag, last_modified) else {
        let _ = async_fs::remove_file(path).await;
        return;
    };
    if let Err(e) = async_fs::write(path, content).await {
        warn!("[Black Hole] Failed to write resume metadata: {}", e);
    }
}

async fn serve_file(path: &FsPath, file_path: &str, request_headers: &HeaderMap) -> Response {
    let len = match async_fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(e) => {
            error!("[Black Hole] Failed to read file: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {e}"),
            )
                .into_response();
        }
    };

    let range_header = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    match resolve_range(range_header, len) {
        RangeOutcome::Full => serve_full(path, file_path).await,
        RangeOutcome::Unsatisfiable => unsatisfiable(len),
        RangeOutcome::Single { start, end } => serve_range(path, file_path, start, end, len).await,
    }
}

async fn serve_full(path: &FsPath, file_path: &str) -> Response {
    match async_fs::read(path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();
            set_content_type(&mut headers, file_path);
            headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
            (StatusCode::OK, headers, content).into_response()
        }
        Err(e) => {
            error!("[Black Hole] Failed to read file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {e}"),
            )
                .into_response()
        }
    }
}

async fn serve_range(path: &FsPath, file_path: &str, start: u64, end: u64, total: u64) -> Response {
    let range_len = end - start + 1;
    let Ok(buf_len) = usize::try_from(range_len) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Range is too large".to_string(),
        )
            .into_response();
    };

    let mut file = match async_fs::File::open(path).await {
        Ok(file) => file,
        Err(e) => {
            error!("[Black Hole] Failed to read file: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(start)).await {
        error!("[Black Hole] Failed to read file: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read file: {e}"),
        )
            .into_response();
    }

    let mut buf = vec![0u8; buf_len];
    if let Err(e) = file.read_exact(&mut buf).await {
        error!("[Black Hole] Failed to read file: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read file: {e}"),
        )
            .into_response();
    }

    info!(
        "[Black Hole] Returning partial content bytes {}-{}/{}",
        start, end, total
    );

    match Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, content_type_for(file_path))
        .header(header::CONTENT_LENGTH, range_len.to_string())
        .header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .body(axum::body::Body::from(buf))
    {
        Ok(response) => response,
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to build response: {e}"),
        )
            .into_response(),
    }
}

fn unsatisfiable(len: u64) -> Response {
    match Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{len}"))
        .body(axum::body::Body::from(Vec::<u8>::new()))
    {
        Ok(response) => response,
        Err(_) => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
    }
}

fn resolve_range(range_header: Option<&str>, len: u64) -> RangeOutcome {
    let Some(raw) = range_header
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return RangeOutcome::Full;
    };
    if raw.contains(',') {
        return RangeOutcome::Full;
    }
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeOutcome::Unsatisfiable;
    };
    let spec = spec.trim();
    if spec.is_empty() {
        return RangeOutcome::Unsatisfiable;
    }

    if let Some(suffix) = spec.strip_prefix('-') {
        let Ok(suffix_len) = suffix.trim().parse::<u64>() else {
            return RangeOutcome::Unsatisfiable;
        };
        if suffix_len == 0 || len == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        let suffix_len = suffix_len.min(len);
        return RangeOutcome::Single {
            start: len - suffix_len,
            end: len - 1,
        };
    }

    let Some((start_text, end_text)) = spec.split_once('-') else {
        return RangeOutcome::Unsatisfiable;
    };
    if start_text.trim().is_empty() {
        return RangeOutcome::Unsatisfiable;
    }
    let Ok(start) = start_text.trim().parse::<u64>() else {
        return RangeOutcome::Unsatisfiable;
    };
    if len == 0 || start >= len {
        return RangeOutcome::Unsatisfiable;
    }
    let end = if end_text.trim().is_empty() {
        len - 1
    } else {
        match end_text.trim().parse::<u64>() {
            Ok(end) => end,
            Err(_) => return RangeOutcome::Unsatisfiable,
        }
    };
    if end < start {
        return RangeOutcome::Unsatisfiable;
    }
    RangeOutcome::Single {
        start,
        end: end.min(len - 1),
    }
}

async fn is_existing_file(path: &FsPath) -> bool {
    match async_fs::metadata(path).await {
        Ok(meta) => meta.is_file(),
        Err(_) => false,
    }
}

async fn file_len(path: &FsPath) -> u64 {
    async_fs::metadata(path)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0)
}

fn content_type_for(file_path: &str) -> &'static str {
    let path_buf = PathBuf::from(file_path);
    let ext = path_buf.extension().and_then(|s| s.to_str()).unwrap_or("");

    match ext {
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "html" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn set_content_type(headers: &mut HeaderMap, file_path: &str) {
    headers.insert(
        header::CONTENT_TYPE,
        content_type_for(file_path).parse().unwrap(),
    );
}

/// 检查路径是否安全，防止目录遍历攻击
fn is_safe_path(path: &str) -> bool {
    // 检查是否包含危险字符
    if path.contains("..") || path.contains("//") || path.contains('\\') {
        return false;
    }

    // 允许以@开头的scoped npm包名（如@highlightjs/cdn-assets）
    // 但不允许其他以/开头的绝对路径
    if path.starts_with('/') && !path.starts_with("@") {
        return false;
    }

    // 检查是否为绝对路径
    if PathBuf::from(path).is_absolute() {
        return false;
    }

    // 检查路径组件
    for component in PathBuf::from(path).components() {
        match component {
            std::path::Component::ParentDir => return false,
            std::path::Component::RootDir => return false,
            std::path::Component::Prefix(_) => return false,
            _ => {}
        }
    }

    true
}

/// 验证路径是否在允许的目录范围内
fn is_path_within_allowed_dirs(target_path: &PathBuf, allowed_dir: &str) -> bool {
    let allowed_path = match fs::canonicalize(allowed_dir) {
        Ok(path) => path,
        Err(_) => return false,
    };

    let target_canonical = match target_path.canonicalize() {
        Ok(path) => path,
        Err(_) => {
            // 如果文件不存在，检查其父目录
            let parent = target_path.parent().unwrap_or(target_path);
            match parent.canonicalize() {
                Ok(path) => path,
                Err(_) => return false,
            }
        }
    };

    target_canonical.starts_with(&allowed_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_requests() {
        assert_eq!(resolve_range(None, 100), RangeOutcome::Full);
        assert_eq!(resolve_range(Some(""), 100), RangeOutcome::Full);
        assert_eq!(
            resolve_range(Some("bytes=0-0"), 100),
            RangeOutcome::Single { start: 0, end: 0 }
        );
        assert_eq!(
            resolve_range(Some("bytes=0-"), 100),
            RangeOutcome::Single { start: 0, end: 99 }
        );
        assert_eq!(
            resolve_range(Some("bytes=-10"), 100),
            RangeOutcome::Single { start: 90, end: 99 }
        );
        assert_eq!(
            resolve_range(Some("bytes=-1000"), 100),
            RangeOutcome::Single { start: 0, end: 99 }
        );
        assert_eq!(
            resolve_range(Some("bytes=0-999"), 100),
            RangeOutcome::Single { start: 0, end: 99 }
        );
        assert_eq!(
            resolve_range(Some("bytes=0-1,2-3"), 100),
            RangeOutcome::Full
        );
        assert_eq!(
            resolve_range(Some("bytes=100-"), 100),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            resolve_range(Some("bytes=10-5"), 100),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            resolve_range(Some("bytes=-0"), 100),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            resolve_range(Some("items=0-1"), 100),
            RangeOutcome::Unsatisfiable
        );
        assert_eq!(
            resolve_range(Some("bytes=0-0"), 0),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn content_range_and_partial_names() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 10-19/100".parse().unwrap(),
        );
        assert_eq!(content_range_bounds(&headers), (Some(10), Some(100)));

        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 10-19/*".parse().unwrap(),
        );
        assert_eq!(content_range_bounds(&headers), (Some(10), None));

        let cached = PathBuf::from("cache")
            .join("vue")
            .join("3.2.0")
            .join("dist")
            .join("vue.global.min.js");
        let (part, meta) = partial_paths(&cached);
        assert_eq!(
            part.file_name().and_then(|name| name.to_str()),
            Some("vue.global.min.js.part")
        );
        assert_eq!(
            meta.file_name().and_then(|name| name.to_str()),
            Some("vue.global.min.js.part.meta")
        );
        assert_eq!(part.parent(), cached.parent());
    }

    #[test]
    fn resume_defaults_to_false_when_missing() {
        let config: Config = toml::from_str(
            r#"
            [proxy]
            enabled = false
            static_dir = "./static"
            cache_dir = "./cache"

            [log]
            enabled = true
            level = "info"

            [server]
            port = 8080
            host = "localhost"
            "#,
        )
        .unwrap();
        assert!(!config.proxy.resume);
    }

    #[tokio::test]
    async fn serve_file_honors_single_range() {
        let dir = std::env::temp_dir().join(format!("blackhole-range-{}", std::process::id()));
        let _ = async_fs::remove_dir_all(&dir).await;
        async_fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("hello.txt");
        async_fs::write(&path, b"hello world").await.unwrap();

        let response = serve_file(&path, "hello.txt", &HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello world");

        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=0-4".parse().unwrap());
        let response = serve_file(&path, "hello.txt", &headers).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 0-4/11"
        );
        assert_eq!(response.headers().get(header::CONTENT_LENGTH).unwrap(), "5");
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello");

        headers.insert(header::RANGE, "bytes=6-".parse().unwrap());
        let response = serve_file(&path, "hello.txt", &headers).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"world");

        headers.insert(header::RANGE, "bytes=-5".parse().unwrap());
        let response = serve_file(&path, "hello.txt", &headers).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"world");

        headers.insert(header::RANGE, "bytes=100-".parse().unwrap());
        let response = serve_file(&path, "hello.txt", &headers).await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes */11"
        );

        headers.insert(header::RANGE, "bytes=0-1,2-3".parse().unwrap());
        let response = serve_file(&path, "hello.txt", &headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello world");

        let _ = async_fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn validator_prefers_etag() {
        let text =
            format_validator(Some("\"abc\""), Some("Wed, 21 Oct 2015 07:28:00 GMT")).unwrap();
        assert_eq!(parse_validator(&text).as_deref(), Some("\"abc\""));

        let text = format_validator(None, Some("Wed, 21 Oct 2015 07:28:00 GMT")).unwrap();
        assert_eq!(
            parse_validator(&text).as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        assert!(format_validator(None, Some("  ")).is_none());
    }
}
