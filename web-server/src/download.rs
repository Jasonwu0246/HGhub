//! 下载：单集，或把选中的多集合并成一个 MP4，供 iPhone Safari 存到「文件」后离线播放。
//!
//! 取流完全复用 media::handle（与在线播放同一套防盗链、线路与安全策略），这里只做编排：
//! 每集：解析清单 → 并发取分片与 Key → 写本地清单 → ffmpeg -c copy 解密封装 → ffprobe 核对时长；
//! 多集：逐集完成后核对各集规格一致 → ffmpeg concat 无损拼接并写入每集章节 → 核对总时长。

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
const MAX_EPISODES: usize = 200;
/// 成片保留时长：多集合并可能要很久，留足时间回来保存
const KEEP_SECS: u64 = 2 * 60 * 60;
const FETCH_CONCURRENCY: usize = 4;
const FETCH_RETRIES: usize = 3;
/// 每取一集前至少要有的剩余空间（分片 + 单集成片）
const MIN_FREE_PER_EP: u64 = 800 * 1024 * 1024;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, Serialize)]
pub struct Status {
    /// running → muxing → (merging) → done / error
    pub state: &'static str,
    /// 当前这一集的分片进度
    pub done: usize,
    pub total: usize,
    /// 多集进度：第几集（从 1 起）/ 共几集，以及当前集号
    pub ep_index: usize,
    pub ep_count: usize,
    pub cur_ep: u32,
    pub filename: String,
    pub bytes: u64,
    /// ffprobe 读出的成片时长（秒）
    pub duration: f64,
    /// 各集清单 EXTINF 累计时长之和（秒），用来核对成片是否完整
    pub expected: f64,
    pub error: Option<String>,
    #[serde(skip)]
    pub created: u64,
}

fn busy(state: &str) -> bool {
    matches!(state, "running" | "muxing" | "merging")
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
                !busy(s.state) && t.saturating_sub(s.created) > KEEP_SECS
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

    pub fn start(self: &Arc<Self>, app: Arc<crate::App>, id: i64, eps: Vec<u32>) -> Result<String, String> {
        self.cleanup();
        let mut eps = eps;
        eps.sort_unstable();
        eps.dedup();
        eps.retain(|e| *e > 0);
        if eps.is_empty() {
            return Err("没有选择剧集".into());
        }
        if eps.len() > MAX_EPISODES {
            return Err(format!("一次最多 {MAX_EPISODES} 集"));
        }
        let key = if eps.len() == 1 {
            format!("{id}-{}", eps[0])
        } else {
            use sha2::{Digest, Sha256};
            let list = eps.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let h: String = Sha256::digest(list.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("{id}-m{}-{}-{h}", eps[0], eps[eps.len() - 1])
        };
        {
            let jobs = self.jobs.lock().unwrap();
            if let Some(s) = jobs.get(&key) {
                if s.lock().unwrap().state != "error" {
                    return Ok(key);
                }
            }
            let running = jobs.values().filter(|s| busy(s.lock().unwrap().state)).count();
            if running >= MAX_RUNNING {
                return Err("同时最多处理 2 个下载，请等当前的完成".into());
            }
        }
        let status = Arc::new(Mutex::new(Status {
            state: "running",
            done: 0,
            total: 0,
            ep_index: 0,
            ep_count: eps.len(),
            cur_ep: eps[0],
            filename: format!("{key}.mp4"),
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
            let r = run(app, id, eps, &work, status.clone()).await;
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

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// 某目录所在分区的可用字节数（读不到时返回 None，不阻断）
async fn free_bytes(dir: &Path) -> Option<u64> {
    let out = tokio::process::Command::new("df").args(["--output=avail", "-B1"]).arg(dir).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().nth(1)?.trim().parse().ok()
}

async fn probe(args: &[&str], file: &Path) -> Result<String, String> {
    let out = tokio::process::Command::new("ffprobe")
        .args(["-v", "error"])
        .args(args)
        .arg(file)
        .output()
        .await
        .map_err(|e| format!("无法运行 ffprobe：{e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn probe_duration(file: &Path) -> f64 {
    probe(&["-show_entries", "format=duration", "-of", "csv=p=0"], file).await.ok().and_then(|s| s.parse().ok()).unwrap_or(0.0)
}

/// 用于判断各集能否无损拼接的规格指纹
async fn probe_spec(file: &Path) -> String {
    probe(&["-show_entries", "stream=codec_type,codec_name,profile,width,height,pix_fmt,sample_rate,channels", "-of", "csv=p=0"], file)
        .await
        .unwrap_or_default()
}

fn close_enough(got: f64, want: f64) -> bool {
    want <= 0.0 || (got - want).abs() <= want.max(10.0) * 0.03 + 2.0
}

fn ep_label(eps: &[u32]) -> String {
    if eps.len() == 1 {
        return format!("第{}集", eps[0]);
    }
    let (a, b) = (eps[0], eps[eps.len() - 1]);
    if (b - a) as usize + 1 == eps.len() {
        format!("第{a}-{b}集")
    } else {
        format!("第{a}-{b}集（选{}集）", eps.len())
    }
}

async fn run(app: Arc<crate::App>, id: i64, eps: Vec<u32>, work: &Path, status: Arc<Mutex<Status>>) -> Result<(), String> {
    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
    let title = api::api_video(tauri::State(&app.api), id)
        .await
        .ok()
        .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("剧集{id}"));
    status.lock().unwrap().filename = format!("{} {}.mp4", sanitize(&title), ep_label(&eps));

    // 逐集取回并封装
    let mut parts: Vec<(u32, PathBuf, f64)> = Vec::new();
    let mut expected_total = 0.0;
    for (i, &ep) in eps.iter().enumerate() {
        if let Some(free) = free_bytes(work).await {
            if free < MIN_FREE_PER_EP {
                return Err(format!("服务器硬盘空间不足（剩 {} MB），请少选几集", free / 1024 / 1024));
            }
        }
        {
            let mut s = status.lock().unwrap();
            s.state = "running";
            s.ep_index = i + 1;
            s.cur_ep = ep;
            s.done = 0;
            s.total = 0;
        }
        let dir = work.join(format!("e{:03}", i + 1));
        let (dur, exp) = fetch_episode(&app, id, ep, &dir, &status).await.map_err(|e| format!("第{ep}集：{e}"))?;
        expected_total += exp;
        parts.push((ep, dir.join("out.mp4"), dur));
        status.lock().unwrap().expected = expected_total;
    }

    let out = work.join("out.mp4");
    if parts.len() == 1 {
        std::fs::rename(&parts[0].1, &out).map_err(|e| e.to_string())?;
    } else {
        status.lock().unwrap().state = "merging";
        // 无损拼接要求各集规格一致
        let first = probe_spec(&parts[0].1).await;
        for (ep, p, _) in &parts[1..] {
            if probe_spec(p).await != first {
                return Err(format!("第{ep}集的视频规格与第{}集不同，无法无损合并；请分开下载", parts[0].0));
            }
        }
        let need: u64 = parts.iter().map(|(_, p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
        if let Some(free) = free_bytes(work).await {
            if free < need + 200 * 1024 * 1024 {
                return Err(format!(
                    "服务器硬盘空间不足以合并（需要约 {} MB，剩 {} MB），请少选几集",
                    need / 1024 / 1024,
                    free / 1024 / 1024
                ));
            }
        }
        let mut list = String::new();
        let mut meta = format!(";FFMETADATA1\ntitle={}\n", sanitize(&title));
        let mut t_ms: u64 = 0;
        for (ep, p, dur) in &parts {
            list.push_str(&format!("file '{}'\n", p.display()));
            let end = t_ms + (dur * 1000.0).round() as u64;
            meta.push_str(&format!("[CHAPTER]\nTIMEBASE=1/1000\nSTART={t_ms}\nEND={end}\ntitle=第{ep}集\n"));
            t_ms = end;
        }
        std::fs::write(work.join("list.txt"), list).map_err(|e| e.to_string())?;
        std::fs::write(work.join("chapters.txt"), meta).map_err(|e| e.to_string())?;
        let ff = tokio::process::Command::new("ffmpeg")
            .current_dir(work)
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "concat", "-safe", "0", "-i", "list.txt", "-i", "chapters.txt"])
            .args(["-map", "0", "-map_metadata", "1", "-map_chapters", "1", "-c", "copy", "-movflags", "+faststart", "out.mp4"])
            .output()
            .await
            .map_err(|e| format!("无法运行 ffmpeg：{e}"))?;
        if !ff.status.success() {
            let msg = String::from_utf8_lossy(&ff.stderr);
            return Err(format!("合并失败：{}", msg.lines().last().unwrap_or("")));
        }
        for (_, p, _) in &parts {
            if let Some(d) = p.parent() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    let duration = probe_duration(&out).await;
    let sum_parts: f64 = parts.iter().map(|(_, _, d)| d).sum();
    if !close_enough(duration, sum_parts) || !close_enough(duration, expected_total) {
        return Err(format!("成片时长 {duration:.0}s 与各集合计 {sum_parts:.0}s / 清单 {expected_total:.0}s 不符"));
    }
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let mut s = status.lock().unwrap();
    s.duration = duration;
    s.bytes = bytes;
    Ok(())
}

/// 取回一集并封装为 `<dir>/out.mp4`，返回（成片时长，清单时长）
async fn fetch_episode(app: &Arc<crate::App>, id: i64, ep: u32, dir: &Path, status: &Arc<Mutex<Status>>) -> Result<(f64, f64), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let url = match test_hook_play_url().map(|u| u.replace("{ep}", &ep.to_string())) {
        Some(u) => u,
        None => {
            let play = api::api_play(tauri::State(&app.api), id, ep).await?;
            play.get("video_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or("没有播放地址")?.to_string()
        }
    };

    // 主清单 → 选码率最高的子清单
    let mut text = String::from_utf8_lossy(&fetch(&format!("{MEDIA_PREFIX}/hls?u={}", enc(&url))).await?).into_owned();
    if text.contains("#EXT-X-STREAM-INF") {
        let mut best: Option<(u64, String)> = None;
        let mut bw = 0u64;
        for line in text.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                bw = rest.split(',').find_map(|kv| kv.trim().strip_prefix("BANDWIDTH=")).and_then(|v| v.parse().ok()).unwrap_or(0);
            } else if !line.is_empty() && !line.starts_with('#') && best.as_ref().is_none_or(|(b, _)| bw > *b) {
                best = Some((bw, line.to_string()));
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
                jobs.push((uri.to_string(), dir.join(&name)));
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
            jobs.push((line.to_string(), dir.join(&name)));
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
        s.done = 0;
    }
    std::fs::write(dir.join("local.m3u8"), out).map_err(|e| e.to_string())?;

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
    let ff = tokio::process::Command::new("ffmpeg")
        .current_dir(dir)
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
    let mp4 = dir.join("out.mp4");
    let duration = probe_duration(&mp4).await;
    if !close_enough(duration, expected) {
        return Err(format!("成片时长 {duration:.0}s 与清单 {expected:.0}s 不符"));
    }
    // 清掉分片，只留成片
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name() != "out.mp4" {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok((duration, expected))
}

/// 测试专用：仅在 debug 构建且设置了 HGHUB_TEST_URL 时生效，release 构建恒为 None
fn test_hook_play_url() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("HGHUB_TEST_URL").ok()
    } else {
        None
    }
}
