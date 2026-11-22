# 网页版（iOS Safari 可用）

在自己的常开服务器上跑一个 HTTP 服务，iPhone 用 Safari 打开即可，可「添加到主屏幕」。仅限个人非商业自用，遵守 [LICENSE](LICENSE) 与 README 的说明。

## 结构

- `web-server/`：axum HTTP 服务。通过 `#[path]` 直接复用 `src-tauri/src` 的核心模块（业务接口、封面解密、HLS 改写与分片代理、线路与安全策略），一行不改；`web-server/shim/` 是让这些模块脱离 Tauri 编译的最小替身。
  - `POST /api/invoke/<cmd>`：替代原生 `invoke`
  - `GET /media/{img,hls,seg}?u=`：替代 `hg://` 媒体协议
  - `/login`：密码登录门（Argon2 哈希、会话 Cookie 30 天、同一 IP 连错 5 次锁 15 分钟）；未登录时页面、接口、媒体一律不可见
  - 多集合并：选集面板「多集合并下载」→ 勾选若干集 → `POST /api/download/start {id, eps:[…]}`；服务器先只取每集「清单 + 第一小段」检查规格，以多数集为准：一致的集无损，规格不同的集单独转码统一（只有音频不同则只转音频；没有声音的补静音；转码集关键帧内嵌参数集、时间基统一 90000），再 ffmpeg concat 拼成一个 MP4，每集一个章节，核对总时长；硬盘不足（每集前需 800MB、合并前需各集总大小 +200MB）会提前停下并提示。成片保留 2 小时，离开页面后回到该剧选集可继续看进度、保存。
  - 「下载本集」：`POST /api/download/start` → `GET /api/download/status/<job>` → `GET /download/<job>`。服务器经同一媒体代理取回全部分片与 Key，用 ffmpeg 解密并无损封装为 MP4（`-c copy`），ffprobe 核对时长与清单一致后才算完成；同时最多 2 集，成片 1 小时后清理。入口在播放页「选集」面板顶部，仅网页版构建显示。需要服务器装 ffmpeg（install.sh 会自动装）。
- `web/`、`vite.web.config.ts`：前端适配层，把 `@tauri-apps/api/core|window` 换成 HTTP 版；`src/api.ts` 在 `VITE_WEB` 下令 `LOCAL = "/media"`。桌面/移动端原有构建不受影响。
- `web-server/deploy/`：`install.sh`（systemd 常驻 + 开机自启）、`verify.py`（验真）、`hghub-web.service`。

## 构建与部署

```bash
bash web-server/build-package.sh          # 产出 web-server/target/pkg/hghub-web.tar.gz
# 上传到服务器后：
tar xzf hghub-web.tar.gz && sudo bash hghub-web/install.sh   # 首次会在终端里设置登录密码
python3 hghub-web/verify.py                                   # 本机验真，应全部 PASS
python3 hghub-web/verify.py --download                        # 另外验「下载本集」：成片完整解码、时长一致
python3 hghub-web/verify.py --merge                           # 另外验多集合并：前两集合成一个 MP4，章节与总时长
```

上线顺序：首次安装只绑 `127.0.0.1:8900`，本机验真全过后，再把 `/etc/hghub-web.env` 的 `HGHUB_BIND` 改为 `0.0.0.0:8900` 并 `systemctl restart hghub-web`，最后在云控制台防火墙放行 TCP 8900，用 `python3 hghub-web/verify.py http://<外网IP>:8900` 复验。

更新：Sync fork → 重新 `build-package.sh` → 上传后再跑一次 `install.sh`（保留密码、会话与配置）。

改密码：`sudo -u hghub HGHUB_DATA=/var/lib/hghub-web hghub-web set-password && sudo systemctl restart hghub-web`

注意：当前为明文 HTTP，登录密码不加密传输；需要时请自行配域名与 HTTPS 反向代理。
