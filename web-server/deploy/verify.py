#!/usr/bin/env python3
"""HGhub 网页版验真：python3 verify.py [基址，默认 http://127.0.0.1:8900]
逐项输出 PASS/FAIL；密码隐藏输入，不落盘。"""
import getpass, http.cookiejar, json, re, sys, urllib.error, urllib.parse, urllib.request

ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
WANT_DL = "--download" in sys.argv
WANT_MERGE = "--merge" in sys.argv
BASE = (ARGS[0] if ARGS else "http://127.0.0.1:8900").rstrip("/")
results = []
def mark(name, ok, info=""):
    results.append(ok); print(f"[{'PASS' if ok else 'FAIL'}] {name}  {info}")

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k): return None
jar = http.cookiejar.CookieJar()
op = urllib.request.build_opener(NoRedirect, urllib.request.HTTPCookieProcessor(jar), urllib.request.ProxyHandler({}))

def req(path, data=None, ctype=None, timeout=40):
    url = path if path.startswith("http") else BASE + path
    r = urllib.request.Request(url, data=data, headers={"Content-Type": ctype} if ctype else {})
    try:
        with op.open(r, timeout=timeout) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers, e.read()

def invoke(cmd, **args):
    s, _, b = req(f"/api/invoke/{cmd}", json.dumps(args).encode(), "application/json", 60)
    try: body = json.loads(b or b"null")
    except Exception: body = b[:200]
    return s, body

print(f"== 目标 {BASE}")
s, h, _ = req("/")
mark("未登录访问首页被挡回登录页", s in (302, 303) and h.get("Location", "").endswith("/login"), f"{s} -> {h.get('Location')}")
s, _ = invoke("api_feed", page=1, size=1)
mark("未登录调用接口被拒", s == 401, str(s))
s, _, _ = req("/media/img?u=" + urllib.parse.quote("https://huangguoai.com/x.jpg", safe=""))
mark("未登录取媒体被拒", s == 401, str(s))

pw = getpass.getpass("网页版登录密码（不显示）：")
s, h, _ = req("/login", urllib.parse.urlencode({"password": pw}).encode(), "application/x-www-form-urlencoded")
logged = s == 303 and h.get("Location", "").endswith("/") and any(c.name == "hg_auth" for c in jar)
mark("密码登录", logged, f"{s} -> {h.get('Location')}")
if not logged: sys.exit(1)

s, _, body = req("/")
mark("登录后首页页面", s == 200 and b"<div id=\"root\">" in body, str(s))

s, feed = invoke("api_feed", page=1, size=10)
items = feed.get("items", []) if isinstance(feed, dict) else []
mark("首页 feed 返回真实数据", s == 200 and len(items) > 0,
     f"{s} 共 {len(items)} 条" + (f"，第一部《{items[0].get('title')}》" if items else f" {str(feed)[:120]}"))
if not items: sys.exit(1)

pick = next((i for i in items if i.get("cover") and i.get("id")), items[0])
vid = pick["id"]
print(f"== 选中：id={vid}《{pick.get('title')}》")
s, detail = invoke("api_video", id=vid)
eps = detail.get("episodes") or [] if isinstance(detail, dict) else []
mark("剧集详情能打开", s == 200 and isinstance(detail, dict) and bool(detail.get("title")), f"{s} 集数 {len(eps)}")

cover = (detail.get("cover") if isinstance(detail, dict) else None) or pick.get("cover")
s, h, b = req("/media/img?u=" + urllib.parse.quote(cover, safe=""))
ct = h.get("Content-Type", "")
mark("封面经代理加载（已解密为图片）", s == 200 and ct.startswith("image/") and len(b) > 500, f"{s} {ct} {len(b)}B")

ep = (eps[0].get("ep_num") if eps else None) or 1
s, play = invoke("api_play", id=vid, ep=ep)
vurl = play.get("video_url", "") if isinstance(play, dict) else ""
mark("播放地址", s == 200 and bool(vurl), f"{s} ep={ep}")
if not vurl: sys.exit(1)

def media(u):
    kind = "hls" if re.search(r"\.m3u8(\?|$)", u, re.I) else "seg"
    return req(f"/media/{kind}?u=" + urllib.parse.quote(u, safe=""))
s, h, b = media(vurl)
text = b.decode("utf-8", "replace")
mark("HLS 清单经代理 200", s == 200 and text.startswith("#EXTM3U"), f"{s} {len(b)}B")

def lines(t): return [l.strip() for l in t.splitlines() if l.strip() and not l.startswith("#")]
def keyuri(t):
    m = re.search(r'#EXT-X-KEY:[^\n]*URI="([^"]+)"', t); return m.group(1) if m else None
# 主清单 → 子清单
if "#EXT-X-STREAM-INF" in text and lines(text):
    s2, _, b2 = req(lines(text)[0]); text = b2.decode("utf-8", "replace")
    mark("子清单经代理 200", s2 == 200 and text.startswith("#EXTM3U"), str(s2))
k = keyuri(text)
if k:
    sk, _, bk = req(k); mark("解密 Key 经代理 200", sk == 200 and len(bk) == 16, f"{sk} {len(bk)}B")
segs = lines(text)
if segs:
    ss, h, bs = req(segs[0]); mark("视频分片经代理 200", ss == 200 and len(bs) > 1000, f"{ss} {len(bs)}B")
    if len(segs) > 1:
        ss, _, bs = req(segs[-1]); mark("末尾分片经代理 200", ss == 200 and len(bs) > 1000, f"{ss} {len(bs)}B")
else:
    mark("视频分片经代理 200", False, "清单里没有分片行")

if WANT_DL:
    import os, shutil, subprocess, tempfile, time
    print(f"== 下载本集：id={vid} 第{ep}集")
    s, _, b = req("/api/download/start", json.dumps({"id": vid, "ep": ep}).encode(), "application/json", 60)
    job = (json.loads(b or b"{}") or {}).get("job") if s == 200 else None
    mark("下载任务已创建", bool(job), f"{s} {b[:120]!r}" if not job else job)
    st = {}
    t0 = time.time()
    while job and time.time() - t0 < 900:
        s, _, b = req(f"/api/download/status/{job}")
        st = json.loads(b or b"{}")
        if st.get("state") in ("done", "error"): break
        print(f"   {st.get('state')} {st.get('done')}/{st.get('total')}", end="\r", flush=True)
        time.sleep(2)
    print()
    mark("服务器合成 MP4", st.get("state") == "done",
         f"{st.get('filename')} {st.get('bytes', 0)/1048576:.1f}MB 成片{st.get('duration', 0):.0f}s / 清单{st.get('expected', 0):.0f}s / 接口{play.get('duration')}s" if st.get("state") == "done" else str(st.get("error")))
    if st.get("state") == "done":
        tmp = tempfile.mkdtemp()
        path = os.path.join(tmp, "ep.mp4")
        s, h, b = req(f"/download/{job}", timeout=600)
        open(path, "wb").write(b)
        disp = h.get("Content-Disposition", "")
        mark("文件下载（附件形式）", s == 200 and len(b) == st["bytes"] and "attachment" in disp, f"{s} {len(b)}B")
        if shutil.which("ffprobe"):
            dur = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path],
                                       capture_output=True, text=True).stdout.strip() or 0)
            dec = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-f", "null", "-"], capture_output=True, text=True)
            mark("成片完整解码无错误", dec.returncode == 0 and not dec.stderr.strip(), dec.stderr.strip()[:160])
            ref = st.get("expected") or play.get("duration") or 0
            mark("成片时长与原片一致", ref > 0 and abs(dur - ref) <= ref * 0.03 + 2, f"{dur:.1f}s vs {ref:.1f}s")
        shutil.rmtree(tmp, ignore_errors=True)

if WANT_MERGE:
    import os, shutil, subprocess, tempfile, time
    eps2 = [e.get("ep_num") for e in eps[:2] if e.get("ep_num")]
    if len(eps2) < 2:
        mark("多集合并（需要至少 2 集）", False, f"这部剧只有 {len(eps)} 集")
    else:
        print(f"== 多集合并：id={vid} 第{eps2}集")
        s, _, b = req("/api/download/start", json.dumps({"id": vid, "eps": eps2}).encode(), "application/json", 60)
        job = (json.loads(b or b"{}") or {}).get("job") if s == 200 else None
        mark("合并任务已创建", bool(job), job or f"{s} {b[:120]!r}")
        st, t0 = {}, time.time()
        while job and time.time() - t0 < 1800:
            s, _, b = req(f"/api/download/status/{job}")
            st = json.loads(b or b"{}")
            if st.get("state") in ("done", "error"): break
            print(f"   {st.get('state')} 第{st.get('ep_index')}/{st.get('ep_count')}集 {st.get('done')}/{st.get('total')}   ", end="\r", flush=True)
            time.sleep(2)
        print()
        ok = st.get("state") == "done"
        mark("服务器合并成一个 MP4", ok, f"{st.get('filename')} {st.get('bytes', 0)/1048576:.1f}MB {st.get('duration', 0):.0f}s" if ok else str(st.get("error")))
        if ok:
            tmp = tempfile.mkdtemp(); path = os.path.join(tmp, "m.mp4")
            s, h, b = req(f"/download/{job}", timeout=1800); open(path, "wb").write(b)
            mark("合并文件下载", s == 200 and len(b) == st["bytes"], f"{s} {len(b)}B")
            if shutil.which("ffprobe"):
                dur = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip() or 0)
                chs = subprocess.run(["ffprobe", "-v", "error", "-show_chapters", "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip().splitlines()
                dec = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-f", "null", "-"], capture_output=True, text=True)
                mark("合并成片完整解码无错误", dec.returncode == 0 and not dec.stderr.strip(), dec.stderr.strip()[:160])
                mark("合并时长 = 各集清单之和", abs(dur - st["expected"]) <= st["expected"] * 0.03 + 2, f"{dur:.1f}s vs {st['expected']:.1f}s")
                mark("每集一个章节", len(chs) == len(eps2), f"{len(chs)} 个")
            shutil.rmtree(tmp, ignore_errors=True)

print(f"== 结果：{sum(results)}/{len(results)} 通过" + ("，全部通过" if all(results) else "，有失败项"))
sys.exit(0 if all(results) else 1)
