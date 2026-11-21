#!/usr/bin/env bash
# 用法：sudo bash install.sh   （在解压出的 hghub-web 目录里运行）
set -euo pipefail
cd "$(dirname "$0")"
[ "$(id -u)" = 0 ] || { echo "请用 sudo 运行"; exit 1; }
if ! command -v ffmpeg >/dev/null || ! command -v ffprobe >/dev/null; then
  echo "== 安装 ffmpeg（「下载本集」用来把分片合成 MP4）"
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ffmpeg >/dev/null
fi
id hghub >/dev/null 2>&1 || useradd --system --home /var/lib/hghub-web --shell /usr/sbin/nologin hghub
install -d -o hghub -g hghub -m 700 /var/lib/hghub-web
install -m 755 hghub-web /usr/local/bin/hghub-web
rm -rf /opt/hghub-web/dist; install -d /opt/hghub-web; cp -r dist /opt/hghub-web/dist; chmod -R a+rX /opt/hghub-web
if [ ! -f /etc/hghub-web.env ]; then
  cat > /etc/hghub-web.env <<'ENV'
# 第一阶段只绑本机；本地验通后改成 0.0.0.0:8900 再 systemctl restart hghub-web
HGHUB_BIND=127.0.0.1:8900
HGHUB_DATA=/var/lib/hghub-web
HGHUB_STATIC=/opt/hghub-web/dist
ENV
fi
if [ ! -s /var/lib/hghub-web/password.hash ]; then
  echo "== 设置网页版登录密码（只在这台服务器上输入，不要发到聊天里）"
  sudo -u hghub HGHUB_DATA=/var/lib/hghub-web /usr/local/bin/hghub-web set-password
fi
install -m 644 hghub-web.service /etc/systemd/system/hghub-web.service
systemctl daemon-reload
systemctl enable hghub-web >/dev/null 2>&1
systemctl restart hghub-web
sleep 2
echo "== 回读"
echo "ffmpeg: $(ffmpeg -version 2>/dev/null | head -1 | cut -d" " -f1-3)"
echo "binary sha256: $(sha256sum /usr/local/bin/hghub-web | cut -c1-16)"
echo "enabled: $(systemctl is-enabled hghub-web)   active: $(systemctl is-active hghub-web)"
grep HGHUB_BIND /etc/hghub-web.env
ss -ltnp 2>/dev/null | grep 8900 || echo "8900 未监听"
journalctl -u hghub-web -n 5 --no-pager
