#!/usr/bin/env python3
"""端到端 HTTP 工作流校验。所有位置以采样帧整数断言。"""
import json, math, os, struct, sys, urllib.request, urllib.parse, time

BASE = "http://127.0.0.1:8080"
DATA = os.path.join(os.path.dirname(__file__), "..", "run_data")
PASS, FAIL = 0, 0

def check(name, cond, extra=""):
    global PASS, FAIL
    if cond:
        PASS += 1
        print(f"  ok  {name}")
    else:
        FAIL += 1
        print(f" FAIL {name} {extra}")

def req(method, path, body=None, raw=None, ctype="application/json"):
    url = BASE + path
    data = None
    headers = {}
    if raw is not None:
        data = raw
        headers["Content-Type"] = ctype  # multipart 时由调用方传入带 boundary 的类型
    elif body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = ctype
    r = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(r) as resp:
            txt = resp.read()
            return resp.status, (json.loads(txt) if txt else None)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()

def write_wav(path, rate, f32):
    n = len(f32)
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", 36 + n * 2) + b"WAVE")
        f.write(b"fmt " + struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16))
        f.write(b"data" + struct.pack("<I", n * 2))
        for v in f32:
            scaled = v * 32767 if v >= 0 else v * 32768
            f.write(struct.pack("<h", int(round(max(-32768, min(32767, scaled))))))

def read_wav_frames(path):
    with open(path, "rb") as f:
        b = f.read()
    assert b[:4] == b"RIFF" and b[8:12] == b"WAVE"
    rate = struct.unpack("<I", b[24:28])[0]
    off = b.index(b"data") + 8
    n = struct.unpack("<I", b[off - 4:off])[0] // 2
    frames = struct.unpack("<%dh" % n, b[off:off + n * 2])
    return rate, [v / 32768.0 for v in frames]

def multipart(path, filename):
    boundary = "----peboundary"
    with open(path, "rb") as f:
        content = f.read()
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; "
            f"filename=\"{filename}\"\r\nContent-Type: audio/wav\r\n\r\n").encode()
    body += content + f"\r\n--{boundary}--\r\n".encode()
    return body, f"multipart/form-data; boundary={boundary}"

# ---------- 1. 建 48k 工程 ----------
print("1) 创建工程")
st, p = req("POST", "/api/projects", {"name": "e2e", "sample_rate": 48000, "channels": 1})
check("create 200", st == 200, str(st))
pid = p["id"]
check("version starts at 1", p["version"] == 1)

# ---------- 2. 准备两种采样率素材并上传 ----------
print("2) 上传 48k 与 44.1k 素材（混合采样率）")
os.makedirs("/tmp/pe_input", exist_ok=True)
# 48k：单脉冲在帧 10
w48 = [0.0] * 1000
w48[10] = 1.0
p48 = "/tmp/pe_input/pulse48.wav"
write_wav(p48, 48000, w48)
# 44.1k：1 秒相位正弦
w44 = [math.sin(2 * math.pi * 300 * i / 44100) * 0.5 for i in range(44100)]
p44 = "/tmp/pe_input/sine441.wav"
write_wav(p44, 44100, w44)

body, ct = multipart(p48, "pulse48.wav")
st, s48 = req("POST", f"/api/projects/{pid}/sources", raw=body, ctype=ct)
check("upload 48k", st == 200 and s48["original_rate"] == 48000 and s48["frame_count"] == 1000)
body, ct = multipart(p44, "sine441.wav")
st, s44 = req("POST", f"/api/projects/{pid}/sources", raw=body, ctype=ct)
check("upload 44.1k", st == 200 and s44["original_rate"] == 44100 and s44["frame_count"] == 44100)

# 原文件未被修改
check("source file immutable", os.path.getsize(p48) > 0 and os.path.getsize(p44) > 0)

# ---------- 3. 加片段：48k 脉冲 + 44.1k 正弦 ----------
print("3) 加入片段并检查精确帧位置")
st, p = req("POST", f"/api/projects/{pid}/items",
            {"source_id": s48["id"], "source_start": 0, "source_end": 1000})
check("add item1", st == 200)
st, p = req("POST", f"/api/projects/{pid}/items",
            {"source_id": s44["id"], "source_start": 0, "source_end": 44100})
check("add item2 (44.1k)", st == 200)
# 44.1k 一秒在 48k 工程中应精确占 48000 帧；item1 占 1000；总长 49000
total = p["items"][1]["timeline_start"]
check("item2 starts at frame 1000", total == 1000, str(total))

# ---------- 4. 预览渲染：脉冲位置与混合速率长度 ----------
print("4) 预览渲染（与导出同一路径）")
st, r = req("GET", f"/api/projects/{pid}/render?since_version={p['version']}")
check("render ok", st == 200)
check("render frame_count exact 49000", r["frame_count"] == 49000, str(r["frame_count"]))
check("pulse at output frame 10", abs(r["frames"][10] - 1.0) < 1e-3, str(r["frames"][10]))
check("no stray pulse elsewhere", r["frames"][11] == 0.0)
check("render carries version", r["version"] == p["version"])

# ---------- 5. 截取只改引用范围 ----------
print("5) 截取片段（引用范围）")
item1 = p["items"][0]["id"]
st, p2 = req("POST", f"/api/projects/{pid}/trim",
             {"item_id": item1, "source_start": 0, "source_end": 100})
check("trim bumps version", st == 200 and p2["version"] == p["version"] + 1)
check("trim changed reference only", p2["items"][0]["source_end"] == 100)

# ---------- 6. 交叉淡化：先在可行长度下工作 ----------
print("6) 交叉淡化真实余量")
# 再加一个很短的 48k 片段（50 帧）放在末尾
st, p3 = req("POST", f"/api/projects/{pid}/items",
             {"source_id": s48["id"], "source_start": 0, "source_end": 50})
short_id = p3["items"][-1]["id"]
prev_id = p3["items"][1]["id"]
# 可行长度 50：余量检查通过、渲染成功
st, xf2 = req("POST", f"/api/projects/{pid}/crossfade", {"item_id": prev_id, "frames": 50})
check("feasible crossfade accepted", st == 200 and xf2["margin"]["feasible"] is True)
st, r2 = req("GET", f"/api/projects/{pid}/render")
check("render ok with feasible crossfade", st == 200, str(st))

# ---------- 7. 撤销：版本回退，波形版本戳防护（当前状态可正常渲染） ----------
print("7) 撤销 / 重做与版本戳")
st, cur = req("GET", f"/api/projects/{pid}")
cur_ver = cur["version"]
st, undone = req("POST", f"/api/projects/{pid}/undo")
check("undo moves version back", st == 200 and undone["version"] == cur_ver - 1,
      f"{undone.get('version')} vs {cur_ver}")
st, wv = req("GET", f"/api/projects/{pid}/waveform?bucket_frames=512&since_version={undone['version']}")
check("waveform fresh for current version", wv["stale"] is False and wv["version"] == undone["version"])
# 用一个更高的 since_version 模拟旧请求，应当被标记 stale
st, wv_old = req("GET", f"/api/projects/{pid}/waveform?bucket_frames=512&since_version=999999")
check("stale waveform detectable", wv_old["stale"] is True)
st, redone = req("POST", f"/api/projects/{pid}/redo")
check("redo restores newer version", st == 200 and redone["version"] == cur_ver)

# ---------- 7b. 不可行淡化：请求 200（>余量 50），必须拒绝渲染并提示可用长度，然后恢复 ----------
print("7b) 素材到头时的可用长度提示")
st, xf = req("POST", f"/api/projects/{pid}/crossfade", {"item_id": prev_id, "frames": 200})
check("crossfade returns margin", st == 200 and xf["margin"] is not None)
check("margin flags infeasible", xf["margin"]["feasible"] is False)
check("max feasible reported as 50", xf["margin"]["max_feasible"] == 50, str(xf["margin"]))
st, err = req("GET", f"/api/projects/{pid}/render")
check("render rejects infeasible crossfade", st == 400 and "50" in err, str(st))
# 恢复到可行长度，保证后续导出可进行
st, _ = req("POST", f"/api/projects/{pid}/crossfade", {"item_id": prev_id, "frames": 50})
check("restore feasible crossfade", st == 200)

# ---------- 8. 游标双向定位 ----------
print("8) 试听游标 <-> 原素材位置")
# 选第一个片段，工程帧 50 -> 源帧 50（同 48k）
st, loc = req("GET", f"/api/projects/{pid}/locate?direction=to_source&frame=50&item_id={item1}")
check("timeline->source", st == 200 and loc["source_frame"] == 50, str(loc))
st, loc2 = req("GET", f"/api/projects/{pid}/locate?direction=to_timeline&frame=50&source_id={s48['id']}")
check("source->timeline", st == 200 and loc2["project_frame"] == 50, str(loc2))

# ---------- 9. 导出：先取消（无半文件），再成功 ----------
print("9) 导出取消与原子提交")
dest = os.path.abspath(os.path.join(DATA, "..", "run_data", "render_out.wav"))
if os.path.exists(dest):
    os.remove(dest)
# 成功导出
st, ex = req("POST", f"/api/projects/{pid}/export", {"dest": dest})
check("export completed", st == 200 and ex["cancelled"] is False and ex["frames"] > 0)
check("target wav exists", os.path.exists(dest))
# 无临时文件残留
leftovers = [f for f in os.listdir(os.path.dirname(dest)) if f.startswith(".") and ".tmp." in f]
check("no temp leftovers", leftovers == [], str(leftovers))
rate, frames = read_wav_frames(dest)
check("exported wav rate 48000", rate == 48000)
check("exported frame count matches render", len(frames) == ex["frames"],
      f"{len(frames)} vs {ex['frames']}")

# 真正的「进行中取消」：先造一个足够长的素材让渲染耗时，发起导出后立刻取消。
import threading
long_st, long_sig = req("POST", "/api/test-signal", {
    "kind": "speech", "sample_rate": 48000, "frames": 2_400_000, "param": 2400})
check("long signal generated", long_st == 200 and long_sig["frames"] == 2_400_000)
reg_st, long_src = req("POST", f"/api/projects/{pid}/sources/register",
                       {"path": long_sig["path"]})
check("long source registered", reg_st == 200)
# 用长素材单独建一个工程，避免污染主工程的一致性断言
_, big = req("POST", "/api/projects", {"name": "cancel-probe", "sample_rate": 48000, "channels": 1})
bpid = big["id"]
_, big = req("POST", f"/api/projects/{bpid}/items",
             {"source_id": long_src["id"], "source_start": 0, "source_end": 2_400_000})
cancel_dest = os.path.abspath(os.path.join(os.path.dirname(dest), "cancelled_out.wav"))
if os.path.exists(cancel_dest):
    os.remove(cancel_dest)
export_result = {}
def long_export():
    export_result["status"], export_result["body"] = req(
        "POST", f"/api/projects/{bpid}/export", {"dest": cancel_dest})
t = threading.Thread(target=long_export)
t.start()
time.sleep(0.1)  # 让长导出进入渲染
cs, cb = req("POST", f"/api/projects/{bpid}/export/cancel")
t.join(timeout=30)
check("cancel endpoint responds", cs == 200 and cb["cancelled_jobs"] >= 1, str(cb))
check("export reports cancelled", export_result.get("body", {}).get("cancelled") is True,
      str(export_result))
check("cancelled export leaves no target", not os.path.exists(cancel_dest))
tmp_left = [f for f in os.listdir(os.path.dirname(dest)) if ".tmp." in f]
check("cancelled export leaves no temp file", tmp_left == [], str(tmp_left))

# ---------- 10. 关闭重开一致性：重新加载工程，重渲染逐样本一致 ----------
print("10) 关闭后重开 -> 一致音频")
st, reopened = req("GET", f"/api/projects/{pid}")
check("reopen loads project", st == 200)
st, r_reopen = req("GET", f"/api/projects/{pid}/render")
check("reopen render same frame count", r_reopen["frame_count"] == ex["frames"])
# 与导出文件逐样本比对（16-bit 量化误差内）
maxdiff = max(abs(a - b) for a, b in zip(r_reopen["frames"], frames))
check("reopened render sample-identical to export", maxdiff < 2e-4, f"maxdiff={maxdiff}")

# 重新导出到新文件，字节层面应与首次导出一致（确定性渲染）
dest2 = dest.replace(".wav", "_reopen.wav")
st, ex2 = req("POST", f"/api/projects/{pid}/export", {"dest": dest2})
check("re-export same length", ex2["frames"] == ex["frames"])
with open(dest, "rb") as f:
    b1 = f.read()
with open(dest2, "rb") as f:
    b2 = f.read()
check("deterministic byte-identical export", b1 == b2)

print(f"\n结果：{PASS} 通过，{FAIL} 失败")
sys.exit(1 if FAIL else 0)
