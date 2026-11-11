//! 编辑操作。每个操作接收旧 Project、返回带递增 version 的新 Project，
//! 不就地修改，便于快照式撤销。操作只改变引用范围与排列，永不触碰源音频文件。

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::domain::{frames_to_rate, Frame, Item, Project, SilenceItem};

pub fn new_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{nanos}_{}", randish())
}

// 避免再引依赖的简单进程内变化量。
fn randish() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(1);
    let mut x = C.fetch_add(1, Ordering::Relaxed) ^ std::process::id();
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// 截取片段：把 item 的引用范围收窄/恢复到 [new_start, new_end)（源帧）。
/// 只改引用，原录音始终完整保留，因此也支持向两侧恢复。
/// 注意：版本号由服务层统一递增，本函数不改 version。
pub fn trim_item(
    mut p: Project,
    rates: &HashMap<String, u32>,
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
    let it = p
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    it.source_start = new_start;
    it.source_end = new_end;
    it.crossfade_out = 0; // 范围变化后淡化余量需重新确认
    Ok(layout(p, rates))
}

/// 把片段在某源帧处切成两段引用，原录音不动。
pub fn split_item(
    p: Project,
    rates: &HashMap<String, u32>,
    item_id: &str,
    at_source_frame: Frame,
) -> Result<Project> {
    let mut p = p;
    let pos = p
        .items
        .iter()
        .position(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    let it = p.items[pos].clone();
    if at_source_frame <= it.source_start || at_source_frame >= it.source_end {
        bail!("切分点 {at_source_frame} 不在片段引用范围内");
    }
    let right = Item {
        id: new_id("item"),
        source_id: it.source_id.clone(),
        source_start: at_source_frame,
        source_end: it.source_end,
        timeline_start: it.timeline_start,
        crossfade_out: 0,
    };
    let left = Item {
        source_end: at_source_frame,
        crossfade_out: 0,
        ..it
    };
    p.items[pos] = left;
    p.items.insert(pos + 1, right);
    Ok(layout(p, rates))
}

/// 调整顺序：把 item 移动到目标索引（按当前 Vec 顺序）。
pub fn reorder(
    mut p: Project,
    rates: &HashMap<String, u32>,
    item_id: &str,
    target_index: usize,
) -> Result<Project> {
    let pos = p
        .items
        .iter()
        .position(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    if target_index >= p.items.len() {
        bail!("目标索引 {target_index} 越界");
    }
    let it = p.items.remove(pos);
    p.items.insert(target_index, it);
    Ok(layout(p, rates))
}

/// 设置交叉淡化长度（工程帧）。可行性在渲染前用真实余量校验。
pub fn set_crossfade(mut p: Project, item_id: &str, frames: Frame) -> Result<Project> {
    if frames < 0 {
        bail!("交叉淡化长度不能为负");
    }
    let it = p
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("片段 {item_id} 不存在"))?;
    it.crossfade_out = frames;
    Ok(p)
}

/// 在时间线末尾追加一段显式静音（保留静音），长度为工程帧。
pub fn append_silence(mut p: Project, length: Frame) -> Result<Project> {
    if length <= 0 {
        bail!("静音长度必须为正");
    }
    p.silences.push(SilenceItem {
        id: new_id("sil"),
        timeline_start: 0,
        length,
    });
    Ok(p)
}

/// 唯一的精确排版函数。按 items 的 Vec 顺序首尾相接；
/// 每条静音按顺序作为相邻片段之间的空隙；交叉淡化使下一片段前移重叠。
/// 所有长度用真实源速率精确换算为工程帧，不使用占位速率。
pub fn layout(mut p: Project, rates: &HashMap<String, u32>) -> Project {
    let mut cursor: Frame = 0;
    let mut silence_idx = 0;

    for i in 0..p.items.len() {
        // 第 i 条片段（i>=1）之前消耗一段静音作为空隙。
        if i > 0 && silence_idx < p.silences.len() {
            p.silences[silence_idx].timeline_start = cursor;
            cursor += p.silences[silence_idx].length;
            silence_idx += 1;
        }
        let xf_in = if i > 0 { p.items[i - 1].crossfade_out } else { 0 };
        p.items[i].timeline_start = cursor - xf_in;
        let rate = rates
            .get(&p.items[i].source_id)
            .copied()
            .unwrap_or(p.sample_rate);
        let len = frames_to_rate(p.items[i].source_len(), rate, p.sample_rate);
        cursor += len;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates() -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert("src".to_string(), 48000);
        m
    }

    fn project_with(items: Vec<Item>) -> Project {
        Project {
            id: "p".into(),
            name: "t".into(),
            sample_rate: 48000,
            channels: 1,
            version: 1,
            items,
            silences: vec![],
        }
    }

    fn item(id: &str, s: Frame, e: Frame) -> Item {
        Item {
            id: id.into(),
            source_id: "src".into(),
            source_start: s,
            source_end: e,
            timeline_start: 0,
            crossfade_out: 0,
        }
    }

    #[test]
    fn trim_only_changes_reference_range() {
        let p = project_with(vec![item("a", 0, 1000)]);
        let p2 = trim_item(p, &rates(), "a", 100, 400).unwrap();
        assert_eq!(p2.items[0].source_start, 100);
        assert_eq!(p2.items[0].source_end, 400);
        // 纯编辑函数不改版本号；版本由服务层统一递增。
        assert_eq!(p2.version, 1);
    }

    #[test]
    fn split_keeps_total_reference_contiguous() {
        let p = project_with(vec![item("a", 0, 1000)]);
        let p2 = split_item(p, &rates(), "a", 400).unwrap();
        assert_eq!(p2.items.len(), 2);
        assert_eq!(p2.items[0].source_end, 400);
        assert_eq!(p2.items[1].source_start, 400);
        assert_eq!(p2.items[1].source_end, 1000);
    }

    #[test]
    fn layout_places_items_back_to_back_and_silence_between() {
        let mut p = project_with(vec![item("a", 0, 100), item("b", 0, 200)]);
        p = append_silence(p, 50).unwrap();
        let p = layout(p, &rates());
        assert_eq!(p.items[0].timeline_start, 0);
        assert_eq!(p.silences[0].timeline_start, 100);
        // 第二条在静音之后：100 + 50 = 150。
        assert_eq!(p.items[1].timeline_start, 150);
    }

    #[test]
    fn crossfade_overlaps_next_item_by_requested_frames() {
        let mut p = project_with(vec![item("a", 0, 100), item("b", 0, 200)]);
        p.items[0].crossfade_out = 20;
        let p = layout(p, &rates());
        assert_eq!(p.items[1].timeline_start, 100 - 20);
    }

    #[test]
    fn reorder_changes_sequence() {
        let p = project_with(vec![item("a", 0, 100), item("b", 0, 200)]);
        let p = reorder(p, &rates(), "a", 1).unwrap();
        assert_eq!(p.items[0].id, "b");
        assert_eq!(p.items[1].id, "a");
    }
}
