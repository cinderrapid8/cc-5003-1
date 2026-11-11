//! 多轨联动编辑用例（主持人 / 嘉宾 / 现场气氛三轨）。
//!
//! 覆盖任务要求的全部关键不变量：
//! * 相同脉冲分布在三轨，多次整组波纹删除后相对位置保持正确；
//! * 锁定轨（含被隐藏的锁定轨）一票否决，工程零变更；
//! * 隐藏轨仍参与组规则，不会因未显示失去同步；
//! * 不同录音起点用独立偏移表达，负起点按 reject / clip 策略显式处理；
//! * 交叉淡化按每轨真实素材余量计算，一轨不足不改变整组时长；
//! * 嵌套剪辑组循环检查；批量移动原子保存（一次快照、可整单撤销）；
//! * 旧单轨工程无需转换素材即可打开；
//! * 缺失源文件渲染前明确报错；
//! * 混音与分轨由同一工程修订渲染，逐样本一致。

use std::collections::HashMap;
use std::sync::Arc;

#[path = "../src/domain.rs"]
mod domain;
#[path = "../src/db.rs"]
mod db;
#[path = "../src/edit.rs"]
mod edit;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/link.rs"]
mod link;
#[path = "../src/render.rs"]
mod render;
#[path = "../src/resample.rs"]
mod resample;
#[path = "../src/signals.rs"]
mod signals;
#[path = "../src/wav.rs"]
mod wav;

use domain::{EditGroup, Frame, Item, Project, Source, TRACK_AMBIENCE, TRACK_GUEST, TRACK_HOST};
use link::ZeroPolicy;
use render::CancelToken;
use wav::WavData;

const RATE: u32 = 48000;

fn wav_pulse(frames: usize, pulse_at: usize) -> WavData {
    signals::single_pulse(RATE, frames, pulse_at)
}

fn source_meta(id: &str, frames: i64) -> Source {
    Source {
        id: id.into(),
        path: format!("{id}.wav"),
        original_rate: RATE,
        channels: 1,
        frame_count: frames,
    }
}

fn loaded_map(ids: &[&str], frames: usize, pulse_at: usize) -> render::LoadedSources {
    let mut m = render::LoadedSources::new();
    for id in ids {
        m.insert((*id).to_string(), Arc::new(wav_pulse(frames, pulse_at)));
    }
    m
}

fn rate_map(ids: &[&str]) -> HashMap<String, u32> {
    ids.iter().map(|id| ((*id).to_string(), RATE)).collect()
}

/// 三轨工程；可给各轨设置不同录音起点偏移（允许负起点）。
fn three_track_project(track_offsets: [Frame; 3]) -> Project {
    let mut tracks = Project::default_tracks();
    tracks[0].start_offset = track_offsets[0];
    tracks[1].start_offset = track_offsets[1];
    tracks[2].start_offset = track_offsets[2];
    Project {
        id: "p".into(),
        name: "multi".into(),
        sample_rate: RATE,
        channels: 1,
        version: 1,
        tracks,
        groups: vec![],
        items: vec![],
        silences: vec![],
    }
}

fn item(id: &str, source_id: &str, track_id: &str, start: Frame, len: Frame) -> Item {
    Item {
        id: id.into(),
        source_id: source_id.into(),
        track_id: track_id.into(),
        source_start: 0,
        source_end: len,
        timeline_start: start,
        crossfade_out: 0,
    }
}

/// 在三轨各放一个 300 帧片段，时间对齐于 start，组成一个联动剪辑组。
fn aligned_group_project(start: Frame) -> (Project, String) {
    let mut p = three_track_project([0, 0, 0]);
    p.items.push(item("h", "sh", TRACK_HOST, start, 300));
    p.items.push(item("g", "sg", TRACK_GUEST, start, 300));
    p.items.push(item("a", "sa", TRACK_AMBIENCE, start, 300));
    let (p2, gid) = link::create_group(p, "对话组", vec!["h".into(), "g".into(), "a".into()], None)
        .unwrap();
    (p2, gid)
}

fn sources_for(ids: &[&str], frames: i64) -> HashMap<String, Source> {
    ids.iter().map(|id| ((*id).to_string(), source_meta(id, frames))).collect()
}

// ---------- 1. 三轨脉冲：多次整组波纹删除后相对位置保持正确 ----------

#[test]
fn repeated_group_ripple_keeps_three_tracks_aligned() {
    // 三轨各两段（各 200 帧），对齐放在 [0,200) 与 [400,600)，中间留 200 帧间隔。
    let mut p = three_track_project([0, 0, 0]);
    let tracks = [TRACK_HOST, TRACK_GUEST, TRACK_AMBIENCE];
    let srcs = ["sh", "sg", "sa"];
    for (ti, &tid) in tracks.iter().enumerate() {
        p.items
            .push(item(&format!("{}_1", tid), srcs[ti], tid, 0, 200));
        p.items
            .push(item(&format!("{}_2", tid), srcs[ti], tid, 400, 200));
    }
    // 每轨两段都入同一组。
    let all_ids: Vec<String> = p.items.iter().map(|i| i.id.clone()).collect();
    let (p, gid) = link::create_group(p, "全场", all_ids, None).unwrap();
    let rates = rate_map(&srcs);

    // 第一次波纹删除：删掉每轨第一段 [0,200)，第二段应整体前移 200 -> 起点 200。
    let p = link::delete_item(p, &rates, "track_host_1", true, ZeroPolicy::Reject).unwrap();
    for tid in tracks {
        let it = p
            .items
            .iter()
            .find(|i| i.id == format!("{tid}_2"))
            .unwrap();
        assert_eq!(it.timeline_start, 200, "第一次波纹后 {tid} 应对齐到 200");
        assert_eq!(p.items.iter().filter(|i| i.track_id == tid).count(), 1);
    }

    // 在剩余片段 [200,400) 之前再插入一段并删除，验证第二次删除仍然三轨同步。
    let mut p = p;
    for (ti, &tid) in tracks.iter().enumerate() {
        p.items
            .push(item(&format!("{tid}_0"), srcs[ti], tid, 0, 100));
    }
    let new_ids: Vec<String> = tracks.iter().map(|t| format!("{t}_0")).collect();
    let p = link::add_items_to_group(p, &gid, new_ids).unwrap();
    let p = link::delete_item(p, &rates, "track_host_0", true, ZeroPolicy::Reject).unwrap();
    for tid in tracks {
        let it = p
            .items
            .iter()
            .find(|i| i.id == format!("{tid}_2"))
            .unwrap();
        // 再前移 100：200 - 100 = 100，三轨必须完全一致。
        assert_eq!(it.timeline_start, 100, "第二次波纹后 {tid} 应对齐到 100");
    }

    // 渲染验证：三轨脉冲（源内偏移 50）出现在同一输出帧 100+50=150，
    // 混音在该帧为三轨之和 3.0，证明相对采样位置正确。
    let metas = sources_for(&srcs, 1000);
    let loaded = loaded_map(&srcs, 1000, 50);
    let out = render::render(&p, &metas, &loaded, None).unwrap();
    assert_eq!(out[150], 3.0, "三轨脉冲必须落在同一输出采样 150");
    assert_eq!(out[149], 0.0);
    assert_eq!(out[151], 0.0);
}

// ---------- 2. 非波纹删除 / 单轨修剪保留其他轨道工程位置 ----------

#[test]
fn non_ripple_delete_and_single_trim_preserve_other_tracks() {
    let (p, _gid) = aligned_group_project(100);
    let rates = rate_map(&["sh", "sg", "sa"]);

    // 非波纹删除主持人片段：其余片段位置一个帧都不动。
    let p2 = link::delete_item(p, &rates, "h", false, ZeroPolicy::Reject).unwrap();
    assert!(p2.items.iter().all(|i| i.id != "h"));
    assert_eq!(p2.items.iter().find(|i| i.id == "g").unwrap().timeline_start, 100);
    assert_eq!(p2.items.iter().find(|i| i.id == "a").unwrap().timeline_start, 100);

    // 单轨修剪嘉宾片段：气氛轨位置保持，嘉宾 timeline_start 也保持。
    let p3 = link::trim_item(p2, "g", 0, 120).unwrap();
    let g = p3.items.iter().find(|i| i.id == "g").unwrap();
    assert_eq!(g.source_end, 120);
    assert_eq!(g.timeline_start, 100);
    assert_eq!(p3.items.iter().find(|i| i.id == "a").unwrap().timeline_start, 100);
}

// ---------- 3. 锁定轨：波纹与修剪被拒绝，且工程零变更 ----------

#[test]
fn locked_track_rejects_linked_edit_without_any_change() {
    let (mut p, gid) = aligned_group_project(0);
    p = link::set_track_locked(p, TRACK_GUEST, true).unwrap();
    let rates = rate_map(&["sh", "sg", "sa"]);
    let before = p.clone();

    let err = link::delete_item(p.clone(), &rates, "h", true, ZeroPolicy::Reject);
    assert!(err.is_err(), "嘉宾轨锁定时整组波纹必须被拒绝");
    assert!(err.unwrap_err().to_string().contains(TRACK_GUEST));
    // 拒绝后工程与操作前逐字段一致（零变更，可直接撤销语义）。
    assert_eq!(p.items, before.items);

    // 直接修剪锁定轨片段也被拒绝。
    assert!(link::trim_item(p.clone(), "g", 0, 10).is_err());
    // 整组移动同样被拒绝。
    assert!(link::move_group(p.clone(), &rates, &gid, 50, ZeroPolicy::Reject).is_err());
}

// ---------- 4. 隐藏轨（含隐藏的锁定轨）仍参与组规则 ----------

#[test]
fn hidden_track_still_participates_in_group_rules() {
    let rates = rate_map(&["sh", "sg", "sa"]);

    // 隐藏气氛轨：它的长片段跨越删除区间，波纹时必须被裁切并随之前移。
    let mut p = three_track_project([0, 0, 0]);
    p.items.push(item("h1", "sh", TRACK_HOST, 0, 100));
    p.items.push(item("h2", "sh", TRACK_HOST, 100, 200));
    p.items.push(item("g1", "sg", TRACK_GUEST, 0, 100));
    p.items.push(item("g2", "sg", TRACK_GUEST, 100, 200));
    p.items.push(item("a_long", "sa", TRACK_AMBIENCE, 0, 300));
    p = link::set_track_hidden(p, TRACK_AMBIENCE, true).unwrap();
    let (p, gid) = link::create_group(
        p,
        "组",
        vec![
            "h1".into(),
            "h2".into(),
            "g1".into(),
            "g2".into(),
            "a_long".into(),
        ],
        None,
    )
    .unwrap();

    let p = link::delete_item(p, &rates, "h1", true, ZeroPolicy::Reject).unwrap();
    // 隐藏轨的长片段被拆成左右两段，右段随波纹前移 100：起点 100。
    let amb_right = p
        .items
        .iter()
        .filter(|i| i.track_id == TRACK_AMBIENCE)
        .max_by_key(|i| i.timeline_start)
        .unwrap();
    assert_eq!(amb_right.timeline_start, 100, "隐藏轨必须随组波纹移动");
    // 状态接口明确标出该轨「隐藏但仍在参与联动」。
    let statuses = link::group_statuses(&p, &rates);
    let s = statuses.iter().find(|s| s.id == gid).unwrap();
    assert!(s.hidden_tracks.contains(&TRACK_AMBIENCE.to_string()));
    assert!(s.member_tracks.contains(&TRACK_AMBIENCE.to_string()));

    // 隐藏 + 锁定：即使看不见，锁定仍然一票否决。
    let (mut p2, _g) = aligned_group_project(0);
    p2 = link::set_track_hidden(p2, TRACK_AMBIENCE, true).unwrap();
    p2 = link::set_track_locked(p2, TRACK_AMBIENCE, true).unwrap();
    let err = link::delete_item(p2.clone(), &rates, "h", true, ZeroPolicy::Reject);
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains(TRACK_AMBIENCE));
}

// ---------- 5. 不同录音起点：独立偏移 + 负起点 reject / clip ----------

#[test]
fn negative_start_uses_reject_or_clip_explicitly() {
    // 主持人录音早于工程零点 50 帧：片段起点 -50。
    let mut p = three_track_project([-50, 0, 25]);
    p.items.push(item("h", "sh", TRACK_HOST, -50, 300));
    p.items.push(item("g", "sg", TRACK_GUEST, 0, 300));
    p.items.push(item("a", "sa", TRACK_AMBIENCE, 25, 300));
    let (p, gid) =
        link::create_group(p, "偏移组", vec!["h".into(), "g".into(), "a".into()], None).unwrap();
    let rates = rate_map(&["sh", "sg", "sa"]);

    // 状态接口必须报出负起点冲突原因。
    let st = link::group_statuses(&p, &rates);
    assert!(st[0]
        .conflicts
        .iter()
        .any(|c| c.item_id.as_deref() == Some("h") && c.reason.contains("零点")));

    // 再向前移 10 帧：reject 整单拒绝、零变更。
    let rejected = link::move_group(p.clone(), &rates, &gid, -10, ZeroPolicy::Reject);
    assert!(rejected.is_err());
    assert_eq!(p.items[0].timeline_start, -50);

    // clip：主持人片段引用范围被裁掉零点之前的 60 帧（原 -50 再前移 10）。
    let clipped = link::move_group(p, &rates, &gid, -10, ZeroPolicy::Clip).unwrap();
    let h = clipped.items.iter().find(|i| i.id == "h").unwrap();
    assert_eq!(h.timeline_start, 0);
    assert_eq!(h.source_start, 60);
    // 嘉宾原在 0，前移 10 后也越过零点，按 clip 同样钳到零点并裁掉 10 帧；
    // 气氛轨 25-10=15 仍为正，保留负起点表达之外的正常移动。
    let g = clipped.items.iter().find(|i| i.id == "g").unwrap();
    assert_eq!(g.timeline_start, 0);
    assert_eq!(g.source_start, 10);
    assert_eq!(
        clipped.items.iter().find(|i| i.id == "a").unwrap().timeline_start,
        15
    );
}

#[test]
fn ripple_into_negative_timeline_rejects_then_clips() {
    // 整组先移到 -50（clip），第一段被钳到零点但仍保留全部引用长度 100；
    // 删除第一段（区间 [0,100)）后，起点 50 的第二段前移 100 会越过零点。
    let mut p = three_track_project([0, 0, 0]);
    p.items.push(item("h1", "sh", TRACK_HOST, -50, 100));
    p.items.push(item("h2", "sh", TRACK_HOST, 50, 100));
    p.items.push(item("g1", "sg", TRACK_GUEST, -50, 100));
    p.items.push(item("g2", "sg", TRACK_GUEST, 50, 100));
    let (p, _gid) = link::create_group(
        p,
        "g",
        vec!["h1".into(), "h2".into(), "g1".into(), "g2".into()],
        None,
    )
    .unwrap();
    let rates = rate_map(&["sh", "sg"]);
    // 删除第一段（timeline [0,100)，delta=100）：第二段 50-100=-50 越过零点。
    let reject = link::delete_item(p.clone(), &rates, "h1", true, ZeroPolicy::Reject);
    assert!(reject.is_err(), "reject 策略必须阻止越过零点的波纹");
    let clip = link::delete_item(p, &rates, "h1", true, ZeroPolicy::Clip).unwrap();
    for id in ["h2", "g2"] {
        let it = clip.items.iter().find(|i| i.id == id).unwrap();
        assert_eq!(it.timeline_start, 0, "clip 后 {id} 钳到零点");
        assert_eq!(it.source_start, 50, "clip 后 {id} 引用范围裁掉 50 帧");
    }
}

// ---------- 6. 嵌套剪辑组：循环检查与递归联动 ----------

#[test]
fn nested_groups_detect_cycles_and_propagate_ripple() {
    let mut p = three_track_project([0, 0, 0]);
    p.items.push(item("h", "sh", TRACK_HOST, 0, 200));
    p.items.push(item("g", "sg", TRACK_GUEST, 0, 200));
    p.items.push(item("a", "sa", TRACK_AMBIENCE, 0, 200));
    let (p, parent) =
        link::create_group(p, "父组", vec!["h".into(), "g".into()], None).unwrap();
    let (p, child) =
        link::create_group(p, "子组", vec!["a".into()], Some(parent.clone())).unwrap();

    // 父组成员递归包含子组片段。
    let members = link::member_items(&p, &parent).unwrap();
    assert_eq!(members.len(), 3);

    // 循环：把父组挂到自己的子孙下必须报错。
    assert!(link::set_group_parent(p.clone(), &parent, Some(child.clone())).is_err());
    assert!(link::set_group_parent(p.clone(), &parent, Some(parent.clone())).is_err());

    // 波纹删除父组片段：子组的气氛轨片段一并被删除（递归联动）。
    let rates = rate_map(&["sh", "sg", "sa"]);
    let p2 = link::delete_item(p, &rates, "h", true, ZeroPolicy::Reject).unwrap();
    assert!(p2.items.is_empty());
}

// ---------- 7. 分组交叉淡化：按每轨真实余量，一轨不足不动整组 ----------

#[test]
fn group_crossfade_respects_per_track_margin() {
    // 主持人两条各 200 帧；气氛轨第二条只有 40 帧（头部余量不足）。
    let mut p = three_track_project([0, 0, 0]);
    p.items.push(item("h1", "sh", TRACK_HOST, 0, 200));
    p.items.push(item("h2", "sh", TRACK_HOST, 200, 200));
    p.items.push(item("a1", "sa", TRACK_AMBIENCE, 0, 200));
    p.items.push(item("a2", "sa", TRACK_AMBIENCE, 200, 40));
    let (p, gid) = link::create_group(
        p,
        "淡化组",
        vec!["h1".into(), "h2".into(), "a1".into(), "a2".into()],
        None,
    )
    .unwrap();
    let metas = sources_for(&["sh", "sa"], 1000);

    // 请求 80 帧：主持人可行，气氛轨最多 40 -> 整单拒绝且零变更。
    let err = link::set_group_crossfade(p.clone(), &gid, 80, &metas).unwrap_err();
    assert!(err.margins.iter().any(|m| !m.feasible && m.max_feasible == 40));
    assert!(p.items.iter().all(|i| i.crossfade_out == 0), "拒绝时不得有任何淡化落值");

    // 降到 40：两轨都可行，落值后渲染总时长不变（淡化只重叠，不改变引用长度）。
    let p2 = link::set_group_crossfade(p, &gid, 40, &metas).unwrap();
    assert_eq!(p2.items.iter().find(|i| i.id == "h1").unwrap().crossfade_out, 40);
    assert_eq!(p2.items.iter().find(|i| i.id == "a1").unwrap().crossfade_out, 40);
    let loaded = {
        let mut m = render::LoadedSources::new();
        m.insert("sh".to_string(), Arc::new(signals::phase_sine(RATE, 1000, 200.0)));
        m.insert("sa".to_string(), Arc::new(signals::phase_sine(RATE, 1000, 300.0)));
        m
    };
    let out = render::render(&p2, &metas, &loaded, None).unwrap();
    assert_eq!(out.len(), 400, "交叉淡化不得偷偷改变整组时长");
}

// ---------- 8. 批量移动原子保存：一次快照、一次撤销恢复全部轨道 ----------

#[test]
fn batch_move_is_one_snapshot_and_undo_restores_all_tracks() {
    let dir = std::env::temp_dir().join(format!("pe_link_db_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = db::Db::open(dir.join("editor.sqlite").to_str().unwrap()).unwrap();
    let (mut p, gid) = aligned_group_project(0);
    p.id = "pp".into();
    db.create_project(&p).unwrap();
    let rates = rate_map(&["sh", "sg", "sa"]);

    // 一次整组移动只产生一个新版本。
    p = link::move_group(p, &rates, &gid, 120, ZeroPolicy::Reject).unwrap();
    p.version += 1;
    db.save_version(&p).unwrap();
    assert_eq!(db.list_versions("pp").unwrap(), vec![1, 2]);

    // 撤销回版本 1：三条轨道的位置全部恢复。
    let restored = db.restore_version("pp", 1).unwrap();
    assert!(restored.items.iter().all(|i| i.timeline_start == 0));

    // 模拟保存中断：给快照表加触发器让写入失败，当前版本必须保持不变。
    db.inject_save_failure();
    let mut p3 = restored;
    p3 = link::move_group(p3, &rates, &gid, 300, ZeroPolicy::Reject).unwrap();
    p3.version = 3;
    assert!(db.save_version(&p3).is_err(), "注入的保存中断必须报错");
    let cur = db.load_project("pp").unwrap();
    assert_eq!(cur.version, 1, "保存中断后当前修订仍是 1，无半提交");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- 9. 旧单轨工程：无轨道快照可直接打开并归一化 ----------

#[test]
fn legacy_single_track_project_opens_without_migration() {
    let dir = std::env::temp_dir().join(format!("pe_legacy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("editor.sqlite");
    {
        let db = db::Db::open(db_path.to_str().unwrap()).unwrap();
        // 手工构造旧格式快照：没有 tracks/groups 字段，item 没有 track_id。
        let legacy_json = r#"{
            "items":[{"id":"old","source_id":"s","source_start":0,
                      "source_end":100,"timeline_start":0,"crossfade_out":0}],
            "silences":[]
        }"#;
        db.create_project_raw_legacy("legacy", 48000, 1, legacy_json).unwrap();
        let p = db.load_project("legacy").unwrap();
        assert_eq!(p.tracks.len(), 1);
        assert_eq!(p.tracks[0].id, domain::TRACK_LEGACY);
        assert_eq!(p.items[0].track_id, domain::TRACK_LEGACY);
        // 素材引用原样保留，没有任何转换。
        assert_eq!(p.items[0].source_end, 100);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- 10. 缺失源文件：渲染前明确报错而非静音吞掉 ----------

#[test]
fn missing_source_file_is_reported_before_render() {
    let (p, _gid) = aligned_group_project(0);
    let metas = sources_for(&["sh", "sg", "sa"], 1000);
    let mut loaded = loaded_map(&["sh", "sg", "sa"], 1000, 10);
    loaded.remove("sg"); // 嘉宾源文件缺失
    let err = render::render(&p, &metas, &loaded, None).unwrap_err();
    assert!(err.to_string().contains("sg"), "错误需指出缺失的源: {err}");
}

// ---------- 11. 混音与分轨引用同一工程修订，逐样本一致 ----------

#[test]
fn mix_and_stems_derive_from_same_revision() {
    let (mut p, _gid) = aligned_group_project(0);
    p.version = 7;
    let metas = sources_for(&["sh", "sg", "sa"], 1000);
    let loaded = loaded_map(&["sh", "sg", "sa"], 1000, 33);

    let mix = render::render(&p, &metas, &loaded, None).unwrap();
    let stems = render::render_stems(&p, &metas, &loaded, None).unwrap();
    assert_eq!(stems.frame_count, mix.len());

    // 分轨逐帧相加等于混音（同一修订、同一渲染路径）。
    let track_ids = [TRACK_HOST, TRACK_GUEST, TRACK_AMBIENCE];
    for f in 0..mix.len() {
        let sum: f32 = track_ids
            .iter()
            .map(|t| stems.tracks.get(*t).unwrap()[f])
            .sum();
        assert!((sum - mix[f]).abs() < 1e-6, "帧 {f} 分轨之和不等于混音");
    }
    // 脉冲帧 33：每轨 1.0，混音 3.0；各分轨在该帧 1.0、其余为 0。
    assert_eq!(mix[33], 3.0);
    for t in track_ids {
        let buf = stems.tracks.get(t).unwrap();
        assert_eq!(buf[33], 1.0);
        assert_eq!(buf[32], 0.0);
    }

    // 导出落盘：混音与分轨 WAV 都写出，导出记录带同一 version。
    let dir = std::env::temp_dir().join(format!("pe_stems_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("mix.wav").to_string_lossy().to_string();
    let outcome =
        export::export_stems(&p, &metas, &loaded, &dest, CancelToken::default().into(), None)
            .unwrap();
    assert!(!outcome.cancelled);
    assert_eq!(outcome.version, 7);
    assert_eq!(outcome.stems.len(), 3);
    for (_, path, frames) in &outcome.stems {
        assert!(std::path::Path::new(path).exists());
        assert_eq!(*frames, mix.len());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- 12. 界面状态：联动范围、锁定轨、冲突原因同时可见 ----------

#[test]
fn group_status_exposes_range_locks_and_conflicts() {
    let (mut p, gid) = aligned_group_project(50);
    p = link::set_track_locked(p, TRACK_HOST, true).unwrap();
    p = link::set_track_hidden(p, TRACK_AMBIENCE, true).unwrap();
    let rates = rate_map(&["sh", "sg", "sa"]);
    let st = link::group_statuses(&p, &rates);
    let s = st.iter().find(|s| s.id == gid).unwrap();
    assert_eq!(s.range_start, 50);
    assert_eq!(s.range_end, 350);
    assert!(s.locked_tracks.contains(&TRACK_HOST.to_string()));
    assert!(s.hidden_tracks.contains(&TRACK_AMBIENCE.to_string()));
    assert!(s.conflicts.iter().any(|c| c.reason.contains("锁定")));
    assert_eq!(s.member_tracks.len(), 3);
}

// ---------- 13. 组结构在删除后自动清理空组与失效成员 ----------

#[test]
fn empty_groups_are_reaped_after_member_removal() {
    let (mut p, gid) = aligned_group_project(0);
    let rates = rate_map(&["sh", "sg", "sa"]);
    // 非波纹逐条删光组成员。
    for id in ["h", "g", "a"] {
        p = link::delete_item(p, &rates, id, false, ZeroPolicy::Reject).unwrap();
    }
    assert!(p.items.is_empty());
    assert!(p.groups.is_empty(), "无成员且无子组的空组应被回收");
    let _ = gid;
}

// 让未直接使用的 EditGroup 导入保持有效（类型在 API 层复用）。
#[allow(dead_code)]
fn _type_anchor(_g: EditGroup) {}
