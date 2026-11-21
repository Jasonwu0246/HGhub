//! 「下载本集」：把一集 HLS 的分片全部经媒体代理取回，交给 ffmpeg 解密并无损封装成一个 MP4，
//! 供 iPhone Safari 存到「文件」后离线播放。
//!
//! 取流完全复用 media::handle（与在线播放同一套防盗链、线路与安全策略），这里只做编排：
//! 解析清单 → 并发取分片与 Key → 写本地清单 → ffmpeg -c copy → ffprobe 核对时长。

use crate::{api, media, MEDIA_PREFIX};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_RUNNING: usize = 2;
const KEEP_SECS: u64 = 60 * 60;
const FETCH_CONCURRENCY: usize = 4;
const FETCH_RETRIES: usize = 3;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, Serialize)]
pub struct Status {
    /// running → muxing → done / error
    pub state: &'static str,
    pub done: usize,
    pub total: usize,
    pub filename: String,
    pub bytes: u64,
    /// ffprobe 读出的成片时长（秒）
    pub duration: f64,
    /// 清单 EXTINF 累计时长（秒），用来核对成片是否完整
    pub expected: f64,
    pub error: Option<String>,
    #[serde(skip)]
    pub created: u64,
}

pub struct Downloads {
    dir: PathBuf,
    jobs: Mutex<HashMap<String, Arc<Mutex<Status>>>>,
}

impl Downloads {
    pub fn new(data_dir: &Path) -> Self {
        let dir = data_dir.join("downloads");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, jobs: Mutex::new(HashMap::new()) }
    }

    fn cleanup(&self) {
        let t = now();
        let mut jobs = self.jobs.lock().unwrap();
        let stale: Vec<String> = jobs
            .iter()
            .filter(|(_, s)| {
                let s = s.lock().unwrap();
                s.state != "running" && s.state != "muxing" && t.saturating_sub(s.created) > KEEP_SECS
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            jobs.remove(&k);
            let _ = std::fs::remove_dir_all(self.dir.join(&k));
        }
    }

    pub fn status(&self, job: &str) -> Option<Status> {
        self.jobs.lock().unwrap().get(job).map(|s| s.lock().unwrap().clone())
    }

    /// 成片路径与文件名（仅 done 状态）
    pub fn file(&self, job: &str) -> Option<(PathBuf, String)> {
        let s = self.status(job)?;
        (s.state == "done").then(|| (self.dir.join(job).join("out.mp4"), s.filename))
    }

    pub fn start(self: &Arc<Self>, app: Arc<crate::App>, id: i64, ep: u32) -> Result<String, String> {
        self.cleanup();
        // 同一集已在做或已做好：直接复用
        let key = format!("{id}-{ep}");
        {
            let jobs = self.jobs.lock().unwrap();
            if let Some(s) = jobs.get(&key) {
                if s.lock().unwrap().state != "error" {
                    return Ok(key);
                }
            }
            let running = jobs.values().filter(|s| matches!(s.lock().unwrap().state, "running" | "muxing")).count();
            if running >= MAX_RUNNING {
                return Err("同时最多处理 2 集，请等当前下载完成".into());
            }
        }
        let status = Arc::new(Mutex::new(Status {
            state: "running",
            done: 0,
            total: 0,
            filename: format!("{id}-{ep}.mp4"),
            bytes: 0,
            duration: 0.0,
            expected: 0.0,
            error: None,
            created: now(),
        }));
        self.jobs.lock().unwrap().insert(key.clone(), status.clone());
        let work = self.dir.join(&key);
        tokio::spawn(async move {
            let _ = std::fs::remove_dir_all(&work);
            let r = run(app, id, ep, &work, status.clone()).await;
            let mut s = status.lock().unwrap();
            s.created = now();
            match r {
                Ok(()) => s.state = "done",
                Err(e) => {
                    s.state = "error";
                    s.error = Some(e);
                    let _ = std::fs::remove_dir_all(&work);
                }
            }
        });
        Ok(key)
    }
}

/// 经媒体代理取一个已改写的地址（形如 `/media/seg?u=…`），返回正文
async fn fetch(local: &str) -> Result<Vec<u8>, String> {
    let rest = local.strip_prefix(MEDIA_PREFIX).ok_or_else(|| format!("清单地址异常：{local}"))?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut last = String::new();
    for attempt in 0..FETCH_RETRIES {
        let resp = media::handle(path, query, MEDIA_PREFIX).await;
        if resp.status().is_success() {
            return Ok(resp.into_body());
        }
        last = format!("HTTP {}", resp.status().as_u16());
        tokio::time::sleep(std::time::Duration::from_millis(600 * (attempt as u64 + 1))).await;
    }
    Err(format!("取流失败（{last}）"))
}

fn attr<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let i = line.find(&format!("{name}=\""))? + name.len() + 2;
    let rest = &line[i..];
    Some(&rest[..rest.find('"')?])
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { ' ' } else { c })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(60).collect()
}

async fn run(app: Arc<crate::App>, id: i64, ep: u32, work: &Path, status: Arc<Mutex<Status>>) -> Result<(), String> {
    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;

    let title = api::api_video(tauri::State(&app.api), id)
        .await
        .ok()
        .and_then(|v| serde_json::to_value(v).ok())
        .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("剧集{id}"));
    let play = serde_json::to_value(api::api_play(tauri::State(&app.api), id, ep).await?).map_err(|e| e.to_string())?;
    let url = play.get("video_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or("没有播放地址")?.to_string();
    status.lock().unwrap().filename = format!("{} 第{ep}集.mp4", sanitize(&title));

    // 主清单 → 选码率最高的子清单
    let mut text = String::from_utf8_lossy(&fetch(&format!("{MEDIA_PREFIX}/hls?u={}", enc(&url))).await?).into_owned();
    if text.contains("#EXT-X-STREAM-INF") {
        let mut best: Option<(u64, String)> = None;
        let mut bw = 0u64;
        for line in text.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                bw = rest.split(',').find_map(|kv| kv.trim().strip_prefix("BANDWIDTH=")).and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if !line.is_empty() && !line.starts_with('#') {
                if best.as_ref().is_none_or(|(b, _)| bw > *b) {
                    best = Some((bw, line.to_string()));
                }
            }
        }
        let (_, variant) = best.ok_or("主清单里没有子清单")?;
        text = String::from_utf8_lossy(&fetch(&variant).await?).into_owned();
    }
    if !text.starts_with("#EXTM3U") {
        return Err("播放清单格式异常".into());
    }

    // 重写为本地清单：分片 / Key / 初始化段都落到工作目录
    let mut out = String::new();
    let mut jobs: Vec<(String, PathBuf)> = Vec::new();
    let mut expected = 0.0f64;
    let mut seg_no = 0usize;
    let mut aux_no = 0usize;
    // fMP4 分片（带 EXT-X-MAP）与 TS 分片用各自的扩展名，ffmpeg 7 会按扩展名校验格式
    let seg_ext = if text.contains("#EXT-X-MAP") { "m4s" } else { "ts" };
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXT-X-KEY") || line.starts_with("#EXT-X-MAP") {
            if let Some(uri) = attr(line, "URI") {
                aux_no += 1;
                let name = format!("aux{aux_no}.bin");
                jobs.push((uri.to_string(), work.join(&name)));
                out.push_str(&line.replacen(uri, &name, 1));
                out.push('\n');
                continue;
            }
        }
        if let Some(d) = line.strip_prefix("#EXTINF:") {
            expected += d.split(',').next().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        }
        if line.starts_with('#') {
            out.push_str(line);
        } else {
            seg_no += 1;
            let name = format!("s{seg_no:05}.{seg_ext}");
            jobs.push((line.to_string(), work.join(&name)));
            out.push_str(&name);
        }
        out.push('\n');
    }
    if seg_no == 0 {
        return Err("清单里没有视频分片".into());
    }
    {
        let mut s = status.lock().unwrap();
        s.total = jobs.len();
        s.expected = expected;
    }
    std::fs::write(work.join("local.m3u8"), out).map_err(|e| e.to_string())?;

    // 并发取回
    let jobs = Arc::new(jobs);
    let next = Arc::new(AtomicUsize::new(0));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..FETCH_CONCURRENCY {
        let (jobs, next, status) = (jobs.clone(), next.clone(), status.clone());
        set.spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((src, dst)) = jobs.get(i) else { return Ok::<(), String>(()) };
                let body = fetch(src).await?;
                tokio::fs::write(dst, &body).await.map_err(|e| e.to_string())?;
                status.lock().unwrap().done += 1;
            }
        });
    }
    while let Some(r) = set.join_next().await {
        if let Err(e) = r.map_err(|e| e.to_string()).and_then(|x| x) {
            set.abort_all();
            return Err(e);
        }
    }

    // 解密 + 无损封装
    status.lock().unwrap().state = "muxing";
    let mp4 = work.join("out.mp4");
    let ff = tokio::process::Command::new("ffmpeg")
        .current_dir(work)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-allowed_extensions", "ALL", "-protocol_whitelist", "file,crypto"])
        .args(["-i", "local.m3u8", "-c", "copy", "-movflags", "+faststart", "out.mp4"])
        .output()
        .await
        .map_err(|e| format!("无法运行 ffmpeg（服务器是否已安装？）：{e}"))?;
    if !ff.status.success() {
        let msg = String::from_utf8_lossy(&ff.stderr);
        return Err(format!("ffmpeg 失败：{}", msg.lines().last().unwrap_or("")));
    }

    // 核对：成片时长应与清单一致
    let probe = tokio::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(&mp4)
        .output()
        .await
        .map_err(|e| format!("无法运行 ffprobe：{e}"))?;
    let duration: f64 = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap_or(0.0);
    if expected > 0.0 && (duration - expected).abs() > expected.max(10.0) * 0.03 + 2.0 {
        return Err(format!("成片时长 {duration:.0}s 与清单 {expected:.0}s 不符"));
    }
    let bytes = std::fs::metadata(&mp4).map(|m| m.len()).unwrap_or(0);

    // 清掉分片，只留成片
    if let Ok(rd) = std::fs::read_dir(work) {
        for e in rd.flatten() {
            if e.file_name() != "out.mp4" {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let mut s = status.lock().unwrap();
    s.duration = duration;
    s.bytes = bytes;
    Ok(())
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}
