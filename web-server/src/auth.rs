//! 密码登录门：Argon2 口令哈希 + 随机会话 Cookie + 失败锁定。

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::http::{header, HeaderMap};
use rand::{rngs::OsRng, RngCore};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const COOKIE: &str = "hg_auth";
const SESSION_SECS: u64 = 30 * 24 * 3600;
const MAX_FAILS: u32 = 5;
const FAIL_WINDOW_SECS: u64 = 10 * 60;
const LOCK_SECS: u64 = 15 * 60;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn token_digest(token: &str) -> String {
    // 会话文件只存令牌的 SHA-256；令牌本身是 256 位随机数
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

struct Fail {
    count: u32,
    first: u64,
    locked_until: u64,
}

pub struct Auth {
    hash: String,
    sessions_path: PathBuf,
    sessions: Mutex<HashMap<String, u64>>,
    fails: Mutex<HashMap<IpAddr, Fail>>,
}

pub enum LoginResult {
    Ok(String),
    Wrong,
    Locked,
}

impl Auth {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let hash_path = dir.join("password.hash");
        let hash = std::fs::read_to_string(&hash_path)
            .map_err(|_| format!("尚未设置登录密码：请先运行 `sudo -u hghub HGHUB_DATA={} hghub-web set-password`", dir.display()))?
            .trim()
            .to_string();
        PasswordHash::new(&hash).map_err(|e| format!("密码哈希文件损坏：{e}"))?;
        let sessions_path = dir.join("sessions.json");
        let mut sessions: HashMap<String, u64> = std::fs::read_to_string(&sessions_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let t = now();
        sessions.retain(|_, exp| *exp > t);
        Ok(Self { hash, sessions_path, sessions: Mutex::new(sessions), fails: Mutex::new(HashMap::new()) })
    }

    fn persist(&self, map: &HashMap<String, u64>) {
        if let Ok(text) = serde_json::to_string(map) {
            let tmp = self.sessions_path.with_extension("tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.sessions_path);
            }
        }
    }

    fn cookie_token(headers: &HeaderMap) -> Option<String> {
        headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == COOKIE && !v.is_empty()).then(|| v.to_string())
        })
    }

    pub fn check_cookie(&self, headers: &HeaderMap) -> bool {
        let Some(token) = Self::cookie_token(headers) else { return false };
        let d = token_digest(&token);
        let t = now();
        self.sessions.lock().map(|m| m.get(&d).is_some_and(|exp| *exp > t)).unwrap_or(false)
    }

    pub fn revoke(&self, headers: &HeaderMap) {
        if let Some(token) = Self::cookie_token(headers) {
            if let Ok(mut m) = self.sessions.lock() {
                m.remove(&token_digest(&token));
                self.persist(&m);
            }
        }
    }

    pub async fn login(&self, ip: IpAddr, password: &str) -> LoginResult {
        let t = now();
        if let Ok(f) = self.fails.lock() {
            if f.get(&ip).is_some_and(|x| x.locked_until > t) {
                return LoginResult::Locked;
            }
        }
        let ok = PasswordHash::new(&self.hash)
            .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
            .unwrap_or(false);
        if !ok {
            let locked = if let Ok(mut f) = self.fails.lock() {
                let e = f.entry(ip).or_insert(Fail { count: 0, first: t, locked_until: 0 });
                if t.saturating_sub(e.first) > FAIL_WINDOW_SECS {
                    e.count = 0;
                    e.first = t;
                }
                e.count += 1;
                if e.count >= MAX_FAILS {
                    e.locked_until = t + LOCK_SECS;
                    e.count = 0;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            return if locked { LoginResult::Locked } else { LoginResult::Wrong };
        }
        if let Ok(mut f) = self.fails.lock() {
            f.remove(&ip);
        }
        let mut raw = [0u8; 32];
        OsRng.fill_bytes(&mut raw);
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        if let Ok(mut m) = self.sessions.lock() {
            m.retain(|_, exp| *exp > t);
            m.insert(token_digest(&token), t + SESSION_SECS);
            self.persist(&m);
        }
        LoginResult::Ok(format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_SECS}"))
    }
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// `hghub-web set-password`：在服务器终端里隐藏输入两次，写入 Argon2 哈希，并让所有已登录会话失效。
pub fn set_password_interactive(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("无法创建 {}: {e}", dir.display()))?;
    let p1 = rpassword::prompt_password("设置网页版登录密码（输入不显示，至少 8 位）：").map_err(|e| e.to_string())?;
    if p1.chars().count() < 8 {
        return Err("密码至少 8 位".into());
    }
    let p2 = rpassword::prompt_password("再输入一次：").map_err(|e| e.to_string())?;
    if p1 != p2 {
        return Err("两次输入不一致".into());
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default().hash_password(p1.as_bytes(), &salt).map_err(|e| e.to_string())?.to_string();
    let path = dir.join("password.hash");
    std::fs::write(&path, format!("{hash}\n")).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = std::fs::remove_file(dir.join("sessions.json"));
    println!("已设置。重启服务后生效：sudo systemctl restart hghub-web");
    Ok(())
}

pub const LOGIN_HTML: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="robots" content="noindex,nofollow">
<title>登录</title>
<style>
*{box-sizing:border-box}html,body{margin:0;height:100%;background:#1a1a1a;color:#eee;font:16px -apple-system,BlinkMacSystemFont,"PingFang SC",sans-serif}
main{min-height:100%;display:flex;align-items:center;justify-content:center;padding:24px}
form{width:100%;max-width:340px;background:#242424;border-radius:16px;padding:28px 22px;box-shadow:0 8px 30px rgba(0,0,0,.35)}
h1{font-size:20px;margin:0 0 18px;text-align:center}
input{width:100%;padding:13px 14px;border-radius:10px;border:1px solid #3a3a3a;background:#1b1b1b;color:#fff;font-size:16px}
button{width:100%;margin-top:14px;padding:13px;border:0;border-radius:10px;background:#f5a524;color:#1a1a1a;font-weight:600;font-size:16px}
.m{min-height:20px;margin:10px 0 0;color:#ff6b6b;font-size:14px;text-align:center}
</style></head><body><main>
<form method="post" action="/login" autocomplete="on">
<h1>HGhub</h1>
<input type="password" name="password" placeholder="访问密码" autocomplete="current-password" required autofocus>
<button type="submit">进入</button>
<p class="m">{{MSG}}</p>
</form></main></body></html>"#;
