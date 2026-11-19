#!/usr/bin/env bash
# 构建网页版部署包：在仓库根目录运行 bash web-server/build-package.sh
# 需要 Node 22 + pnpm 10 + Rust 1.90+（Linux x86_64，产物要求目标机 glibc ≥ 构建机）
set -euo pipefail
cd "$(dirname "$0")/.."
pnpm install --frozen-lockfile
npx tsc --noEmit
npx vite build --config vite.web.config.ts
cp app-icon.png dist-web/apple-touch-icon.png
cargo build --release --manifest-path web-server/Cargo.toml
out=web-server/target/pkg/hghub-web; rm -rf "$out"; mkdir -p "$out"
cp web-server/target/release/hghub-web "$out/"; cp -r dist-web "$out/dist"
cp web-server/deploy/install.sh web-server/deploy/verify.py web-server/deploy/hghub-web.service "$out/"
tar czf web-server/target/pkg/hghub-web.tar.gz -C web-server/target/pkg hghub-web
sha256sum web-server/target/pkg/hghub-web.tar.gz
