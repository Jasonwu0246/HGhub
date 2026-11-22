import { useEffect, useRef, useState } from "react";
import { IDownload } from "../icons";

/* 网页版下载：单集，或在选集里多选、由服务器合并成一个视频。
   完成后 Safari 存到「文件」，离线可看。只在网页版构建（VITE_WEB）里出现。 */

type Status = {
  state: "checking" | "running" | "muxing" | "transcoding" | "merging" | "done" | "error";
  done: number; total: number; ep_index: number; ep_count: number; cur_ep: number;
  filename: string; bytes: number; duration: number; expected: number; error?: string | null; note?: string | null;
};

async function call<T>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, { credentials: "same-origin", ...init });
  if (r.status === 401) { window.location.href = "/login"; throw new Error("未登录"); }
  const data = await r.json().catch(() => null);
  if (!r.ok) throw new Error(typeof data === "string" ? data : `HTTP ${r.status}`);
  return data as T;
}

const mb = (n: number) => (n >= 1024 ** 3 ? `${(n / 1024 ** 3).toFixed(2)} GB` : `${(n / 1024 / 1024).toFixed(1)} MB`);
const hms = (s: number) => {
  const t = Math.round(s), h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), x = t % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(x).padStart(2, "0")}` : `${m}:${String(x).padStart(2, "0")}`;
};
const busy = (s?: Status | null) => !!s && ["checking", "running", "muxing", "transcoding", "merging"].includes(s.state);
const store = {
  get: (k: string) => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* 无痕模式等 */ } },
  del: (k: string) => { try { localStorage.removeItem(k); } catch { /* 忽略 */ } },
};

export default function DownloadBar({ videoId, ep, epNums, selecting, selected, onSelecting, onSelectAll, onClearSel }: {
  videoId: number; ep: number; epNums: number[];
  selecting: boolean; selected: number[];
  onSelecting: (on: boolean) => void; onSelectAll: () => void; onClearSel: () => void;
}) {
  const lsKey = `hg_dl_${videoId}`;
  const [job, setJob] = useState<string | null>(null);
  const [st, setSt] = useState<Status | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const timer = useRef<number | undefined>(undefined);

  // 换剧时恢复这部剧上次的下载任务（离开页面再回来也能接着看进度、保存）
  useEffect(() => {
    setSt(null); setErr(null);
    setJob(store.get(lsKey));
  }, [lsKey]);

  useEffect(() => {
    if (!job) return;
    let alive = true;
    const tick = async () => {
      try {
        const s = await call<Status>(`/api/download/status/${encodeURIComponent(job)}`);
        if (!alive) return;
        setSt(s);
        if (busy(s)) timer.current = window.setTimeout(tick, 1000);
      } catch {
        if (!alive) return;
        // 任务已过期或服务重启：回到初始状态
        store.del(lsKey); setJob(null); setSt(null);
      }
    };
    tick();
    return () => { alive = false; window.clearTimeout(timer.current); };
  }, [job, lsKey]);

  const start = async (eps: number[]) => {
    setErr(null); setSt(null);
    try {
      const r = await call<{ job: string }>("/api/download/start", {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id: videoId, eps }),
      });
      store.set(lsKey, r.job);
      setJob(r.job);
      onSelecting(false);
    } catch (e) {
      setErr((e as Error).message);
    }
  };

  const reset = () => { store.del(lsKey); setJob(null); setSt(null); setErr(null); };
  const failed = err || st?.state === "error";

  // 进度：整体 =（已完成集数 + 当前集分片比例）/ 总集数
  // 检查规格只占很小一段（0–5%），之后按集推进到 100%
  const overall = st && st.ep_count
    ? st.state === "merging" ? 100
      : st.state === "checking" ? Math.round((Math.max(st.ep_index, 1) / st.ep_count) * 5)
      : 5 + Math.round(((Math.max(st.ep_index, 1) - 1 + (st.total ? st.done / st.total : 0)) / st.ep_count) * 95)
    : 0;

  let body;
  if (selecting) {
    const all = selected.length === epNums.length && epNums.length > 0;
    body = (
      <div>
        <div className="flex items-center gap-3 text-[11.5px]">
          <span className="text-on font-semibold">已选 {selected.length} 集</span>
          <button onClick={all ? onClearSel : onSelectAll} className="text-primary pressable">{all ? "清空" : "全选"}</button>
          <button onClick={() => onSelecting(false)} className="ml-auto text-mut pressable">取消</button>
        </div>
        <button disabled={!selected.length} onClick={() => start(selected)}
          className="mt-2 w-full h-9 rounded-(--r-ctl) bg-primary text-on-primary text-[12.5px] font-semibold disabled:opacity-40 pressable">
          {selected.length > 1 ? `合并成一个视频下载（${selected.length} 集）` : selected.length === 1 ? "下载这 1 集" : "点下面的集数来选择"}
        </button>
        <div className="mt-1.5 text-[10.5px] text-dim">点集数勾选。服务器先检查各集规格，一致的无损合并；个别集不同会单独转码统一。每集一个章节。</div>
      </div>
    );
  } else if (st?.state === "done" && job) {
    body = (
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1">
          <div className="truncate text-[12px] font-semibold text-on">{st.filename}</div>
          <div className="text-[10.5px] text-mut">{mb(st.bytes)} · {hms(st.duration)}{st.ep_count > 1 ? ` · ${st.ep_count} 集` : ""} · 已核对完整</div>
          {st.note && <div className="text-[10px] text-dim">{st.note.replace("会单独转码统一", "已单独转码统一")}</div>}
        </div>
        <a href={`/download/${encodeURIComponent(job)}`} download={st.filename}
          className="shrink-0 h-8 px-3 rounded-(--r-ctl) bg-primary text-on-primary text-[12px] font-semibold flex items-center pressable">
          保存到 iPhone
        </a>
      </div>
    );
  } else if (busy(st)) {
    const s = st!;
    const k = `${Math.max(s.ep_index, 1)}/${s.ep_count}`;
    const label = s.state === "merging" ? `正在合并 ${s.ep_count} 集…`
      : s.state === "checking" ? `检查各集规格 ${k}`
      : s.ep_count > 1 ? `第 ${k} 集（第${s.cur_ep}集）${s.state === "transcoding" ? "转码统一中" : s.state === "muxing" ? "封装中" : "取回中"}`
      : s.state === "muxing" ? "正在合成 MP4…" : `正在取回第 ${s.cur_ep} 集`;
    body = (
      <div>
        <div className="flex items-center text-[11.5px] text-on/90">
          <span>{label}</span>
          <span className="ml-auto text-mut">{overall}%</span>
        </div>
        <div className="mt-2 h-1.5 rounded-full bg-s2 overflow-hidden">
          <div className="h-full bg-primary transition-all" style={{ width: `${overall}%` }} />
        </div>
        {s.note && <div className="mt-1.5 text-[10.5px] text-on/80">{s.note}</div>}
        {s.ep_count > 1 && <div className="mt-1 text-[10.5px] text-dim">可以先去看别的，回到这部剧的选集还能看到进度。{s.state === "transcoding" ? "转码比较慢，请耐心等。" : ""}</div>}
      </div>
    );
  } else {
    body = (
      <div className="flex items-center gap-2">
        <button onClick={() => start([ep])} className="flex-1 flex items-center gap-2 text-[12.5px] font-semibold text-on pressable">
          <IDownload size={15} />
          <span>下载第 {ep} 集</span>
        </button>
        <button onClick={() => { reset(); onSelecting(true); }}
          className="shrink-0 h-7 px-2.5 rounded-(--r-ctl) bg-s2 text-[11.5px] font-semibold text-primary pressable">
          多集合并下载
        </button>
      </div>
    );
  }

  return (
    <div className="mb-3 rounded-(--r-sm) bg-s3 px-3 py-2.5">
      {body}
      {failed && !selecting && (
        <div className="mt-1.5 flex items-center text-[10.5px] text-mut">
          <span className="min-w-0 flex-1">失败：{err || st?.error}</span>
          <button onClick={reset} className="ml-2 text-primary pressable">知道了</button>
        </div>
      )}
      {st?.state === "done" && !selecting && (
        <div className="mt-1.5 flex items-center text-[10.5px] text-dim">
          <span className="min-w-0 flex-1">保存后在「文件」App →「下载项」打开，没网也能看。服务器 2 小时后清理。</span>
          <button onClick={reset} className="ml-2 text-primary pressable">再下别的</button>
        </div>
      )}
    </div>
  );
}
