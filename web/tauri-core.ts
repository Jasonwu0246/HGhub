// 网页版适配层：替代 @tauri-apps/api/core。
// 原生 invoke(cmd, args) → POST /api/invoke/<cmd>（同源、带登录 Cookie），
// 错误语义与 Tauri 一致：失败时 reject 一个字符串。
export async function invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (cmd.startsWith("plugin:")) return undefined as T;
  if (cmd === "open_url") {
    const url = typeof args?.url === "string" ? args.url : "";
    if (/^https?:\/\//i.test(url)) window.open(url, "_blank", "noopener,noreferrer");
    return undefined as T;
  }
  if (cmd === "set_orientation") return undefined as T;
  let r: Response;
  try {
    r = await fetch(`/api/invoke/${encodeURIComponent(cmd)}`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(args ?? {}),
    });
  } catch {
    throw "网络连接失败，请检查网络";
  }
  if (r.status === 401) {
    window.location.href = "/login";
    throw "未登录";
  }
  const text = await r.text();
  let data: unknown = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!r.ok) throw typeof data === "string" ? data : `HTTP ${r.status}`;
  return data as T;
}

export async function addPluginListener(): Promise<{ unregister: () => Promise<void> }> {
  return { unregister: async () => {} };
}
