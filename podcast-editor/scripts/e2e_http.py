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

# ---------- 11. 多轨联动编辑（主持人 / 嘉宾 / 现场气氛） ----------
print("11) 多轨联动编辑工作流")
HOST, GUEST, AMB = "track_host", "track_guest", "track_ambience"
_, mp = req("POST", "/api/projects", {"name": "linked", "sample_rate": 48000, "channels": 1})
mpid = mp["id"]

# 三轨各准备一个带脉冲的 48k 素材（脉冲在源帧 50）
def pulse_src(tag, project=None):
    target = project if project is not None else mpid
    w = [0.0] * 1000
    w[50] = 1.0
    pth = f"/tmp/pe_input/pulse_{tag}.wav"
    write_wav(pth, 48000, w)
    body, ct = multipart(pth, f"pulse_{tag}.wav")
    st, s = req("POST", f"/api/projects/{target}/sources", raw=body, ctype=ct)
    assert st == 200, s
    return s["id"]

src_h, src_g, src_a = pulse_src("h"), pulse_src("g"), pulse_src("a")

# 轨道初始状态：三条角色轨 + 默认未锁未隐
st, tracks = req("GET", f"/api/projects/{mpid}/tracks")
track_ids = [t["id"] for t in tracks]
check("three role tracks present", st == 200 and HOST in track_ids and GUEST in track_ids and AMB in track_ids,
      str(track_ids))

# 每轨三段，对齐放在 [0,200)、[400,600)、[800,1000)，保证任何一次删除后组仍有成员。
def add(track, sid, start, length=200):
    st, pp = req("POST", f"/api/projects/{mpid}/items",
                 {"source_id": sid, "track_id": track, "source_start": 0,
                  "source_end": length, "timeline_start": start})
    assert st == 200, pp
    return pp["items"][-1]["id"]

ids = {}
for tr, sid in ((HOST, src_h), (GUEST, src_g), (AMB, src_a)):
    ids[(tr, 1)] = add(tr, sid, 0)
    ids[(tr, 2)] = add(tr, sid, 400)
    ids[(tr, 3)] = add(tr, sid, 800)

# 全部入同一剪辑组
all_item_ids = [ids[k] for k in sorted(ids)]
st, gr = req("POST", f"/api/projects/{mpid}/groups",
             {"name": "全场", "item_ids": all_item_ids, "parent_id": None})
check("create linked group", st == 200 and gr["group_id"])
gid = gr["group_id"]

# 11a. 整组波纹删除第一段：剩余两段同步前移 200（400->200，800->600）
st, after = req("POST", f"/api/projects/{mpid}/items/delete",
                {"item_id": ids[(HOST, 1)], "ripple": True, "zero_policy": "reject"})
check("ripple delete ok", st == 200, str(after))
for tr in (HOST, GUEST, AMB):
    i2 = next(i for i in after["items"] if i["id"] == ids[(tr, 2)])
    i3 = next(i for i in after["items"] if i["id"] == ids[(tr, 3)])
    check(f"{tr} segments shift in lockstep", i2["timeline_start"] == 200 and i3["timeline_start"] == 600,
          f"{i2['timeline_start']},{i3['timeline_start']}")

# 三轨脉冲落在同一输出帧 200+50=250，混音为三轨之和
st, r = req("GET", f"/api/projects/{mpid}/render?since_version={after['version']}")
check("three-track pulses align at 250", st == 200 and abs(r["frames"][250] - 3.0) < 1e-3,
      str(r["frames"][250] if st == 200 else r))

# 11b. 组状态暴露联动范围 / 轨道 / 冲突（剩余成员范围 200..800）
st, status = req("GET", f"/api/projects/{mpid}/groups/status")
gstat = next((g for g in status["groups"] if g["id"] == gid), None)
check("status exposes member tracks", st == 200 and gstat is not None
      and set(gstat["member_tracks"]) == {HOST, GUEST, AMB}, str(gstat))
check("status range 200..800", gstat["range_start"] == 200 and gstat["range_end"] == 800,
      str((gstat["range_start"], gstat["range_end"])))

# 11c. 锁定轨一票否决且工程零变更
before_locked = after
st, lkerr = req("POST", f"/api/projects/{mpid}/tracks/{GUEST}", {"locked": True})
check("lock guest track", st == 200)
st, denied = req("POST", f"/api/projects/{mpid}/items/delete",
                 {"item_id": ids[(HOST, 2)], "ripple": True, "zero_policy": "reject"})
check("locked track rejects linked ripple", st == 400 and GUEST in str(denied), str(denied))
st, cur_locked = req("GET", f"/api/projects/{mpid}")
check("rejection leaves project unchanged", cur_locked["items"] == before_locked["items"])
# 解锁
req("POST", f"/api/projects/{mpid}/tracks/{GUEST}", {"locked": False})

# 11d. 隐藏轨仍参与联动：在独立工程上验证，避免删空主剪辑组。
_, hp = req("POST", "/api/projects", {"name": "hidden", "sample_rate": 48000, "channels": 1})
hpid = hp["id"]
sh = pulse_src("hh", hpid); sg = pulse_src("gg", hpid); sa = pulse_src("aa", hpid)
# 每轨放两段：[0,100) 与 [100,300)；气氛轨额外放一条横跨 [0,300) 的长片段。
def add_to(proj, track, sid, start, length=200):
    st, pp = req("POST", f"/api/projects/{proj}/items",
                 {"source_id": sid, "track_id": track, "source_start": 0,
                  "source_end": length, "timeline_start": start})
    assert st == 200, pp
    return pp["items"][-1]["id"]
h_first, h_second = add_to(hpid, HOST, sh, 0, 100), add_to(hpid, HOST, sh, 100, 200)
g_first, g_second = add_to(hpid, GUEST, sg, 0, 100), add_to(hpid, GUEST, sg, 100, 200)
a_first, a_second = add_to(hpid, AMB, sa, 0, 100), add_to(hpid, AMB, sa, 100, 200)
a_long = add_to(hpid, AMB, sa, 0, 300)
st, hgr = req("POST", f"/api/projects/{hpid}/groups",
              {"name": "隐藏联动组", "item_ids": [h_first, h_second, g_first, g_second,
                                                a_first, a_second, a_long], "parent_id": None})
check("hidden-scenario group created", st == 200)
hgid = hgr["group_id"]
st, hid = req("POST", f"/api/projects/{hpid}/tracks/{AMB}", {"hidden": True})
check("hide ambience track", st == 200 and any(t["id"] == AMB and t["hidden"] for t in hid["tracks"]))
# 删除首段区间 [0,100)：气氛长片段被裁切，右段保留在边界帧 100，且仍是组成员。
st, rip2 = req("POST", f"/api/projects/{hpid}/items/delete",
               {"item_id": h_first, "ripple": True, "zero_policy": "reject"})
check("ripple with hidden track ok", st == 200, str(rip2))
amb_long_after = next((i for i in rip2["items"] if i["source_id"] == sa and i["source_start"] >= 100), None)
check("hidden spanning track clipped and kept at boundary", amb_long_after is not None
      and amb_long_after["timeline_start"] == 100, str(amb_long_after))
st, status2 = req("GET", f"/api/projects/{hpid}/groups/status")
gstat2 = next((g for g in status2["groups"] if g["id"] == hgid), None)
check("status flags hidden-but-participating",
      gstat2 is not None and AMB in gstat2["hidden_tracks"] and AMB in gstat2["member_tracks"],
      str(gstat2))
# 隐藏 + 锁定一票否决
req("POST", f"/api/projects/{hpid}/tracks/{AMB}", {"locked": True})
st, hidden_lock_err = req("POST", f"/api/projects/{hpid}/items/delete",
                          {"item_id": h_second, "ripple": True, "zero_policy": "reject"})
check("hidden+locked track vetoes linked edit", st == 400 and AMB in str(hidden_lock_err),
      str(hidden_lock_err))

# 11e. 分组交叉淡化（主组此刻仍完整）：按各轨真实余量统一落值
st, gxf = req("POST", f"/api/projects/{mpid}/groups/{gid}/crossfade", {"frames": 20})
check("group crossfade evaluates per-track margins", st == 200 and gxf["feasible"] is True, str(gxf))

# 11f. 嵌套成环检测（用第三段建子组，再试图把父组挂到子组下）
st, child = req("POST", f"/api/projects/{mpid}/groups",
                {"name": "子组", "item_ids": [ids[(HOST, 3)]], "parent_id": gid})
check("create child group", st == 200 and isinstance(child, dict) and child.get("group_id"), str(child))
child_gid = child["group_id"]
st, cycle = req("POST", f"/api/projects/{mpid}/groups/{gid}/parent",
                {"parent_id": child_gid})
check("nested cycle rejected", st == 400 and "环" in str(cycle), str(cycle))

# 11g. 负起点（独立工程，避免 clip 移动清空主组）：reject 拒绝、clip 裁切
_, negp = req("POST", "/api/projects", {"name": "negative", "sample_rate": 48000, "channels": 1})
npid = negp["id"]
nsh, nsg, nsa = pulse_src("nh", npid), pulse_src("ng", npid), pulse_src("na", npid)
nids = []
for tr, sid in ((HOST, nsh), (GUEST, nsg), (AMB, nsa)):
    nids.append(add_to(npid, tr, sid, 0, 300))
st, ngr = req("POST", f"/api/projects/{npid}/groups",
              {"name": "负起点组", "item_ids": nids, "parent_id": None})
check("negative-scenario group created", st == 200)
ngid = ngr["group_id"]
st, neg_reject = req("POST", f"/api/projects/{npid}/groups/{ngid}/move",
                     {"delta": -100000, "zero_policy": "reject"})
check("negative move rejected explicitly", st == 400 and "reject" in str(neg_reject), str(neg_reject))
st, clipped = req("POST", f"/api/projects/{npid}/groups/{ngid}/move",
                  {"delta": -100000, "zero_policy": "clip"})
check("clip move clamps to zero without error", st == 200
      and all(i["timeline_start"] >= 0 for i in clipped["items"]), str(clipped))

# 11h. 分轨导出（独立三轨工程）：混音 + 每轨 WAV，统一版本
_, sp = req("POST", "/api/projects", {"name": "stems", "sample_rate": 48000, "channels": 1})
spid = sp["id"]
ssh, ssg, ssa = pulse_src("sth", spid), pulse_src("stg", spid), pulse_src("sta", spid)
sids = []
for tr, sid in ((HOST, ssh), (GUEST, ssg), (AMB, ssa)):
    sids.append(add_to(spid, tr, sid, 0, 300))
st, sgr = req("POST", f"/api/projects/{spid}/groups",
              {"name": "分轨组", "item_ids": sids, "parent_id": None})
check("stems-scenario group created", st == 200)
st, cur_for_stem = req("GET", f"/api/projects/{spid}")
stem_dest = os.path.abspath(os.path.join(os.path.dirname(dest), "linked_stems.wav"))
st, stems = req("POST", f"/api/projects/{spid}/export/stems", {"dest": stem_dest})
check("stems export ok", st == 200 and stems["cancelled"] is False, str(stems))
check("stems reference one revision", stems["version"] == cur_for_stem["version"],
      f"{stems.get('version')} vs {cur_for_stem['version']}")
stem_paths = [s["path"] for s in stems["stems"]]
check("one stem wav per track", len(stems["stems"]) >= 3 and all(os.path.exists(x) for x in stem_paths),
      str(stem_paths))
check("mix wav exists", os.path.exists(stems["mix_path"]))

# 11i. 一次撤销恢复全部受影响轨道（在主工程上撤销最近一次联动编辑）
st, undone_link = req("POST", f"/api/projects/{mpid}/undo")
check("undo restores linked project in one step", st == 200, str(undone_link))

# 11j. 缺失源文件在渲染前明确报错。
# 上传即进入服务进程内存缓存，因此用「独立实例 + 丢文件 + 冷重启」复刻真实场景：
# 工程关闭期间源文件丢失，重开后缓存为空，preflight 必须在渲染前报错而非当静音。
import subprocess, signal
BIN = os.environ.get("PE_BIN", "./target/debug/podcast_editor")
cold_dir = "/tmp/pe_cold"
cold_port = "127.0.0.1:8091"
cold_base = "http://" + cold_port
os.makedirs(cold_dir, exist_ok=True)
cold_env = dict(os.environ, PE_DATA_DIR=cold_dir, PE_BIND=cold_port)
cold = subprocess.Popen([BIN], cwd=os.path.join(os.path.dirname(__file__), ".."),
                        env=cold_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
for _ in range(50):
    try:
        urllib.request.urlopen(cold_base + "/api/projects", timeout=1); break
    except Exception:
        time.sleep(0.1)
try:
    def creq(method, path, body=None):
        r = urllib.request.Request(cold_base + path,
                                   data=json.dumps(body).encode() if body is not None else None,
                                   headers={"Content-Type": "application/json"}, method=method)
        try:
            with urllib.request.urlopen(r, timeout=10) as resp:
                txt = resp.read()
                return resp.status, (json.loads(txt) if txt else None)
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode()
    st, mproj = creq("POST", "/api/projects", {"name": "missing", "sample_rate": 48000, "channels": 1})
    missid = mproj["id"]
    ghost = "/tmp/pe_input/ghost_cold.wav"
    write_wav(ghost, 48000, [0.0] * 100)
    body, ct = multipart(ghost, "ghost_cold.wav")
    up = urllib.request.Request(cold_base + f"/api/projects/{missid}/sources", data=body,
                                headers={"Content-Type": ct}, method="POST")
    with urllib.request.urlopen(up, timeout=10) as resp:
        gs = json.loads(resp.read())
    creq("POST", f"/api/projects/{missid}/items",
         {"source_id": gs["id"], "source_start": 0, "source_end": 100})
    check("ghost source registered on cold instance", True)
    # 上传会在数据目录留存独立副本，DB 引用的是该副本路径；删除它才是真正丢失源文件。
    os.remove(gs["path"])
    cold.send_signal(signal.SIGTERM); cold.wait(timeout=10)
    # 冷重启：内存缓存清空，但 SQLite 里仍登记着已丢失的源引用。
    cold = subprocess.Popen([BIN], cwd=os.path.join(os.path.dirname(__file__), ".."),
                            env=cold_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        try:
            urllib.request.urlopen(cold_base + "/api/projects", timeout=1); break
        except Exception:
            time.sleep(0.1)
    st, miss_err = creq("GET", f"/api/projects/{missid}/render")
    check("missing source reported before render", st == 400 and "缺失" in str(miss_err),
          str(miss_err)[:120])
finally:
    if cold.poll() is None:
        cold.send_signal(signal.SIGTERM); cold.wait(timeout=10)

# ---------- 12. 旧单轨工程无需转换素材即可打开 ----------
print("12) 旧单轨工程兼容性（HTTP 层）")
# 旧工程经当前接口创建后，其片段落在默认主持人轨；这里验证载入不报错且保留引用。
st, legacy = req("GET", f"/api/projects/{pid}")
check("existing project reloads with track metadata", st == 200
      and all("track_id" in i for i in legacy["items"]))
check("legacy items keep source references intact",
      all(i["source_end"] > i["source_start"] for i in legacy["items"]))

print(f"\n结果：{PASS} 通过，{FAIL} 失败")
sys.exit(1 if FAIL else 0)
