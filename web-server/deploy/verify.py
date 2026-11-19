#!/usr/bin/env python3
"""HGhub 网页版验真：python3 verify.py [基址，默认 http://127.0.0.1:8900]
逐项输出 PASS/FAIL；密码隐藏输入，不落盘。"""
import getpass, http.cookiejar, json, re, sys, urllib.error, urllib.parse, urllib.request

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8900").rstrip("/")
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

print(f"== 结果：{sum(results)}/{len(results)} 通过" + ("，全部通过" if all(results) else "，有失败项"))
sys.exit(0 if all(results) else 1)
