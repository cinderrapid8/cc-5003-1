//! 领域模型与纯逻辑。
//!
//! 时间基准约定（关键不变量）：
//! * 工程有固定的采样率 `project_rate`，时间线上所有位置、长度、交叉淡化长度
//!   都以「工程采样帧」的整数 i64 表达；系统中不存在毫秒字段。
//! * 素材片段只保存对原始录音的引用范围（source_start/source_end，源帧整数），
//!   任何编辑都不会改写原始音频文件。
//! * 不同采样率的素材在导入时确定唯一的转换策略（见 resample 模块），
//!   时间线上只出现工程帧；源帧 -> 工程帧用精确有理映射即时换算，
//!   不做任何会累积的浮点步进。

use serde::{Deserialize, Serialize};

pub type Frame = i64;

/// 三条固定角色轨道的 ID。新建工程自动创建；旧单轨工程归一化到 legacy 轨道。
pub const TRACK_HOST: &str = "track_host";
pub const TRACK_GUEST: &str = "track_guest";
pub const TRACK_AMBIENCE: &str = "track_ambience";
pub const TRACK_LEGACY: &str = "track_legacy";

/// 一段对原始素材的引用。编辑只改这里的范围与顺序，原文件不被覆盖。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: String,
    pub source_id: String,
    /// 所属轨道。旧工程快照缺省该字段，反序列化后为空串，载入时归一化到 legacy 轨道。
    #[serde(default)]
    pub track_id: String,
    /// 源文件中的起始帧（源采样率下的整数帧）。
    pub source_start: Frame,
    /// 源文件中的结束帧（不含）。
    pub source_end: Frame,
    /// 在工程时间线上的起始帧（工程采样率下的整数帧）。
    /// 允许为负：负起点表示录音起点早于工程零点，越过零点的部分按 ZeroPolicy 处理。
    pub timeline_start: Frame,
    /// 与同一轨道下一条片段的交叉淡化长度（工程帧）；最后一条为 0。
    pub crossfade_out: Frame,
}

impl Item {
    /// 引用的源帧数量。
    pub fn source_len(&self) -> Frame {
        self.source_end - self.source_start
    }

    /// 转换到工程采样率后占用的工程帧数（精确四舍五入，仅用于长度换算，
    /// 真正逐帧渲染时按有理映射取整，不累积）。
    pub fn project_len(&self, source_rate: u32, project_rate: u32) -> Frame {
        frames_to_rate(self.source_len(), source_rate, project_rate)
    }
}

/// 静音片段。保留静音是显式的时间线对象，而不是「删掉音频后剩下的空隙」。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SilenceItem {
    pub id: String,
    pub timeline_start: Frame,
    pub length: Frame,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub id: String,
    pub path: String,
    pub original_rate: u32,
    pub channels: u16,
    /// 原始录音的总帧数。原文件只读，该值在导入时确定后不变。
    pub frame_count: Frame,
}

/// 时间线轨道。隐藏只是显示状态：隐藏轨道上的片段仍参与所属剪辑组的
/// 全部联动规则（波纹移动、锁轨冲突、跨淡化校验），不会因为未显示而失去同步。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub role: String, // host | guest | ambience | legacy
    /// 录音起点相对工程零点的独立偏移（工程帧，可为负）。
    /// 同一条原始时间在不同轨道上通过各自偏移对齐；偏移只是位置语义，
    /// 真正的片段位置仍以 item.timeline_start 为准。
    pub start_offset: Frame,
    pub hidden: bool,
    /// 锁定后拒绝一切会改变该轨片段位置/范围的编辑（含联动波纹）。
    pub locked: bool,
}

/// 剪辑组：把多条轨道上时间对齐的片段绑定为一个联动单元。
/// 组可嵌套（parent_id 指向上级组）；嵌套关系不允许成环。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// 成员片段。子组成员通过递归展开隐式包含，不在此列重复。
    #[serde(default)]
    pub item_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub version: i64,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub groups: Vec<EditGroup>,
    pub items: Vec<Item>,
    pub silences: Vec<SilenceItem>,
}

impl Project {
    pub fn track(&self, track_id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == track_id)
    }

    pub fn is_locked(&self, track_id: &str) -> bool {
        self.track(track_id).is_some_and(|t| t.locked)
    }

    /// 旧单轨工程的归一化：没有轨道信息时补一条 legacy 轨，
    /// 所有没有 track_id 的片段挂到该轨。不转换任何素材、不改写源文件。
    pub fn normalize_legacy(&mut self) {
        if self.tracks.is_empty() {
            self.tracks.push(Track {
                id: TRACK_LEGACY.to_string(),
                name: "单轨工程".to_string(),
                role: "legacy".to_string(),
                start_offset: 0,
                hidden: false,
                locked: false,
            });
        }
        for it in &mut self.items {
            if it.track_id.is_empty() {
                it.track_id = TRACK_LEGACY.to_string();
            }
        }
        self.groups
            .retain(|g| g.item_ids.iter().any(|id| self.items.iter().any(|i| &i.id == id)));
    }

    /// 新建三轨工程的标准轨道集（主持人 / 嘉宾 / 现场气氛），起点偏移均为 0。
    pub fn default_tracks() -> Vec<Track> {
        vec![
            Track {
                id: TRACK_HOST.to_string(),
                name: "主持人".to_string(),
                role: "host".to_string(),
                start_offset: 0,
                hidden: false,
                locked: false,
            },
            Track {
                id: TRACK_GUEST.to_string(),
                name: "嘉宾".to_string(),
                role: "guest".to_string(),
                start_offset: 0,
                hidden: false,
                locked: false,
            },
            Track {
                id: TRACK_AMBIENCE.to_string(),
                name: "现场气氛".to_string(),
                role: "ambience".to_string(),
                start_offset: 0,
                hidden: false,
                locked: false,
            },
        ]
    }
}

/// 把某个采样率下的帧数换算为另一采样率下的帧数（最近整数，仅用于长度/边界换算）。
pub fn frames_to_rate(frames: Frame, from_rate: u32, to_rate: u32) -> Frame {
    if from_rate == to_rate {
        return frames;
    }
    let num = to_rate as i128 * frames as i128;
    let den = from_rate as i128;
    // 向最近整数取整，正负都成立（本系统帧均为非负）。
    ((num + den / 2) / den) as Frame
}

/// 工程时间线帧 -> 某素材的源帧（用于「试听游标定位到原素材位置」）。
/// 返回最近的源帧整数。
pub fn project_frame_to_source(
    project_frame: Frame,
    source_rate: u32,
    project_rate: u32,
) -> Frame {
    frames_to_rate(project_frame, project_rate, source_rate)
}

/// 交叉淡化余量检查结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CrossfadeMargin {
    pub item_id: String,
    pub requested: Frame,
    /// 该片段尾部真实可用的工程帧数（引用范围之内、素材真实存在的样本）。
    pub tail_available: Frame,
    /// 下一片段头部真实可用的工程帧数。
    pub head_available: Frame,
    pub feasible: bool,
    pub max_feasible: Frame,
}

/// 计算 item 与它的下一条相邻片段做交叉淡化时的真实余量。
///
/// 交叉淡化需要两侧真实样本：当前片段尾部取 `crossfade_out` 帧、
/// 下一片段头部取同等帧数。任何一侧引用范围到头（或素材到头）都会限制可用长度。
pub fn check_crossfade(
    item: &Item,
    next: &Item,
    sources: &std::collections::HashMap<String, Source>,
    project_rate: u32,
) -> CrossfadeMargin {
    let cur_src = &sources[&item.source_id];
    let next_src = &sources[&next.source_id];

    // 当前片段尾部余量：引用范围内剩余的源帧，换算成工程帧。
    let tail_source = item.source_len();
    let tail_avail = frames_to_rate(tail_source, cur_src.original_rate, project_rate);

    // 下一片段头部余量：其引用范围本身的长度（头部可用就是它引用的全部）。
    let head_source = next.source_len();
    let head_avail = frames_to_rate(head_source, next_src.original_rate, project_rate);

    let max = tail_avail.min(head_avail);
    CrossfadeMargin {
        item_id: item.id.clone(),
        requested: item.crossfade_out,
        tail_available: tail_avail,
        head_available: head_avail,
        feasible: item.crossfade_out <= max && item.crossfade_out > 0,
        max_feasible: max,
    }
}

/// 时间线总长度（工程帧）：取所有片段/静音的最远结束位置。
/// `rates` 提供每个素材的原始采样率，保证混合采样率下长度仍精确。
/// 负起点片段只统计落在工程零点之后的部分（零点之前的素材被裁切或拒绝）。
pub fn timeline_length(
    project: &Project,
    rates: &std::collections::HashMap<String, u32>,
) -> Frame {
    let mut end = 0i64;
    for it in &project.items {
        let src_rate = rates
            .get(&it.source_id)
            .copied()
            .unwrap_or(project.sample_rate);
        let len = frames_to_rate(it.source_len(), src_rate, project.sample_rate);
        end = end.max(it.timeline_start + len);
    }
    for s in &project.silences {
        end = end.max(s.timeline_start + s.length);
    }
    end.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_conversion_is_exact_and_drift_free() {
        // 48k 工程中 1 秒 44.1k 素材：44100 源帧 -> 48000 工程帧，精确。
        assert_eq!(frames_to_rate(44100, 44100, 48000), 48000);
        // 反向换算也精确。
        assert_eq!(frames_to_rate(48000, 48000, 44100), 44100);
        // 同采样率恒等。
        assert_eq!(frames_to_rate(123456, 48000, 48000), 123456);
        // 非整数比例取最近整数，而不是截断后反复累积。
        // 100 帧 8k -> 48k：100*6 = 600。
        assert_eq!(frames_to_rate(100, 8000, 48000), 600);
        // 1 帧 44100 -> 48000：48000/44100 ≈ 1.088 -> 1。
        assert_eq!(frames_to_rate(1, 44100, 48000), 1);
    }

    #[test]
    fn crossfade_margin_limited_by_short_head() {
        let mut sources = std::collections::HashMap::new();
        sources.insert(
            "a".into(),
            Source {
                id: "a".into(),
                path: "a.wav".into(),
                original_rate: 48000,
                channels: 1,
                frame_count: 1000,
            },
        );
        sources.insert(
            "b".into(),
            Source {
                id: "b".into(),
                path: "b.wav".into(),
                original_rate: 48000,
                channels: 1,
                frame_count: 1000,
            },
        );
        let item = Item {
            id: "i1".into(),
            source_id: "a".into(),
            track_id: crate::domain::TRACK_LEGACY.into(),
            source_start: 0,
            source_end: 1000,
            timeline_start: 0,
            crossfade_out: 200,
        };
        // 下一片段只有 50 帧引用 -> 头部余量 50，淡化 200 不可行。
        let next = Item {
            id: "i2".into(),
            source_id: "b".into(),
            track_id: crate::domain::TRACK_LEGACY.into(),
            source_start: 0,
            source_end: 50,
            timeline_start: 1000,
            crossfade_out: 0,
        };
        let m = check_crossfade(&item, &next, &sources, 48000);
        assert!(!m.feasible);
        assert_eq!(m.max_feasible, 50);
        assert_eq!(m.tail_available, 1000);
        assert_eq!(m.head_available, 50);
    }
}
