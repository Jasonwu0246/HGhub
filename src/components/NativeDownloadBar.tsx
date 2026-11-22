import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { IDownload } from "../icons";

/* Windows 桌面版「下载本集」：在本机取回、解密并拼成完整视频，存到「下载\HGhub」，离线可看。 */

type Status = {
  state: "running" | "done" | "error";
  done: number; total: number; filename: string; path: string;
  bytes: number; expected: number; error?: string | null;
};

const mb = (n: number) => `${(n / 1024 / 1024).toFixed(1)} MB`;
const mmss = (s: number) => `${Math.floor(s / 60)}:${String(Math.round(s % 60)).padStart(2, "0")}`;
const msg = (e: unknown) => (typeof e === "string" ? e : (e as Error)?.message || "下载失败");

export default function NativeDownloadBar({ videoId, ep }: { videoId: number; ep: number }) {
  const [job, setJob] = useState<string | null>(null);
  const [st, setSt] = useState<Status | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const timer = useRef<number | undefined>(undefined);

  useEffect(() => { setJob(null); setSt(null); setErr(null); }, [videoId, ep]);

  useEffect(() => {
    if (!job) return;
    let alive = true;
    const tick = async () => {
      try {
        const s = await invoke<Status>("download_status", { job });
        if (!alive) return;
        setSt(s);
        if (s.state === "running") timer.current = window.setTimeout(tick, 700);
      } catch (e) {
        if (alive) setErr(msg(e));
      }
    };
    tick();
    return () => { alive = false; window.clearTimeout(timer.current); };
  }, [job]);

  const start = async () => {
    setErr(null); setSt(null); setStarting(true);
    try {
      setJob(await invoke<string>("download_start", { id: videoId, ep }));
    } catch (e) {
      setErr(msg(e));
    } finally {
      setStarting(false);
    }
  };

  const reveal = () => { if (job) invoke("download_reveal", { job }).catch((e) => setErr(msg(e))); };
  const failed = err || st?.state === "error";
  const pct = st && st.total ? Math.round((st.done / st.total) * 100) : 0;

  return (
    <div className="mb-3 rounded-(--r-sm) bg-s3 px-3 py-2.5">
      {st?.state === "done" ? (
        <div className="flex items-center gap-2">
          <div className="min-w-0 flex-1">
            <div className="truncate text-[12px] font-semibold text-on">{st.filename}</div>
            <div className="text-[10.5px] text-mut">{mb(st.bytes)} · {mmss(st.expected)} · 已存到「下载\HGhub」</div>
          </div>
          <button onClick={reveal}
            className="shrink-0 h-8 px-3 rounded-(--r-ctl) bg-primary text-on-primary text-[12px] font-semibold pressable">
            打开文件夹
          </button>
        </div>
      ) : st?.state === "running" || starting ? (
        <div>
          <div className="flex items-center text-[11.5px] text-on/90">
            <span>{starting ? "准备中…" : `正在下载第 ${ep} 集`}</span>
            <span className="ml-auto text-mut">{st ? `${st.done}/${st.total || "…"}` : ""}</span>
          </div>
          <div className="mt-2 h-1.5 rounded-full bg-s2 overflow-hidden">
            <div className="h-full bg-primary transition-all" style={{ width: `${pct}%` }} />
          </div>
        </div>
      ) : (
        <button onClick={start} className="w-full flex items-center gap-2 text-[12.5px] font-semibold text-on pressable">
          <IDownload size={15} />
          <span>下载第 {ep} 集，离线看</span>
          {failed && <span className="ml-auto text-[10.5px] text-primary">重试</span>}
        </button>
      )}
      {failed && <div className="mt-1.5 text-[10.5px] text-mut">失败：{err || st?.error}</div>}
      {st?.state === "done" && (
        <div className="mt-1.5 text-[10.5px] text-dim">.ts 格式，用 Windows 自带「媒体播放器」或 VLC 打开，没网也能看。</div>
      )}
    </div>
  );
}
