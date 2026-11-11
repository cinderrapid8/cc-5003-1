//! 多轨联动编辑（主持人 / 嘉宾 / 现场气氛三轨）。
//!
//! 核心约定：
//! * 剪辑组（EditGroup）把多轨上时间对齐的片段绑定为一个联动单元，
//!   波纹删除、整组移动、分组交叉淡化都以组为单位作用于全部成员，
//!   成员包含子组的递归展开；嵌套关系禁止成环。
//! * 轨道锁定是硬约束：任何会改动该轨片段位置/范围的操作都被拒绝，
//!   并返回结构化冲突（冲突原因精确到轨/片段/组）。
//! * 轨道隐藏只是界面状态：隐藏轨的片段仍是组成员，参与全部联动规则，
//!     不会因为未显示而失去同步。
//! * 录音起点差异由 Track.start_offset 表达；片段位置允许为负（录音早于
//!   工程零点）。任何可能产生/加深负起点的组操作都必须显式给出 ZeroPolicy：
//!   Reject 整单拒绝且不产生任何变更；Clip 明确裁切引用范围到零点。
//! * 所有函数都是纯函数：接收 Project、返回新 Project，版本号由服务层递增，
//!   一次联动编辑只落一个版本快照，天然支持「一次撤销恢复所有受影响轨道」。

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::domain::{
    check_crossfade, frames_to_rate, CrossfadeMargin, EditGroup, Frame, Item, Project, Track,
};

/// 片段越过工程零点（负起点）时的处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZeroPolicy {
    /// 整单拒绝：不移动、不裁切、不删除任何片段，返回冲突原因。
    Reject,
    /// 明确裁切：把越界部分从引用范围中裁掉，起点钳到零点；整段都在零点前则删除。
    Clip,
}

/// 结构化联动冲突。界面同时展示联动范围、锁定轨与冲突原因。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LinkConflict {
    pub group_id: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

/// 组的联动状态快照（供界面展示联动范围、锁定轨、隐藏轨、冲突）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupStatus {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    /// 联动范围（工程帧，含负起点）。
    pub range_start: Frame,
    pub range_end: Frame,
    pub member_item_ids: Vec<String>,
    pub member_tracks: Vec<String>,
    pub locked_tracks: Vec<String>,
    /// 隐藏但仍在参与联动的轨道（证明隐藏不失同步）。
    pub hidden_tracks: Vec<String>,
    pub conflicts: Vec<LinkConflict>,
}

// ---------- 组结构 ----------

fn find_group<'a>(p: &'a Project, gid: &str) -> Result<&'a EditGroup> {
    p.groups
        .iter()
        .find(|g| g.id == gid)
        .ok_or_else(|| anyhow::anyhow!("剪辑组 {gid} 不存在"))
}

/// 递归收集组及其子组的全部成员片段（去重，保持稳定顺序）。
pub fn member_items<'a>(p: &'a Project, gid: &str) -> Result<Vec<&'a Item>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut stack = vec![gid.to_string()];
    let mut pushed = HashSet::new();
    while let Some(cur) = stack.pop() {
        let g = find_group(p, &cur)?;
        for id in &g.item_ids {
            if seen.insert(id.clone()) {
                if let Some(it) = p.items.iter().find(|i| &i.id == id) {
                    out.push(it);
                }
            }
        }
        for child in p.groups.iter().filter(|g| g.parent_id.as_deref() == Some(cur.as_str())) {
            if pushed.insert(child.id.clone()) {
                stack.push(child.id.clone());
            }
        }
    }
    Ok(out)
}

/// 递归收集组自身与全部后代组 ID。
#[allow(dead_code)]
pub fn transitive_group_ids(p: &Project, gid: &str) -> Result<Vec<String>> {
    find_group(p, gid)?;
    let mut out = vec![gid.to_string()];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i].clone();
        for g in p.groups.iter().filter(|g| g.parent_id.as_deref() == Some(cur.as_str())) {
            if !out.contains(&g.id) {
                out.push(g.id.clone());
            }
        }
        i += 1;
    }
    Ok(out)
}

/// 嵌套循环检查：若把 `gid` 的父组设为 `new_parent`，
/// 沿 new_parent 的祖先链走回 gid 即成环，拒绝；父组不能指向自己。
pub fn ensure_no_cycle(p: &Project, gid: &str, new_parent: Option<&str>) -> Result<()> {
    if let Some(pid) = new_parent {
        if pid == gid {
            bail!("剪辑组不能嵌套到自身");
        }
        find_group(p, pid)?;
        let mut cur = Some(pid.to_string());
        let mut steps = 0;
        while let Some(id) = cur {
            if id == gid {
                bail!("嵌套剪辑组成环：{gid} 已是 {pid} 的祖先");
            }
            let g = find_group(p, &id)?;
            cur = g.parent_id.clone();
            steps += 1;
            if steps > p.groups.len() + 1 {
                bail!("嵌套剪辑组关系异常（疑似成环）");
            }
        }
    }
    Ok(())
}

/// 新建剪辑组。item_ids 必须全部存在；parent_id 给定时做循环检查。
pub fn create_group(
    mut p: Project,
    name: &str,
    item_ids: Vec<String>,
    parent_id: Option<String>,
) -> Result<(Project, String)> {
    for id in &item_ids {
        if !p.items.iter().any(|i| &i.id == id) {
            bail!("片段 {id} 不存在，无法入组");
        }
    }
    let gid = crate::edit::new_id("group");
    ensure_no_cycle(&p, &gid, parent_id.as_deref())?;
    p.groups.push(EditGroup {
        id: gid.clone(),
        name: name.to_string(),
        parent_id,
        item_ids,
    });
    Ok((p, gid))
}

/// 把片段加入已存在的组。
pub fn add_items_to_group(mut p: Project, gid: &str, item_ids: Vec<String>) -> Result<Project> {
    for id in &item_ids {
        if !p.items.iter().any(|i| &i.id == id) {
            bail!("片段 {id} 不存在，无法入组");
        }
    }
    let g = p
        .groups
        .iter_mut()
        .find(|g| g.id == gid)
        .ok_or_else(|| anyhow::anyhow!("剪辑组 {gid} 不存在"))?;
    for id in item_ids {
        if !g.item_ids.contains(&id) {
            g.item_ids.push(id);
        }
    }
    Ok(p)
}

/// 调整嵌套父组（含循环检查）。
pub fn set_group_parent(mut p: Project, gid: &str, parent_id: Option<String>) -> Result<Project> {
    ensure_no_cycle(&p, gid, parent_id.as_deref())?;
    let g = p
        .groups
        .iter_mut()
        .find(|g| g.id == gid)
        .ok_or_else(|| anyhow::anyhow!("剪辑组 {gid} 不存在"))?;
    g.parent_id = parent_id;
    Ok(p)
}

// ---------- 轨道 ----------

pub fn set_track_locked(mut p: Project, track_id: &str, locked: bool) -> Result<Project> {
    let t = track_mut(&mut p, track_id)?;
    t.locked = locked;
    Ok(p)
}

pub fn set_track_hidden(mut p: Project, track_id: &str, hidden: bool) -> Result<Project> {
    let t = track_mut(&mut p, track_id)?;
    t.hidden = hidden;
    Ok(p)
} /// 设置录音起点独立偏移（工程帧，可为负）。只记录起点语义，不移动既有片段。
pub fn set_track_offset(mut p: Project, track_id: &str, offset: Frame) -> Result<Project> {
    let t = track_mut(&mut p, track_id)?;
    t.start_offset = offset;
    Ok(p)
}

fn track_mut<'a>(p: &'a mut Project, track_id: &str) -> Result<&'a mut Track> {
    p.tracks
        .iter_mut()
        .find(|t| t.id == track_id)
        .ok_or_else(|| anyhow::anyhow!("轨道 {track_id} 不存在"))
}

/// 收集成员片段触及的锁定轨（隐藏轨不豁免）。
fn locked_member_tracks(p: &Project, members: &[&Item]) -> Vec<String> {
    let mut out: Vec<String> = members
        .iter()
        .map(|i| i.track_id.clone())
        .filter(|tid| p.is_locked(tid))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn lock_conflicts(group_id: &str, tracks: &[String]) -> Vec<LinkConflict> {
    tracks
        .iter()
        .map(|tid| LinkConflict {
            group_id: group_id.to_string(),
            reason: format!("轨道 {tid} 已锁定，联动编辑被拒绝"),
            track_id: Some(tid.clone()),
            item_id: None,
        })
        .collect()
}

// ---------- 单轨修剪（保留其他轨道工程位置） ----------

/// 单轨片段修剪：只改该片段的引用范围，不调用全局排版，
/// 因此同轨其他片段与其他所有轨道的 timeline_start 原样保留。
pub fn trim_item(
    mut p: Project,
    item_id: &str,
    new_start: Frame,
    new_end: Frame,
) -> Result<Project> {
    if new_end <= new_start {
        bail!("截取范围无效：end({new_end}) 必须大于 start({new_start})");
    }
    if new_start < 0 {
        bail!("源帧位置不能为负");
    }
    let pos = p
        .items
        .iter()
        .position(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    if p.is_locked(&p.items[pos].track_id) {
        bail!("轨道 {} 已锁定，不能修剪片段 {item_id}", p.items[pos].track_id);
    }
    let it = &mut p.items[pos];
    it.source_start = new_start;
    it.source_end = new_end;
    it.crossfade_out = 0; // 范围变化后淡化余量需重新确认
    Ok(p)
}

// ---------- 就地切分（不动其他轨道位置） ----------

/// 在某源帧处把片段切成两个引用。右片段继承同一轨道与时间线位置语义，
/// 不做全局排版，其他片段（含其他轨道）位置不变；右片段继承原组成员身份。
pub fn split_item(
    mut p: Project,
    rates: &HashMap<String, u32>,
    item_id: &str,
    at_source_frame: Frame,
) -> Result<Project> {
    let pos = p
        .items
        .iter()
        .position(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    if p.is_locked(&p.items[pos].track_id) {
        bail!("轨道 {} 已锁定，不能切分片段 {item_id}", p.items[pos].track_id);
    }
    let it = p.items[pos].clone();
    if at_source_frame <= it.source_start || at_source_frame >= it.source_end {
        bail!("切分点 {at_source_frame} 不在片段引用范围内");
    }
    let src_rate = rates.get(&it.source_id).copied().unwrap_or(p.sample_rate);
    let local_src = at_source_frame - it.source_start;
    let split_proj = frames_to_rate(local_src, src_rate, p.sample_rate);
    let right = Item {
        id: crate::edit::new_id("item"),
        source_id: it.source_id.clone(),
        track_id: it.track_id.clone(),
        source_start: at_source_frame,
        source_end: it.source_end,
        timeline_start: it.timeline_start + split_proj,
        crossfade_out: 0,
    };
    let left = Item {
        source_end: at_source_frame,
        crossfade_out: 0,
        ..it
    };
    p.items[pos] = left.clone();
    p.items.insert(pos + 1, right.clone());
    // 右片段继承直接父组身份，保证切分后联动不丢。
    for g in p.groups.iter_mut() {
        if g.item_ids.iter().any(|id| id == item_id) {
            g.item_ids.push(right.id.clone());
        }
    }
    Ok(p)
}

// ---------- 删除与波纹 ----------/// 删除片段。
/// * `ripple=false`：仅删除该片段，任何其他片段的工程位置都不变。
/// * `ripple=true`：
///     - 片段属于某个组 → 整组波纹：同组（含子组）落在删除区间内的片段
///       被删除/裁切，其余组成员统一前移；锁定轨（含隐藏的锁定轨）一票否决；
///     - 片段不属于组 → 仅在本轨波纹，组外片段一律不动。
pub fn delete_item(
    mut p: Project,
    rates: &HashMap<String, u32>,
    item_id: &str,
    ripple: bool,
    policy: ZeroPolicy,
) -> Result<Project> {
    let target = p
        .items
        .iter()
        .find(|i| i.id == item_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    if p.is_locked(&target.track_id) {
        bail!("轨道 {} 已锁定，不能删除片段 {item_id}", target.track_id);
    }

    // 找到直接包含该片段的组（一个片段至多直接属于一个组）。
    let owner_group = p
        .groups
        .iter()
        .find(|g| g.item_ids.iter().any(|id| id == item_id))
        .cloned();

    if !ripple {
        p.items.retain(|i| i.id != item_id);
        reset_fades_after_removal(&mut p);
        prune_group_memberships(&mut p);
        return Ok(p);
    }

    let seg_start = target.timeline_start;
    let seg_end = item_end(&target, rates, p.sample_rate);
    let delta = seg_end - seg_start;
    if delta <= 0 {
        bail!("删除区间长度必须为正");
    }

    // 受波纹影响的成员集合与组 ID。
    let (member_ids, gid): (HashSet<String>, String) = if let Some(g) = &owner_group {
        let members = member_items(&p, &g.id)?;
        let locked = locked_member_tracks(&p, &members);
        if !locked.is_empty() {
            bail!(
                "整组波纹被锁定轨拒绝：{}",
                lock_conflicts(&g.id, &locked)
                    .into_iter()
                    .map(|c| c.reason)
                    .collect::<Vec<_>>()
                    .join("；")
            );
        }
        (members.iter().map(|i| i.id.clone()).collect(), g.id.clone())
    } else {
        (HashSet::new(), String::new())
    };

    // 先做零点预检（Reject 时必须零变更）。
    // 受管片段（组成员，或无组时同轨片段）只要与删除区间有「右侧接触」——
    // 完全位于区间之后，或跨越右边界（会被裁切/拆分后跳到 seg_end）——
    // 就必然前移 delta。资格只看归属与引用关系，不看当前位置，
    // 因为片段可能已被之前的 clip 操作钳到零点。
    if policy == ZeroPolicy::Reject {
        for it in p.items.iter() {
            let in_scope =
                member_ids.contains(&it.id) || (gid.is_empty() && it.track_id == target.track_id);
            if !in_scope {
                continue;
            }
            let end = item_end(it, rates, p.sample_rate);
            let after_segment = it.timeline_start >= seg_end;
            let crosses_right = it.timeline_start < seg_end && end > seg_end;
            if after_segment || crosses_right {
                // 右相交/拆分先落到 seg_end；纯后置片段从当前位置前移。
                let new_start = if crosses_right {
                    seg_end - delta
                } else {
                    it.timeline_start - delta
                };
                if new_start < 0 {
                    bail!(
                        "波纹后片段 {} 起点变为 {}（越过工程零点），按 reject 策略整单拒绝",
                        it.id,
                        new_start
                    );
                }
            }
        }
    }

    // 第一步：处理与删除区间相交的片段（删除 / 裁切 / 一分为二）。
    // shift_ids 显式记录所有需要波纹前移的片段（含裁切/拆分产生的新右片段），
    // 避免依赖「位置恰好等于 seg_end」这种脆弱判断。
    // replacements 记录「被裁切/拆分的原片段 -> 派生片段」，派生片段继承原片段
    // 的组成员身份，保证隐藏轨等成员在裁切后不会脱离联动。
    let mut additions: Vec<Item> = Vec::new();
    let mut shift_ids: HashSet<String> = HashSet::new();
    let mut replacements: HashMap<String, Vec<String>> = HashMap::new();
    let mut survivors: Vec<Item> = Vec::new();
    for it in p.items.drain(..) {
        let in_scope =
            member_ids.contains(&it.id) || (gid.is_empty() && it.track_id == target.track_id);
        let end = item_end(&it, rates, p.sample_rate);
        let intersects = in_scope && it.timeline_start < seg_end && end > seg_start;
        if !intersects {
            // 受管且不在删除区间内的片段一律参与波纹前移（无论当前位置在哪，
            // 包括已被 clip 钳到零点的片段）；相交资格由上面的区间判断决定。
            if in_scope && end <= seg_start {
                // 完全位于区间之前：不前移。
            } else if in_scope {
                shift_ids.insert(it.id.clone());
            }
            survivors.push(it);
            continue;
        }
        if it.timeline_start >= seg_start && end <= seg_end {
            continue; // 整段落在删除区间内
        }
        // 部分相交：按工程帧差值换算回本素材源帧后裁切引用。
        let src_rate = rates.get(&it.source_id).copied().unwrap_or(p.sample_rate);
        let to_src = |proj: Frame| frames_to_rate(proj, p.sample_rate, src_rate);
        let parent_id = it.id.clone();
        let mut derived: Vec<String> = Vec::new();
        // 先判「区间完全落在片段内部」：起点恰好等于 seg_start 时 left_len=0，
        // 必须优先于右相交分支，否则会误删区间左侧（本就为空）之外还走错分类。
        if it.timeline_start <= seg_start && end > seg_end {
            // 区间完全落在片段内部（含起点对齐 seg_start 的退化情形）：
            // 拆成左右两个引用，原录音不动；左侧为空则只保留右侧。
            // 右片段保留在原边界帧 seg_end、不前移——它引用的素材本就不在被删区间内，
            // 只是跳过了区间内的源帧；这与「区间之后的独立片段整体前移」相区别。
            let left_len = seg_start - it.timeline_start;
            let right_skip = to_src(seg_end - seg_start);
            let mut right = it.clone();
            right.source_start = right.source_start + to_src(left_len) + right_skip;
            right.timeline_start = seg_end;
            right.id = crate::edit::new_id("item");
            right.crossfade_out = 0;
            derived.push(right.id.clone());
            if left_len > 0 {
                let mut left = it;
                left.source_end = left.source_start + to_src(left_len);
                left.crossfade_out = 0;
                survivors.push(left);
            }
            additions.push(right);
        } else if it.timeline_start < seg_start && end <= seg_end {
            // 左相交：只保留区间左侧部分，位置不变、不前移。
            let cut = (seg_start - it.timeline_start).min(end - it.timeline_start);
            let mut kept = it;
            kept.source_end = kept.source_start + to_src(cut);
            survivors.push(kept);
        } else {
            // 右相交：裁掉区间内的头部，剩余部分随后整体前移。
            let cut = (seg_end - it.timeline_start).min(end - it.timeline_start);
            let mut kept = it;
            kept.source_start = kept.source_start + to_src(cut);
            kept.timeline_start = seg_end;
            kept.crossfade_out = 0;
            shift_ids.insert(kept.id.clone());
            derived.push(kept.id.clone());
            survivors.push(kept);
        }
        if !derived.is_empty() {
            replacements.insert(parent_id, derived);
        }
    }
    survivors.extend(additions);
    p.items = survivors;

    // 派生片段继承原片段的（直接）组成员身份，裁切/拆分不脱离联动。
    if !gid.is_empty() {
        if let Some(g) = p.groups.iter_mut().find(|g| g.id == gid) {
            for (parent, kids) in &replacements {
                if g.item_ids.iter().any(|id| id == parent) {
                    for kid in kids {
                        if !g.item_ids.contains(kid) {
                            g.item_ids.push(kid.clone());
                        }
                    }
                }
            }
        }
    }

    // 第二步：显式登记的片段统一前移 delta（其他片段一律不动）。
    for it in p.items.iter_mut() {
        if shift_ids.contains(&it.id) {
            it.timeline_start -= delta;
            it.crossfade_out = 0;
            apply_zero_policy(it, policy, rates, p.sample_rate)?;
        }
    }

    // 清理被删片段的组登记与失效淡化。
    reset_fades_after_removal(&mut p);
    prune_group_memberships(&mut p);
    Ok(p)
}

/// 删除后：从组成员清单中移除不存在的片段；清空空组（含无成员且无子组的组）。
fn prune_group_memberships(p: &mut Project) {
    for g in p.groups.iter_mut() {
        g.item_ids.retain(|id| p.items.iter().any(|i| &i.id == id));
    }
    let keep: HashSet<String> = p
        .groups
        .iter()
        .filter(|g| {
            let has_items = !g.item_ids.is_empty();
            let has_children = p
                .groups
                .iter()
                .any(|other| other.parent_id.as_deref() == Some(g.id.as_str()));
            has_items || has_children
        })
        .map(|g| g.id.clone())
        .collect();
    p.groups.retain(|g| keep.contains(&g.id));
}

/// 删除/裁切后重建同轨相邻关系：淡化只允许存在于同轨真正相邻的两条片段之间，
/// 每条轨道最后一条片段的淡化必须清零。
fn reset_fades_after_removal(p: &mut Project) {
    let mut by_track: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, it) in p.items.iter().enumerate() {
        by_track.entry(it.track_id.clone()).or_default().push(idx);
    }
    for idxs in by_track.values() {
        let mut order = idxs.clone();
        order.sort_by_key(|&i| p.items[i].timeline_start);
        for &i in &order {
            p.items[i].crossfade_out = 0;
        }
    }
}

/// 零点策略落地：负起点片段按策略拒绝或裁切引用范围。
fn apply_zero_policy(
    it: &mut Item,
    policy: ZeroPolicy,
    rates: &HashMap<String, u32>,
    project_rate: u32,
) -> Result<()> {
    if it.timeline_start >= 0 {
        return Ok(());
    }
    match policy {
        ZeroPolicy::Reject => bail!(
            "片段 {} 波纹后起点为 {}，越过工程零点，按 reject 策略拒绝",
            it.id,
            it.timeline_start
        ),
        ZeroPolicy::Clip => {
            let src_rate = rates.get(&it.source_id).copied().unwrap_or(project_rate);
            let skip_src = frames_to_rate(-it.timeline_start, project_rate, src_rate);
            it.source_start += skip_src;
            it.timeline_start = 0;
            if it.source_end <= it.source_start {
                // 整段都在零点之前：标记为空，由调用方移除。
                it.source_end = it.source_start;
            }
        }
    }
    Ok(())
}

// ---------- 整组移动 ----------

/// 整组批量移动 delta 个工程帧（delta 可负）。一次调用、一次快照保存。
/// 隐藏轨成员同样移动；锁定轨一票否决；负起点按 policy 处理。
pub fn move_group(
    mut p: Project,
    rates: &HashMap<String, u32>,
    gid: &str,
    delta: Frame,
    policy: ZeroPolicy,
) -> Result<Project> {
    let members = member_items(&p, gid)?;
    let locked = locked_member_tracks(&p, &members);
    if !locked.is_empty() {
        bail!(
            "整组移动被锁定轨拒绝：{}",
            lock_conflicts(gid, &locked)
                .into_iter()
                .map(|c| c.reason)
                .collect::<Vec<_>>()
                .join("；")
        );
    }
    let ids: HashSet<String> = members.iter().map(|i| i.id.clone()).collect();
    if policy == ZeroPolicy::Reject {
        for it in p.items.iter().filter(|i| ids.contains(&i.id)) {
            if it.timeline_start + delta < 0 {
                bail!(
                    "移动后片段 {} 起点为 {}（越过工程零点），按 reject 策略整单拒绝",
                    it.id,
                    it.timeline_start + delta
                );
            }
        }
    }
    for it in p.items.iter_mut().filter(|i| ids.contains(&i.id)) {
        it.timeline_start += delta;
        apply_zero_policy(it, policy, rates, p.sample_rate)?;
    }
    p.items.retain(|i| i.source_end > i.source_start);
    prune_group_memberships(&mut p);
    Ok(p)
}

// ---------- 分组交叉淡化 ----------

/// 分组交叉淡化的可行性错误：携带每条轨道各自的真实余量。
/// 任一轨余量不足即整单拒绝，绝不通过缩短淡化去偷偷改变整组时长。
#[derive(Debug)]
pub struct GroupCrossfadeError {
    pub margins: Vec<CrossfadeMargin>,
}

impl std::fmt::Display for GroupCrossfadeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "分组交叉淡化余量不足：")?;
        for m in &self.margins {
            if !m.feasible {
                write!(
                    f,
                    "片段 {} 请求 {} 帧但最多支持 {} 帧；",
                    m.item_id, m.requested, m.max_feasible
                )?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for GroupCrossfadeError {}

/// 评估组内每条轨道相邻成员片段对的真实淡化余量（按各轨实际素材计算）。
pub fn evaluate_group_crossfades(
    p: &Project,
    gid: &str,
    frames: Frame,
    sources: &HashMap<String, crate::domain::Source>,
) -> Result<Vec<CrossfadeMargin>> {
    if frames < 0 {
        bail!("交叉淡化长度不能为负");
    }
    let members = member_items(p, gid)?;
    let tracks: Vec<String> = {
        let mut t: Vec<String> = members.iter().map(|i| i.track_id.clone()).collect();
        t.sort_unstable();
        t.dedup();
        t
    };
    let mut margins = Vec::new();
    for tid in tracks {
        let mut seq: Vec<&Item> = members.iter().copied().filter(|i| i.track_id == tid).collect();
        seq.sort_by_key(|i| i.timeline_start);
        for pair in seq.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            let mut candidate = left.clone();
            candidate.crossfade_out = frames;
            margins.push(check_crossfade(
                &candidate,
                right,
                sources,
                p.sample_rate,
            ));
        }
    }
    Ok(margins)
}

/// 设置分组交叉淡化：先按每条轨道真实余量统一评估，全部可行才落值，
/// 否则返回 GroupCrossfadeError 且工程不发生任何变更。
pub fn set_group_crossfade(
    mut p: Project,
    gid: &str,
    frames: Frame,
    sources: &HashMap<String, crate::domain::Source>,
) -> std::result::Result<Project, GroupCrossfadeError> {
    let member_owned: Vec<Item> = member_items(&p, gid)
        .map_err(|_| GroupCrossfadeError {
            margins: vec![CrossfadeMargin {
                item_id: gid.to_string(),
                requested: frames,
                tail_available: 0,
                head_available: 0,
                feasible: false,
                max_feasible: 0,
            }],
        })?
        .into_iter()
        .cloned()
        .collect();
    let locked = {
        let mut lk: Vec<String> = member_owned
            .iter()
            .map(|i| i.track_id.clone())
            .filter(|tid| p.is_locked(tid))
            .collect();
        lk.sort_unstable();
        lk.dedup();
        lk
    };
    if !locked.is_empty() {
        return Err(GroupCrossfadeError {
            margins: vec![CrossfadeMargin {
                item_id: locked.join(","),
                requested: frames,
                tail_available: 0,
                head_available: 0,
                feasible: false,
                max_feasible: 0,
            }],
        });
    }
    let margins = evaluate_group_crossfades(&p, gid, frames, sources).map_err(|_| {
        GroupCrossfadeError {
            margins: vec![],
        }
    })?;
    if margins.iter().any(|m| !m.feasible) {
        return Err(GroupCrossfadeError { margins });
    }
    // 全部可行：同轨相邻成员对的左片段落值，其余成员淡化清零。
    let ids: HashSet<String> = member_owned.iter().map(|i| i.id.clone()).collect();
    for it in p.items.iter_mut() {
        if ids.contains(&it.id) {
            it.crossfade_out = 0;
        }
    }
    let tracks: Vec<String> = {
        let mut t: Vec<String> = member_owned.iter().map(|i| i.track_id.clone()).collect();
        t.sort_unstable();
        t.dedup();
        t
    };
    for tid in tracks {
        let mut seq: Vec<&Item> = member_owned
            .iter()
            .filter(|i| i.track_id == tid)
            .collect();
        seq.sort_by_key(|i| i.timeline_start);
        for pair in seq.windows(2) {
            if let Some(target) = p.items.iter_mut().find(|i| i.id == pair[0].id) {
                target.crossfade_out = frames;
            }
        }
    }
    Ok(p)
}

// ---------- 状态与冲突展示 ----------

/// 计算每个组的联动范围、锁定轨、隐藏轨与冲突原因（纯工程内规则）。
pub fn group_statuses(p: &Project, rates: &HashMap<String, u32>) -> Vec<GroupStatus> {
    let mut out = Vec::new();
    for g in &p.groups {
        let members = member_items(p, &g.id).unwrap_or_default();
        if members.is_empty() {
            continue;
        }
        let mut range_start = Frame::MAX;
        let mut range_end = Frame::MIN;
        let mut tracks: Vec<String> = Vec::new();
        let mut locked_tracks: Vec<String> = Vec::new();
        let mut hidden_tracks: Vec<String> = Vec::new();
        let mut conflicts = Vec::new();
        for it in &members {
            let end = item_end(it, rates, p.sample_rate);
            range_start = range_start.min(it.timeline_start);
            range_end = range_end.max(end);
            if !tracks.contains(&it.track_id) {
                tracks.push(it.track_id.clone());
            }
            if let Some(t) = p.track(&it.track_id) {
                if t.locked && !locked_tracks.contains(&t.id) {
                    locked_tracks.push(t.id.clone());
                }
                if t.hidden && !hidden_tracks.contains(&t.id) {
                    hidden_tracks.push(t.id.clone());
                }
            }
            if it.timeline_start < 0 {
                conflicts.push(LinkConflict {
                    group_id: g.id.clone(),
                    reason: format!(
                        "片段 {} 越过工程零点（负起点 {} 帧），需显式裁切或拒绝",
                        it.id, it.timeline_start
                    ),
                    track_id: Some(it.track_id.clone()),
                    item_id: Some(it.id.clone()),
                });
            }
        }
        for tid in &locked_tracks {
            conflicts.push(LinkConflict {
                group_id: g.id.clone(),
                reason: format!("轨道 {tid} 已锁定，该组联动编辑将被拒绝"),
                track_id: Some(tid.clone()),
                item_id: None,
            });
        }
        out.push(GroupStatus {
            id: g.id.clone(),
            name: g.name.clone(),
            parent_id: g.parent_id.clone(),
            range_start,
            range_end,
            member_item_ids: members.iter().map(|i| i.id.clone()).collect(),
            member_tracks: tracks,
            locked_tracks,
            hidden_tracks,
            conflicts,
        });
    }
    out
}

/// 片段在工程时间线上的结束帧（不含），按其素材真实速率精确换算。
pub fn item_end(it: &Item, rates: &HashMap<String, u32>, project_rate: u32) -> Frame {
    let src_rate = rates.get(&it.source_id).copied().unwrap_or(project_rate);
    it.timeline_start + frames_to_rate(it.source_len(), src_rate, project_rate)
}
