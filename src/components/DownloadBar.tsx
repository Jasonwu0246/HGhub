import { useEffect, useRef, useState } from "react";
import { IDownload } from "../icons";

/* 网页版「下载本集」：服务器把这一集拼成 MP4，Safari 存到「文件」后可离线看。
   只在网页版构建（VITE_WEB）里出现；原生 App 不渲染。 */

type Status = {
  state: "running" | "muxing" | "done" | "error";
  done: number; total: number; filename: string; bytes: number;
  duration: number; expected: number; error?: string | null;
};

async function call<T>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, { credentials: "same-origin", ...init });
  if (r.status === 401) { window.location.href = "/login"; throw new Error("未登录"); }
  const data = await r.json().catch(() => null);
  if (!r.ok) throw new Error(typeof data === "string" ? data : `HTTP ${r.status}`);
  return data as T;
}

const mb = (n: number) => `${(n / 1024 / 1024).toFixed(1)} MB`;
const mmss = (s: number) => `${Math.floor(s / 60)}:${String(Math.round(s % 60)).padStart(2, "0")}`;

export default function DownloadBar({ videoId, ep }: { videoId: number; ep: number }) {
  const [job, setJob] = useState<string | null>(null);
  const [st, setSt] = useState<Status | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const timer = useRef<number | undefined>(undefined);

  useEffect(() => { setJob(null); setSt(null); setErr(null); }, [videoId, ep]);

  useEffect(() => {
    if (!job) return;
    let alive = true;
    const tick = async () => {
      try {
        const s = await call<Status>(`/api/download/status/${encodeURIComponent(job)}`);
        if (!alive) return;
        setSt(s);
        if (s.state === "running" || s.state === "muxing") timer.current = window.setTimeout(tick, 1000);
      } catch (e) {
        if (alive) setErr((e as Error).message);
      }
    };
    tick();
    return () => { alive = false; window.clearTimeout(timer.current); };
  }, [job]);

  const start = async () => {
    setErr(null); setSt(null);
    try {
      const r = await call<{ job: string }>("/api/download/start", {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id: videoId, ep }),
      });
      setJob(r.job);
    } catch (e) {
      setErr((e as Error).message);
    }
  };

  const busy = st && (st.state === "running" || st.state === "muxing");
  const pct = st && st.total ? Math.round((st.done / st.total) * 100) : 0;

  return (
    <div className="mb-3 rounded-(--r-sm) bg-s3 px-3 py-2.5">
      {st?.state === "done" && job ? (
        <div className="flex items-center gap-2">
          <div className="min-w-0 flex-1">
            <div className="truncate text-[12px] font-semibold text-on">{st.filename}</div>
            <div className="text-[10.5px] text-mut">{mb(st.bytes)} · {mmss(st.duration)} · 已核对完整</div>
          </div>
          <a href={`/download/${encodeURIComponent(job)}`} download={st.filename}
            className="shrink-0 h-8 px-3 rounded-(--r-ctl) bg-primary text-on-primary text-[12px] font-semibold flex items-center pressable">
            保存到 iPhone
          </a>
        </div>
      ) : busy ? (
        <div>
          <div className="flex items-center text-[11.5px] text-on/90">
            <span>{st!.state === "muxing" ? "正在合成 MP4…" : `正在取回第 ${ep} 集`}</span>
            <span className="ml-auto text-mut">{st!.state === "muxing" ? "" : `${st!.done}/${st!.total || "…"}`}</span>
          </div>
          <div className="mt-2 h-1.5 rounded-full bg-s2 overflow-hidden">
            <div className="h-full bg-primary transition-all" style={{ width: `${st!.state === "muxing" ? 100 : pct}%` }} />
          </div>
        </div>
      ) : (
        <button onClick={start} className="w-full flex items-center gap-2 text-[12.5px] font-semibold text-on pressable">
          <IDownload size={15} />
          <span>下载第 {ep} 集，离线看</span>
          {(err || st?.state === "error") && <span className="ml-auto text-[10.5px] text-primary">重试</span>}
        </button>
      )}
      {(err || st?.state === "error") && (
        <div className="mt-1.5 text-[10.5px] text-mut">失败：{err || st?.error}</div>
      )}
      {st?.state === "done" && (
        <div className="mt-1.5 text-[10.5px] text-dim">保存后在「文件」App →「下载项」里打开，没网也能看。服务器 1 小时后清理此文件。</div>
      )}
    </div>
  );
}
