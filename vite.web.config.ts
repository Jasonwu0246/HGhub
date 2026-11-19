// 网页版构建：npx vite build --config vite.web.config.ts
// 与 vite.config.ts 相同，只把 Tauri API 换成 HTTP 适配层，并打开 VITE_WEB 开关（媒体走 /media）。
import { defineConfig, mergeConfig } from "vite";
import { fileURLToPath } from "node:url";
import base from "./vite.config";

export default mergeConfig(base, defineConfig({
  resolve: {
    alias: {
      "@tauri-apps/api/core": fileURLToPath(new URL("./web/tauri-core.ts", import.meta.url)),
      "@tauri-apps/api/window": fileURLToPath(new URL("./web/tauri-window.ts", import.meta.url)),
    },
  },
  define: { "import.meta.env.VITE_WEB": JSON.stringify("1") },
  build: { outDir: "dist-web", emptyOutDir: true },
  plugins: [{
    name: "hghub-web-head",
    transformIndexHtml: (html: string) => html.replace(
      "</head>",
      '    <link rel="apple-touch-icon" href="/apple-touch-icon.png" />\n    <meta name="apple-mobile-web-app-title" content="HGhub" />\n  </head>',
    ),
  }],
}));
