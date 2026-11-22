//! 下载：单集，或把选中的多集合并成一个 MP4，供 iPhone Safari 存到「文件」后离线播放。
//!
//! 取流完全复用 media::handle（与在线播放同一套防盗链、线路与安全策略），这里只做编排：
//! 每集：解析清单 → 并发取分片与 Key → 写本地清单 → ffmpeg -c copy 解密封装 → ffprobe 核对时长；
//! 多集：先只取每集「清单 + 第一小段」检查规格 → 以多数集为准，规格不同的集单独转码统一，
//! 其余集无损 → ffmpeg concat 拼接并写入每集章节 → 核对总时长。

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
    /// (checking) → running → muxing → (transcoding) → … → (merging) → done / error
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
    /// 多集时检查规格的结论（哪几集要转码）
    pub note: Option<String>,
    #[serde(skip)]
    pub created: u64,
}

fn busy(state: &str) -> bool {
    matches!(state, "checking" | "running" | "muxing" | "transcoding" | "merging")
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
            state: if eps.len() > 1 { "checking" } else { "running" },
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
            note: None,
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
    let multi = eps.len() > 1;

    // ① 规划每一集；多集时先只取「清单 + 第一小段」检查规格，再决定怎么合并
    let mut plans: Vec<(u32, Plan)> = Vec::new();
    let mut specs: Vec<Spec> = Vec::new();
    for (i, &ep) in eps.iter().enumerate() {
        {
            let mut s = status.lock().unwrap();
            s.state = if multi { "checking" } else { "running" };
            s.ep_index = i + 1;
            s.cur_ep = ep;
        }
        let dir = work.join(format!("e{:03}", i + 1));
        let plan = plan_episode(&app, id, ep, &dir).await.map_err(|e| format!("第{ep}集：{e}"))?;
        if multi {
            fetch_jobs(&plan.jobs[..plan.probe_jobs], None).await.map_err(|e| format!("第{ep}集：{e}"))?;
            specs.push(probe_spec(&dir.join("probe.m3u8")).await.map_err(|e| format!("第{ep}集：{e}"))?);
        }
        plans.push((ep, plan));
    }

    // ② 以多数集的规格为准，找出不同的几集
    let (target, odd): (Option<Spec>, Vec<usize>) = if multi {
        let mut counts: Vec<(usize, usize)> = Vec::new(); // (代表下标, 次数)
        for (i, s) in specs.iter().enumerate() {
            match counts.iter_mut().find(|(j, _)| specs[*j] == *s) {
                Some(c) => c.1 += 1,
                None => counts.push((i, 1)),
            }
        }
        let best = counts.iter().max_by_key(|(j, n)| (*n, std::cmp::Reverse(*j))).map(|(j, _)| *j).unwrap_or(0);
        let t = specs[best].clone();
        let odd: Vec<usize> = (0..specs.len()).filter(|&i| specs[i] != t).collect();
        if !odd.is_empty() {
            if t.vcodec != "h264" {
                return Err(format!("多数集的视频编码是 {}，暂不支持把不同规格的集转成它；请分开下载", t.vcodec));
            }
            if t.acodec.as_deref().is_some_and(|c| c != "aac") {
                return Err(format!("多数集的音频编码是 {}，暂不支持统一；请分开下载", t.acodec.as_deref().unwrap_or("")));
            }
            let parts: Vec<String> = odd.iter().map(|&i| format!("第{}集（{}）", plans[i].0, specs[i].diff(&t))).collect();
            status.lock().unwrap().note = Some(format!("{} 与其他集规格不同，会单独转码统一，其余集无损", parts.join("、")));
        }
        (Some(t), odd)
    } else {
        (None, Vec::new())
    };

    // ③ 逐集下载、封装；规格不同的集再转码统一
    let mut parts: Vec<(u32, PathBuf, f64)> = Vec::new();
    let mut expected_total = 0.0;
    for (i, (ep, plan)) in plans.iter().enumerate() {
        if let Some(free) = free_bytes(work).await {
            if free < MIN_FREE_PER_EP {
                return Err(format!("服务器硬盘空间不足（剩 {} MB），请少选几集", free / 1024 / 1024));
            }
        }
        {
            let mut s = status.lock().unwrap();
            s.state = "running";
            s.ep_index = i + 1;
            s.cur_ep = *ep;
            s.done = 0;
            s.total = plan.jobs.len();
        }
        let wrap = |e: String| format!("第{ep}集：{e}");
        fetch_jobs(&plan.jobs, Some(&status)).await.map_err(wrap)?;
        status.lock().unwrap().state = "muxing";
        let mut dur = mux_episode(&plan.dir, plan.expected).await.map_err(wrap)?;
        if odd.contains(&i) {
            status.lock().unwrap().state = "transcoding";
            dur = normalize(&plan.dir, &specs[i], target.as_ref().unwrap(), plan.expected).await.map_err(wrap)?;
        }
        expected_total += plan.expected;
        parts.push((*ep, plan.dir.join("out.mp4"), dur));
        status.lock().unwrap().expected = expected_total;
    }

    // ④ 合并
    let out = work.join("out.mp4");
    if parts.len() == 1 {
        std::fs::rename(&parts[0].1, &out).map_err(|e| e.to_string())?;
    } else {
        status.lock().unwrap().state = "merging";
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
        ffmpeg(work, &[
            "-f", "concat", "-safe", "0", "-i", "list.txt", "-i", "chapters.txt",
            "-map", "0", "-map_metadata", "1", "-map_chapters", "1", "-c", "copy",
            "-video_track_timescale", "90000", "-movflags", "+faststart", "out.mp4",
        ])
        .await
        .map_err(|e| format!("合并失败：{e}"))?;
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

/* ───────────────────────── 单集：规划 / 取回 / 封装 ───────────────────────── */

struct Plan {
    dir: PathBuf,
    /// (代理地址, 本地文件)，按清单顺序：Key / 初始化段 / 分片
    jobs: Vec<(String, PathBuf)>,
    /// 前多少个 job 足够探测规格（到第一个分片为止）
    probe_jobs: usize,
    expected: f64,
}

/// 取清单（选码率最高的一路），改写为本地清单 local.m3u8，并写只含第一小段的 probe.m3u8
async fn plan_episode(app: &Arc<crate::App>, id: i64, ep: u32, dir: &Path) -> Result<Plan, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let url = match test_hook_play_url().map(|u| u.replace("{ep}", &ep.to_string())) {
        Some(u) => u,
        None => {
            let play = api::api_play(tauri::State(&app.api), id, ep).await?;
            play.get("video_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or("没有播放地址")?.to_string()
        }
    };
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

    let mut out = String::new();
    let mut probe: Option<String> = None;
    let mut jobs: Vec<(String, PathBuf)> = Vec::new();
    let mut probe_jobs = 0;
    let mut expected = 0.0f64;
    let mut seg_no = 0usize;
    let mut aux_no = 0usize;
    // fMP4 分片（带 EXT-X-MAP）与 TS 分片用各自的扩展名，ffmpeg 7 会按扩展名校验格式
    let seg_ext = if text.contains("#EXT-X-MAP") { "m4s" } else { "ts" };
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        let mut written = line.to_string();
        if line.starts_with("#EXT-X-KEY") || line.starts_with("#EXT-X-MAP") {
            if let Some(uri) = attr(line, "URI") {
                aux_no += 1;
                let name = format!("aux{aux_no}.bin");
                jobs.push((uri.to_string(), dir.join(&name)));
                written = line.replacen(uri, &name, 1);
            }
        } else if let Some(d) = line.strip_prefix("#EXTINF:") {
            expected += d.split(',').next().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        } else if !line.starts_with('#') {
            seg_no += 1;
            let name = format!("s{seg_no:05}.{seg_ext}");
            jobs.push((line.to_string(), dir.join(&name)));
            written = name;
        }
        out.push_str(&written);
        out.push('\n');
        if seg_no == 1 && probe.is_none() {
            probe = Some(format!("{out}#EXT-X-ENDLIST\n"));
            probe_jobs = jobs.len();
        }
    }
    let probe = probe.ok_or("清单里没有视频分片")?;
    std::fs::write(dir.join("local.m3u8"), out).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("probe.m3u8"), probe).map_err(|e| e.to_string())?;
    Ok(Plan { dir: dir.to_path_buf(), jobs, probe_jobs, expected })
}

/// 并发取回；本地已有的（检查规格时取过的）直接跳过
async fn fetch_jobs(jobs: &[(String, PathBuf)], status: Option<&Arc<Mutex<Status>>>) -> Result<(), String> {
    let jobs = Arc::new(jobs.to_vec());
    let next = Arc::new(AtomicUsize::new(0));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..FETCH_CONCURRENCY {
        let (jobs, next, status) = (jobs.clone(), next.clone(), status.cloned());
        set.spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((src, dst)) = jobs.get(i) else { return Ok::<(), String>(()) };
                if !std::fs::metadata(dst).map(|m| m.len() > 0).unwrap_or(false) {
                    let body = fetch(src).await?;
                    tokio::fs::write(dst, &body).await.map_err(|e| e.to_string())?;
                }
                if let Some(s) = &status {
                    s.lock().unwrap().done += 1;
                }
            }
        });
    }
    while let Some(r) = set.join_next().await {
        if let Err(e) = r.map_err(|e| e.to_string()).and_then(|x| x) {
            set.abort_all();
            return Err(e);
        }
    }
    Ok(())
}

async fn ffmpeg(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = tokio::process::Command::new("ffmpeg")
        .current_dir(dir)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .await
        .map_err(|e| format!("无法运行 ffmpeg（服务器是否已安装？）：{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("ffmpeg 失败").to_string())
    }
}

/// 解密 + 无损封装为 out.mp4，核对时长，清掉分片；返回成片时长
async fn mux_episode(dir: &Path, expected: f64) -> Result<f64, String> {
    ffmpeg(dir, &[
        "-allowed_extensions", "ALL", "-protocol_whitelist", "file,crypto",
        "-i", "local.m3u8", "-c", "copy", "-video_track_timescale", "90000", "-movflags", "+faststart", "out.mp4",
    ])
    .await?;
    let duration = probe_duration(&dir.join("out.mp4")).await;
    if !close_enough(duration, expected) {
        return Err(format!("成片时长 {duration:.0}s 与清单 {expected:.0}s 不符"));
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name() != "out.mp4" {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(duration)
}

/* ───────────────────────── 规格：探测 / 比较 / 统一 ───────────────────────── */

#[derive(Clone, PartialEq, Debug)]
struct Spec {
    vcodec: String,
    profile: String,
    width: u32,
    height: u32,
    pix_fmt: String,
    acodec: Option<String>,
    sample_rate: u32,
    channels: u32,
}

impl Spec {
    fn video_eq(&self, o: &Spec) -> bool {
        self.vcodec == o.vcodec && self.profile == o.profile && self.width == o.width && self.height == o.height && self.pix_fmt == o.pix_fmt
    }
    fn audio_eq(&self, o: &Spec) -> bool {
        self.acodec == o.acodec && self.sample_rate == o.sample_rate && self.channels == o.channels
    }
    /// 给人看的差异说明，例如「854x480 → 1280x720，音频 48000Hz → 44100Hz」
    fn diff(&self, t: &Spec) -> String {
        let mut d = Vec::new();
        if (self.width, self.height) != (t.width, t.height) {
            d.push(format!("{}x{} → {}x{}", self.width, self.height, t.width, t.height));
        }
        if self.vcodec != t.vcodec {
            d.push(format!("视频 {} → {}", self.vcodec, t.vcodec));
        } else if self.profile != t.profile {
            d.push(format!("{} → {}", self.profile, t.profile));
        }
        if self.pix_fmt != t.pix_fmt {
            d.push(format!("{} → {}", self.pix_fmt, t.pix_fmt));
        }
        match (&self.acodec, &t.acodec) {
            (None, Some(_)) => d.push("无声音 → 补静音".into()),
            (Some(_), None) => d.push("去掉声音".into()),
            (Some(a), Some(b)) => {
                if a != b {
                    d.push(format!("音频 {a} → {b}"));
                }
                if self.sample_rate != t.sample_rate {
                    d.push(format!("音频 {}Hz → {}Hz", self.sample_rate, t.sample_rate));
                }
                if self.channels != t.channels {
                    d.push(format!("声道 {} → {}", self.channels, t.channels));
                }
            }
            _ => {}
        }
        d.join("，")
    }
}

async fn probe_spec(file: &Path) -> Result<Spec, String> {
    let out = tokio::process::Command::new("ffprobe")
        .args(["-v", "error", "-allowed_extensions", "ALL", "-protocol_whitelist", "file,crypto"])
        .args(["-show_entries", "stream=codec_type,codec_name,profile,width,height,pix_fmt,sample_rate,channels", "-of", "json"])
        .arg(file)
        .output()
        .await
        .map_err(|e| format!("无法运行 ffprobe：{e}"))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "读不出视频规格".to_string())?;
    let streams = v.get("streams").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    let pick = |t: &str| streams.iter().find(|s| s.get("codec_type").and_then(|x| x.as_str()) == Some(t)).cloned();
    let s = |o: &serde_json::Value, k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let n = |o: &serde_json::Value, k: &str| {
        o.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|t| t.parse().ok()))).unwrap_or(0) as u32
    };
    let vs = pick("video").ok_or("没有视频轨")?;
    let a = pick("audio");
    Ok(Spec {
        vcodec: s(&vs, "codec_name"),
        profile: s(&vs, "profile"),
        width: n(&vs, "width"),
        height: n(&vs, "height"),
        pix_fmt: s(&vs, "pix_fmt"),
        acodec: a.as_ref().map(|a| s(a, "codec_name")),
        sample_rate: a.as_ref().map(|a| n(a, "sample_rate")).unwrap_or(0),
        channels: a.as_ref().map(|a| n(a, "channels")).unwrap_or(0),
    })
}

/// 把一集转成目标规格：视频不同才重编码（关键帧内嵌参数集，时间基对齐 90000），音频不同只重编码音频
async fn normalize(dir: &Path, have: &Spec, t: &Spec, expected: f64) -> Result<f64, String> {
    let mut args: Vec<String> = vec!["-i".into(), "out.mp4".into()];
    let add_silence = have.acodec.is_none() && t.acodec.is_some();
    if add_silence {
        let layout = if t.channels == 1 { "mono" } else { "stereo" };
        args.extend(["-f", "lavfi", "-i"].map(String::from));
        args.push(format!("anullsrc=r={}:cl={layout}", t.sample_rate));
        args.extend(["-map", "0:v:0", "-map", "1:a:0", "-shortest"].map(String::from));
    } else {
        args.extend(["-map", "0:v:0"].map(String::from));
        if t.acodec.is_some() {
            args.extend(["-map", "0:a:0"].map(String::from));
        }
    }
    if have.video_eq(t) {
        args.extend(["-c:v", "copy"].map(String::from));
    } else {
        args.push("-vf".into());
        args.push(format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1",
            w = t.width,
            h = t.height
        ));
        args.extend(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-threads", "2"].map(String::from));
        args.extend(["-pix_fmt".to_string(), t.pix_fmt.clone()]);
        let prof = match t.profile.as_str() {
            "High" => Some("high"),
            "Main" => Some("main"),
            "Baseline" | "Constrained Baseline" => Some("baseline"),
            _ => None,
        };
        if let Some(p) = prof {
            args.extend(["-profile:v".to_string(), p.to_string()]);
        }
        args.extend(["-x264-params", "repeat-headers=1"].map(String::from));
    }
    match &t.acodec {
        None => args.push("-an".into()),
        Some(_) if !add_silence && have.audio_eq(t) => args.extend(["-c:a", "copy"].map(String::from)),
        Some(_) => {
            args.extend(["-c:a", "aac", "-b:a", "128k"].map(String::from));
            args.extend(["-ar".to_string(), t.sample_rate.to_string(), "-ac".to_string(), t.channels.max(1).to_string()]);
        }
    }
    args.extend(["-video_track_timescale", "90000", "-movflags", "+faststart", "norm.mp4"].map(String::from));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    ffmpeg(dir, &refs).await.map_err(|e| format!("转码失败：{e}"))?;
    let got = probe_spec(&dir.join("norm.mp4")).await?;
    if got != *t {
        return Err(format!("转码后规格仍不一致（{}）", got.diff(t)));
    }
    let duration = probe_duration(&dir.join("norm.mp4")).await;
    if !close_enough(duration, expected) {
        return Err(format!("转码后时长 {duration:.0}s 与清单 {expected:.0}s 不符"));
    }
    std::fs::rename(dir.join("norm.mp4"), dir.join("out.mp4")).map_err(|e| e.to_string())?;
    Ok(duration)
}

/// 测试专用：仅在 debug 构建且设置了 HGHUB_TEST_URL 时生效，release 构建恒为 None
fn test_hook_play_url() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("HGHUB_TEST_URL").ok()
    } else {
        None
    }
}
