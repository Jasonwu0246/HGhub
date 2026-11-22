//! 「下载本集」（Windows 桌面版）：在本机把一集 HLS 取回、解密并按顺序拼成一个完整文件，
//! 存到「下载 / HGhub」文件夹，离线可看。
//!
//! 取流完全复用 media::handle（与在线播放同一套防盗链、线路与安全策略）；不依赖 ffmpeg：
//! TS 分片解密后直接首尾相接即是合法的 MPEG-TS（.ts），带 EXT-X-MAP 的 fMP4 分片拼成 .mp4。
//! 画质无损，Windows 自带播放器与 VLC 均可直接播放。

use crate::{api, media};
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use once_cell::sync::Lazy;
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

/// media::handle 改写清单时用的内部前缀；只在本模块内解析，不对外暴露
const ORIGIN: &str = "/dl";
const BATCH: usize = 4;
const RETRIES: usize = 3;
const MAX_RUNNING: usize = 2;

#[derive(Clone, Serialize)]
pub struct Status {
    /// running → done / error
    pub state: &'static str,
    pub done: usize,
    pub total: usize,
    pub filename: String,
    pub path: String,
    pub bytes: u64,
    /// 清单 EXTINF 累计时长（秒）
    pub expected: f64,
    pub error: Option<String>,
}

static JOBS: Lazy<Mutex<HashMap<String, Arc<Mutex<Status>>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[tauri::command]
pub async fn download_start(app: AppHandle, state: State<'_, api::Api>, id: i64, ep: u32) -> Result<String, String> {
    let key = format!("{id}-{ep}");
    {
        let jobs = JOBS.lock().map_err(|_| "下载状态锁失败")?;
        if let Some(s) = jobs.get(&key) {
            if s.lock().map(|s| s.state != "error").unwrap_or(false) {
                return Ok(key);
            }
        }
        if jobs.values().filter(|s| s.lock().map(|s| s.state == "running").unwrap_or(false)).count() >= MAX_RUNNING {
            return Err("同时最多下载 2 集，请等当前下载完成".into());
        }
    }
    let dir = app.path().download_dir().map_err(|e| format!("找不到「下载」文件夹：{e}"))?.join("HGhub");

    // 标题与播放地址在命令里取（需要 State），下载本身放到后台
    let title = api::api_video(State::clone(&state), id)
        .await
        .ok()
        .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("剧集{id}"));
    let play = api::api_play(state, id, ep).await?;
    let url = play.get("video_url").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or("没有播放地址")?.to_string();

    let status = Arc::new(Mutex::new(Status {
        state: "running",
        done: 0,
        total: 0,
        filename: String::new(),
        path: String::new(),
        bytes: 0,
        expected: 0.0,
        error: None,
    }));
    JOBS.lock().map_err(|_| "下载状态锁失败")?.insert(key.clone(), status.clone());
    let base = format!("{} 第{ep}集", sanitize(&title));
    tauri::async_runtime::spawn(async move {
        let r = run(&url, &dir, &base, status.clone()).await;
        if let Ok(mut s) = status.lock() {
            match r {
                Ok(()) => s.state = "done",
                Err(e) => {
                    s.state = "error";
                    s.error = Some(e);
                }
            }
        }
    });
    Ok(key)
}

#[tauri::command]
pub fn download_status(job: String) -> Result<Status, String> {
    let jobs = JOBS.lock().map_err(|_| "下载状态锁失败")?;
    let s = jobs.get(&job).ok_or("下载任务不存在")?;
    let s = s.lock().map_err(|_| "下载状态锁失败")?.clone();
    Ok(s)
}

#[tauri::command]
pub fn download_reveal(app: AppHandle, job: String) -> Result<(), String> {
    let s = download_status(job)?;
    if s.state != "done" {
        return Err("文件还没下载完成".into());
    }
    app.opener().reveal_item_in_dir(&s.path).map_err(|e| e.to_string())
}

/* ───────────────────────── 下载主体（与 Tauri 无关，便于单独测试） ───────────────────────── */

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { ' ' } else { c })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(60).collect();
    let s = s.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() { "HGhub".into() } else { s }
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// 经媒体代理取一个已改写的地址（形如 `/dl/seg?u=…`）
async fn fetch(local: &str) -> Result<Vec<u8>, String> {
    let rest = local.strip_prefix(ORIGIN).ok_or_else(|| "清单地址异常".to_string())?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut last = String::new();
    for attempt in 0..RETRIES {
        let resp = media::handle(path, query, ORIGIN).await;
        if resp.status().is_success() {
            return Ok(resp.into_body());
        }
        last = format!("HTTP {}", resp.status().as_u16());
        tokio::time::sleep(std::time::Duration::from_millis(600 * (attempt as u64 + 1))).await;
    }
    Err(format!("取流失败（{last}）"))
}

fn attr<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let i = line.find(&format!("{name}="))? + name.len() + 1;
    let rest = &line[i..];
    if let Some(r) = rest.strip_prefix('"') {
        Some(&r[..r.find('"')?])
    } else {
        Some(rest.split(',').next().unwrap_or("").trim())
    }
}

fn parse_iv(hex: &str) -> Result<[u8; 16], String> {
    let h = hex.trim_start_matches("0x").trim_start_matches("0X");
    if h.len() > 32 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("IV 格式异常：{hex}"));
    }
    let h = format!("{h:0>32}");
    let mut iv = [0u8; 16];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(iv)
}

fn seq_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

pub(crate) fn aes128_cbc_decrypt(key: &[u8], iv: [u8; 16], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err("密文长度不是 16 的整数倍".into());
    }
    let cipher = Aes128::new_from_slice(key).map_err(|_| "Key 长度异常")?;
    let mut prev = iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut ct = [0u8; 16];
        ct.copy_from_slice(chunk);
        let mut block = ct;
        cipher.decrypt_block((&mut block).into());
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev = ct;
    }
    let pad = *out.last().unwrap_or(&0) as usize;
    if pad == 0 || pad > 16 || out.len() < pad || !out[out.len() - pad..].iter().all(|b| *b as usize == pad) {
        return Err("解密失败（填充不合法，Key 或 IV 不对）".into());
    }
    out.truncate(out.len() - pad);
    Ok(out)
}

struct Seg {
    uri: String,
    /// (Key 地址, IV)；None 表示未加密
    key: Option<(String, [u8; 16])>,
}

pub(crate) async fn run(url: &str, dir: &Path, base: &str, status: Arc<Mutex<Status>>) -> Result<(), String> {
    // 主清单 → 码率最高的子清单
    let mut text = String::from_utf8_lossy(&fetch(&format!("{ORIGIN}/hls?u={}", enc(url))).await?).into_owned();
    if text.contains("#EXT-X-STREAM-INF") {
        let (mut best, mut bw): (Option<(u64, String)>, u64) = (None, 0);
        for line in text.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                bw = attr(rest, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0);
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

    // 解析分片、Key、初始化段
    let mut seq: u64 = 0;
    let mut cur_key: Option<(String, Option<[u8; 16]>)> = None;
    let mut init: Option<String> = None;
    let mut segs: Vec<Seg> = Vec::new();
    let mut expected = 0.0f64;
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            match attr(rest, "METHOD").unwrap_or("NONE") {
                "NONE" => cur_key = None,
                "AES-128" => {
                    let uri = attr(rest, "URI").ok_or("Key 缺少 URI")?.to_string();
                    let iv = attr(rest, "IV").map(parse_iv).transpose()?;
                    cur_key = Some((uri, iv));
                }
                m => return Err(format!("不支持的加密方式 {m}")),
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attr(rest, "URI").ok_or("EXT-X-MAP 缺少 URI")?.to_string();
            if init.as_ref().is_some_and(|u| *u != uri) {
                return Err("清单中途更换初始化段，暂不支持".into());
            }
            init = Some(uri);
        } else if let Some(d) = line.strip_prefix("#EXTINF:") {
            expected += d.split(',').next().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        } else if !line.is_empty() && !line.starts_with('#') {
            let n = seq + segs.len() as u64;
            segs.push(Seg { uri: line.to_string(), key: cur_key.as_ref().map(|(u, iv)| (u.clone(), iv.unwrap_or_else(|| seq_iv(n)))) });
        }
    }
    if segs.is_empty() {
        return Err("清单里没有视频分片".into());
    }
    let fmp4 = init.is_some();
    let ext = if fmp4 { "mp4" } else { "ts" };

    std::fs::create_dir_all(dir).map_err(|e| format!("无法创建 {}：{e}", dir.display()))?;
    let mut target = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while target.exists() {
        target = dir.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    let part = target.with_extension(format!("{ext}.part"));
    {
        let mut s = status.lock().map_err(|_| "锁失败")?;
        s.total = segs.len();
        s.expected = expected;
        s.filename = target.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    }

    let result = write_all(&part, init.as_deref(), &segs, fmp4, &status).await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &target).map_err(|e| format!("保存失败：{e}"))?;
    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let mut s = status.lock().map_err(|_| "锁失败")?;
    s.bytes = bytes;
    s.path = target.to_string_lossy().into_owned();
    Ok(())
}

async fn write_all(part: &PathBuf, init: Option<&str>, segs: &[Seg], fmp4: bool, status: &Arc<Mutex<Status>>) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::io::BufWriter::new(std::fs::File::create(part).map_err(|e| format!("无法写入文件：{e}"))?);
    if let Some(u) = init {
        file.write_all(&fetch(u).await?).map_err(|e| e.to_string())?;
    }
    let mut keys: HashMap<String, Vec<u8>> = HashMap::new();
    for (bi, batch) in segs.chunks(BATCH).enumerate() {
        // Key 先取（通常整集只有一把）
        for s in batch {
            if let Some((u, _)) = &s.key {
                if !keys.contains_key(u) {
                    let k = fetch(u).await?;
                    if k.len() != 16 {
                        return Err(format!("Key 长度异常（{} 字节）", k.len()));
                    }
                    keys.insert(u.clone(), k);
                }
            }
        }
        let handles: Vec<_> = batch.iter().map(|s| tauri::async_runtime::spawn(fetch_owned(s.uri.clone()))).collect();
        for (i, (h, s)) in handles.into_iter().zip(batch).enumerate() {
            let idx = bi * BATCH + i + 1;
            let raw = h.await.map_err(|e| e.to_string())??;
            let data = match &s.key {
                Some((u, iv)) => aes128_cbc_decrypt(&keys[u], *iv, &raw).map_err(|e| format!("第 {idx} 段{e}"))?,
                None => raw,
            };
            if !fmp4 && (data.is_empty() || data[0] != 0x47 || data.len() % 188 != 0) {
                return Err(format!("第 {idx} 段校验失败（不是完整的 TS 数据）"));
            }
            file.write_all(&data).map_err(|e| format!("写入失败：{e}"))?;
            if let Ok(mut st) = status.lock() {
                st.done += 1;
            }
        }
    }
    file.flush().map_err(|e| format!("写入失败：{e}"))?;
    Ok(())
}

async fn fetch_owned(u: String) -> Result<Vec<u8>, String> {
    fetch(&u).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encrypt(key: &[u8], iv: [u8; 16], plain: &[u8]) -> Vec<u8> {
        use aes::cipher::BlockEncrypt;
        let c = Aes128::new_from_slice(key).unwrap();
        let pad = 16 - plain.len() % 16;
        let mut data = plain.to_vec();
        data.extend(std::iter::repeat(pad as u8).take(pad));
        let mut prev = iv;
        let mut out = Vec::new();
        for chunk in data.chunks(16) {
            let mut b = [0u8; 16];
            for i in 0..16 { b[i] = chunk[i] ^ prev[i]; }
            c.encrypt_block((&mut b).into());
            out.extend_from_slice(&b);
            prev = b;
        }
        out
    }

    #[test]
    fn decrypt_roundtrip_and_bad_key() {
        let key = [7u8; 16];
        let iv = seq_iv(42);
        let plain: Vec<u8> = (0..188 * 5).map(|i| (i % 251) as u8).collect();
        let ct = encrypt(&key, iv, &plain);
        assert_eq!(aes128_cbc_decrypt(&key, iv, &ct).unwrap(), plain);
        assert!(aes128_cbc_decrypt(&[8u8; 16], iv, &ct).is_err() || aes128_cbc_decrypt(&[8u8; 16], iv, &ct).unwrap() != plain);
        assert!(aes128_cbc_decrypt(&key, iv, &ct[..ct.len() - 1]).is_err());
    }

    #[test]
    fn iv_and_names() {
        assert_eq!(parse_iv("0x0000000000000000000000000000002a").unwrap(), seq_iv(42));
        assert_eq!(parse_iv("0x2a").unwrap(), seq_iv(42));
        assert!(parse_iv("0xZZ").is_err());
        assert_eq!(sanitize("神瞳: 觉醒/第一季?"), "神瞳 觉醒 第一季");
        assert_eq!(sanitize("..."), "HGhub");
        assert_eq!(attr("METHOD=AES-128,URI=\"k.bin\",IV=0x01", "URI"), Some("k.bin"));
        assert_eq!(attr("BANDWIDTH=900000,RESOLUTION=640x360", "BANDWIDTH"), Some("900000"));
    }

    /// 端到端：HGHUB_DL_TEST_URL=<本地加密 HLS 主清单> cargo test -- --ignored download_e2e
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn download_e2e() {
        let url = std::env::var("HGHUB_DL_TEST_URL").expect("HGHUB_DL_TEST_URL");
        let out = PathBuf::from(std::env::var("HGHUB_DL_TEST_OUT").unwrap_or_else(|_| "/tmp/hg-dl-test".into()));
        let _ = std::fs::remove_dir_all(&out);
        let data = out.with_extension("data");
        let _ = std::fs::create_dir_all(&data);
        crate::net::bind_base(&data);
        crate::api::security_set(false).await.unwrap();
        let st = Arc::new(Mutex::new(Status { state: "running", done: 0, total: 0, filename: String::new(), path: String::new(), bytes: 0, expected: 0.0, error: None }));
        run(&url, &out, "测试 第1集", st.clone()).await.unwrap();
        let s = st.lock().unwrap().clone();
        println!("E2E path={} bytes={} done={}/{} expected={}", s.path, s.bytes, s.done, s.total, s.expected);
        assert_eq!(s.done, s.total);
        assert!(s.bytes > 0 && std::path::Path::new(&s.path).exists());
    }
}
