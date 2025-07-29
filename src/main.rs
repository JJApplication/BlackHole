use axum::{
    extract::{Path, State},
    http::HeaderMap,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use regex::Regex;
use serde::Deserialize;
use std::{fs, path::PathBuf};
use tokio::fs as async_fs;
use tower_http::trace::TraceLayer;
use tracing::{info, warn, error};

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
    index_cache: std::sync::Arc<tokio::sync::RwLock<Option<String>>>,
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
        
        tracing_subscriber::fmt()
            .with_max_level(level)
            .init();
            
        info!("[Black Hole] Configuration loaded successfully: {:?}", config);
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
        index_cache: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
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
    info!("[Black Hole] Proxy feature status: {}", config.proxy.enabled);

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
                axum::http::header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            return (StatusCode::OK, headers, cached_content.clone()).into_response();
        }
    }

    // 缓存中没有，从文件读取
    let index_path = PathBuf::from("ui").join("index.html");
    info!("[Black Hole] Reading index.html from file: {:?}", index_path);

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
                axum::http::header::CONTENT_TYPE,
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
) -> impl IntoResponse {
    let request_path = format!("/static/{}", path);
    info!("[Black Hole] Received request: {}", request_path);

    // 检查是否为unpkg格式
    if let Some(captures) = state.unpkg_regex.captures(&request_path) {
        let package_name = captures.get(1).unwrap().as_str();
        let version = captures.get(2).unwrap().as_str();
        let file_path = captures.get(3).unwrap().as_str();
        
        return handle_unpkg_request(&state, package_name, version, file_path).await;
    }

    // 本地静态文件请求
    handle_local_static_request(&state, &path).await
}

async fn handle_local_static_request(
    state: &AppState,
    file_path: &str,
) -> Response {
    // 安全路径验证
    if !is_safe_path(file_path) {
        warn!("[Black Hole] Detected unsafe path access: {}", file_path);
        return (StatusCode::FORBIDDEN, "Forbidden: Unsafe path").into_response();
    }
    
    let local_path = PathBuf::from(&state.config.proxy.static_dir).join(file_path);
    
    // 验证解析后的路径是否在允许的目录内
    if !is_path_within_allowed_dirs(&local_path, &state.config.proxy.static_dir) {
        warn!("[Black Hole] Detected directory traversal attack: {:?}", local_path);
        return (StatusCode::FORBIDDEN, "Forbidden: Outside allowed directory range").into_response();
    }
    
    info!("[Black Hole] Looking for local file: {:?}", local_path);

    match async_fs::read(&local_path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();
            set_content_type(&mut headers, file_path);
            
            info!("[Black Hole] Successfully returned local file: {}", file_path);
            (StatusCode::OK, headers, content).into_response()
        }
        Err(_) => {
            warn!("[Black Hole] File not found: {}", file_path);
            (StatusCode::NOT_FOUND, format!("File not found: {}", file_path)).into_response()
        }
    }
}

async fn handle_unpkg_request(
    state: &AppState,
    package_name: &str,
    version: &str,
    file_path: &str,
) -> Response {
    // 构建缓存路径，去掉版本号前的@符号以兼容Windows文件系统
    let safe_version = version.trim_start_matches('@');
    let cache_dir = PathBuf::from(&state.config.proxy.cache_dir)
        .join(package_name)
        .join(safe_version);
    let cached_file = cache_dir.join(file_path);

    info!("[Black Hole] Checking cache file: {:?}", cached_file.display());

    // 检查缓存是否存在
    if let Ok(content) = async_fs::read(&cached_file).await {
        info!("[Black Hole] Using cached file: {:?}", cached_file);
        let mut headers = HeaderMap::new();
        set_content_type(&mut headers, file_path);
        return (StatusCode::OK, headers, content).into_response();
    }

    if !state.config.proxy.enabled {
        return (StatusCode::SERVICE_UNAVAILABLE, "Proxy service not enabled").into_response();
    }

    // 从unpkg下载文件
    let unpkg_url = format!("https://unpkg.com/{}@{}/{}", package_name, version, file_path);
    info!("[Black Hole] Downloading from unpkg: {}", unpkg_url);

    match state.client.get(&unpkg_url).send().await {
        Ok(response) => {
            if !response.status().is_success() {
                let status = response.status();
                error!("[Black Hole] unpkg returned error: {}", status);
                return (StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), format!("unpkg returned error: {}", status)).into_response();
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
                    if let Err(e) = async_fs::write(&cached_file, &content).await {
                        warn!("[Black Hole] Failed to save cache file: {}", e);
                    }

                    let mut headers = HeaderMap::new();
                    set_content_type(&mut headers, file_path);
                    
                    info!("[Black Hole] Successfully downloaded and cached file: {}", file_path);
                    (StatusCode::OK, headers, content.to_vec()).into_response()
                }
                Err(e) => {
                    error!("[Black Hole] Failed to read response: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read response: {}", e)).into_response()
                }
            }
        }
        Err(e) => {
            error!("[Black Hole] Download failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Download failed: {}", e)).into_response()
        }
    }
}

fn set_content_type(headers: &mut HeaderMap, file_path: &str) {
    let path_buf = PathBuf::from(file_path);
    let ext = path_buf
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let content_type = match ext {
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "html" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    };

    headers.insert(
        axum::http::header::CONTENT_TYPE,
        content_type.parse().unwrap(),
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