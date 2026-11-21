//! HGhub 网页版服务。
//!
//! 复用 `src-tauri/src` 里的 Rust 核心模块（业务接口、封面解密、HLS 改写与分片代理、线路与安全策略），
//! 不改一行，靠 `shim/` 里的 tauri 替身编译。前端原来的 `invoke(cmd, args)` 改走
//! `POST /api/invoke/<cmd>`，原来的 `hg://` 媒体协议改走 `GET /media/{img,hls,seg}?u=`。
//! 所有页面、接口与媒体都在密码登录门之后。

#[path = "../../src-tauri/src/api.rs"]
#[allow(dead_code)]
mod api;
#[path = "../../src-tauri/src/lines.rs"]
#[allow(dead_code)]
mod lines;
#[path = "../../src-tauri/src/logging.rs"]
#[allow(dead_code)]
mod logging;
#[path = "../../src-tauri/src/media.rs"]
#[allow(dead_code)]
mod media;
#[path = "../../src-tauri/src/net.rs"]
#[allow(dead_code)]
mod net;
#[path = "../../src-tauri/src/network_policy.rs"]
#[allow(dead_code)]
mod network_policy;
#[path = "../../src-tauri/src/playback_log.rs"]
#[allow(dead_code)]
mod playback_log;
#[path = "../../src-tauri/src/session.rs"]
#[allow(dead_code)]
mod session;

mod auth;
mod download;

use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, Path, RawQuery, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tower_http::services::{ServeDir, ServeFile};

/// 媒体代理挂载点；m3u8 内的分片与 key 会被改写为 `/media/seg?u=…`（同源相对地址）。
const MEDIA_PREFIX: &str = "/media";

pub struct App {
    api: api::Api,
    data_dir: PathBuf,
    auth: auth::Auth,
    downloads: Arc<download::Downloads>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() {
    let data_dir = PathBuf::from(env_or("HGHUB_DATA", "/var/lib/hghub-web"));
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set-password") {
        std::process::exit(match auth::set_password_interactive(&data_dir) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("设置失败：{e}");
                1
            }
        });
    }

    let bind = env_or("HGHUB_BIND", "127.0.0.1:8900");
    let static_dir = PathBuf::from(env_or("HGHUB_STATIC", "/opt/hghub-web/dist"));
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("无法创建数据目录 {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    let auth = match auth::Auth::load(&data_dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    // 与 src-tauri/src/lib.rs 的 setup 一致：恢复线路、安全模式、诊断日志、账号会话
    net::bind_base(&data_dir);
    playback_log::bind(&data_dir);
    let downloads = Arc::new(download::Downloads::new(&data_dir));
    let app = Arc::new(App { api: api::Api::new(), data_dir: data_dir.clone(), auth, downloads });
    app.api.session.bind(data_dir.clone());

    let index = static_dir.join("index.html");
    let spa = ServeDir::new(&static_dir).fallback(ServeFile::new(&index));

    let gated = Router::new()
        .route("/api/invoke/{cmd}", post(invoke))
        .route("/media/{kind}", get(media_proxy))
        .route("/api/download/start", post(download_start))
        .route("/api/download/status/{job}", get(download_status))
        .route("/download/{job}", get(download_file))
        .route("/logout", post(logout))
        .fallback_service(spa)
        .layer(middleware::from_fn_with_state(app.clone(), require_login));

    let router = Router::new()
        .route("/login", get(login_page).post(login_submit))
        .merge(gated)
        .layer(middleware::from_fn(security_headers))
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("无法监听 {bind}: {e}");
            std::process::exit(1);
        }
    };
    println!("hghub-web 监听 http://{bind}  静态目录 {}  数据目录 {}", static_dir.display(), data_dir.display());
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
            tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        })
        .await
        .expect("server");
}

/* ───────────────────────── 安全响应头 ───────────────────────── */

async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    h.insert("Referrer-Policy", "no-referrer".parse().unwrap());
    h.insert("X-Frame-Options", "DENY".parse().unwrap());
    resp
}

/* ───────────────────────── 登录门 ───────────────────────── */

async fn require_login(State(app): State<Arc<App>>, headers: HeaderMap, req: Request, next: Next) -> Response {
    if app.auth.check_cookie(&headers) {
        // 同源校验：带 Origin 的写请求必须来自本站
        if req.method() != axum::http::Method::GET {
            if let (Some(origin), Some(host)) = (
                headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
                headers.get(header::HOST).and_then(|v| v.to_str().ok()),
            ) {
                let same = url::Url::parse(origin).ok().is_some_and(|u| {
                    let o = match u.port() { Some(p) => format!("{}:{p}", u.host_str().unwrap_or("")), None => u.host_str().unwrap_or("").to_string() };
                    o.eq_ignore_ascii_case(host)
                });
                if !same {
                    return (StatusCode::FORBIDDEN, "origin denied").into_response();
                }
            }
        }
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path.starts_with("/api/") || path.starts_with("/media/") || path == "/logout" {
        return (StatusCode::UNAUTHORIZED, Json(json!("未登录"))).into_response();
    }
    Redirect::to("/login").into_response()
}

#[derive(serde::Deserialize)]
struct LoginQuery {
    e: Option<String>,
}

async fn login_page(State(app): State<Arc<App>>, headers: HeaderMap, q: axum::extract::Query<LoginQuery>) -> Response {
    if app.auth.check_cookie(&headers) {
        return Redirect::to("/").into_response();
    }
    let msg = match q.e.as_deref() {
        Some("1") => "密码不正确",
        Some("2") => "尝试次数过多，请 15 分钟后再试",
        _ => "",
    };
    let html = auth::LOGIN_HTML.replace("{{MSG}}", msg);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")],
        html,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Response {
    match app.auth.login(peer.ip(), &form.password).await {
        auth::LoginResult::Ok(cookie) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/".to_string()), (header::SET_COOKIE, cookie)],
        )
            .into_response(),
        auth::LoginResult::Wrong => Redirect::to("/login?e=1").into_response(),
        auth::LoginResult::Locked => Redirect::to("/login?e=2").into_response(),
    }
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    app.auth.revoke(&headers);
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/login".to_string()), (header::SET_COOKIE, auth::clear_cookie())],
    )
        .into_response()
}

/* ───────────────────────── 媒体代理（替代 hg:// 协议） ───────────────────────── */

async fn media_proxy(Path(kind): Path<String>, RawQuery(query): RawQuery) -> Response {
    let path = format!("/{kind}");
    let resp = media::handle(&path, query.as_deref().unwrap_or(""), MEDIA_PREFIX).await;
    resp.map(Body::from)
}

/* ───────────────────────── 下载本集 ───────────────────────── */

#[derive(serde::Deserialize)]
struct DownloadReq {
    id: i64,
    ep: u32,
}

async fn download_start(State(app): State<Arc<App>>, Json(r): Json<DownloadReq>) -> Response {
    match app.downloads.start(app.clone(), r.id, r.ep) {
        Ok(job) => Json(json!({ "job": job })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn download_status(State(app): State<Arc<App>>, Path(job): Path<String>) -> Response {
    match app.downloads.status(&job) {
        Some(s) => ([(header::CACHE_CONTROL, "no-store")], Json(s)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!("下载任务不存在或已过期"))).into_response(),
    }
}

async fn download_file(State(app): State<Arc<App>>, Path(job): Path<String>) -> Response {
    let Some((path, name)) = app.downloads.file(&job) else {
        return (StatusCode::NOT_FOUND, "下载任务不存在、未完成或已过期").into_response();
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "文件已清理").into_response(),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let encoded = percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC).to_string();
    (
        [
            (header::CONTENT_TYPE, "video/mp4".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"episode.mp4\"; filename*=UTF-8''{encoded}")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response()
}

/* ───────────────────────── invoke → HTTP ───────────────────────── */

fn arg<T: DeserializeOwned>(a: &Value, key: &str) -> Result<T, String> {
    serde_json::from_value(a.get(key).cloned().unwrap_or(Value::Null)).map_err(|e| format!("参数 {key} 无效：{e}"))
}

fn to_json<T: serde::Serialize>(r: Result<T, String>) -> Result<Value, String> {
    r.map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
}

async fn invoke(State(app): State<Arc<App>>, Path(cmd): Path<String>, body: Bytes) -> Response {
    let a: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).unwrap_or(json!({})) };
    let st = || tauri::State(&app.api);
    let handle = || tauri::AppHandle { data_dir: app.data_dir.clone() };

    macro_rules! call {
        ($fut:expr) => {
            to_json($fut.await)
        };
    }
    // 参数解析失败也走统一错误返回
    macro_rules! p {
        ($k:literal) => {
            match arg(&a, $k) {
                Ok(v) => v,
                Err(e) => return err_response(e),
            }
        };
    }

    let result: Result<Value, String> = match cmd.as_str() {
        "app_boot" => call!(api::app_boot(st())),
        "security_set" => call!(api::security_set(p!("strict"))),
        "debug_log" => call!(api::debug_log(handle(), p!("msg"))),
        "net_diag" => call!(api::net_diag()),
        "line_state" => call!(api::line_state()),
        "line_set" => call!(api::line_set(st(), p!("url"))),
        "line_mirrors" => call!(api::line_mirrors()),
        "line_probe" => call!(api::line_probe(p!("urls"))),
        "check_update" => call!(api::check_update()),
        // 网页版由前端适配层直接 window.open；方向切换由浏览器自身处理
        "open_url" => Ok(Value::Null),
        "set_orientation" => call!(api::set_orientation(handle(), p!("landscape"))),
        "api_login" => call!(api::api_login(st(), p!("username"), p!("password"))),
        "api_register" => call!(api::api_register(st(), p!("username"), p!("password"), arg(&a, "email").unwrap_or_default())),
        "api_logout" => call!(api::api_logout(st())),
        "api_me" => call!(api::api_me(st())),
        "api_feed" => call!(api::api_feed(st(), p!("page"), p!("size"))),
        "api_category" => call!(api::api_category(st(), p!("cate"), p!("page"), p!("size"))),
        "api_rank" => call!(api::api_rank(st(), p!("kind"))),
        "api_search" => call!(api::api_search(st(), p!("q"), p!("page"), p!("size"))),
        "cache_clear" => call!(api::cache_clear()),
        "api_posts" => call!(api::api_posts(st(), p!("page"), p!("size"))),
        "api_post" => call!(api::api_post(st(), p!("id"))),
        "api_post_like" => call!(api::api_post_like(st(), p!("id"), p!("on"))),
        "api_post_collect" => call!(api::api_post_collect(st(), p!("id"), p!("on"))),
        "api_post_comments" => call!(api::api_post_comments(st(), p!("id"), p!("page"), p!("size"))),
        "api_video" => call!(api::api_video(st(), p!("id"))),
        "api_play" => call!(api::api_play(st(), p!("id"), p!("ep"))),
        "api_comments" => call!(api::api_comments(st(), p!("id"), p!("ep"), p!("page"), p!("size"))),
        "api_comment_send" => call!(api::api_comment_send(st(), p!("id"), p!("ep"), p!("text"), arg(&a, "replyId").unwrap_or(None))),
        "api_like" => call!(api::api_like(st(), p!("id"), p!("ep"), p!("on"))),
        "api_collect" => call!(api::api_collect(st(), p!("id"), p!("on"))),
        "api_follow" => call!(api::api_follow(st(), p!("uid"), p!("on"))),
        "playback_log_status" => to_json(playback_log::playback_log_status()),
        "playback_log_set" => to_json(playback_log::playback_log_set(p!("enabled"))),
        "playback_log_write" => to_json(playback_log::playback_log_write(p!("entries"))),
        "playback_log_read" => to_json(playback_log::playback_log_read()),
        _ => return (StatusCode::NOT_FOUND, Json(json!(format!("未知命令 {cmd}")))).into_response(),
    };
    match result {
        Ok(v) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Err(e) => err_response(e),
    }
}

fn err_response(e: String) -> Response {
    (StatusCode::BAD_REQUEST, [(header::CACHE_CONTROL, "no-store")], Json(json!(e))).into_response()
}
