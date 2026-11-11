//! 时间线渲染（预览与导出共用同一函数，保证「所见即所导」）。
//!
//! 输出位置全部以工程采样帧表达。渲染前会对每条交叉淡化做真实余量校验，
//! 余量不足时返回结构化错误并给出可用长度，而不是悄悄截断。
//! 渲染支持取消；取消由调用方负责清理半成品文件（见 export 模块）。
//!
//! # 交叉淡化布局
//! 片段 i 的 `crossfade_out = x` 表示：它与下一片段在时间线上重叠 x 个工程帧。
//! 重叠区内片段 i 尾部按 cos 曲线淡出、片段 i+1 头部按 sin 曲线淡入（等功率，
//! sin²+cos²=1）。两侧都必须有真实样本（validate_crossfades 保证）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::domain::{check_crossfade, frames_to_rate, Item, Project, Source};
use crate::resample::interpolate_at;
use crate::wav::WavData;

#[derive(Debug, Clone)]
pub struct RenderError {
    pub item_id: String,
    pub requested: i64,
    pub max_feasible: i64,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "交叉淡化余量不足：片段 {} 请求 {} 帧，但两侧真实余量最多支持 {} 帧",
            self.item_id, self.requested, self.max_feasible
        )
    }
}
impl std::error::Error for RenderError {}

/// 渲染前置检查失败（缺源文件 / 越界引用 / 淡化余量不足），携带明确原因。
#[derive(Debug, Clone)]
pub struct RenderPreflightError {
    pub item_id: String,
    pub reason: String,
}
impl std::fmt::Display for RenderPreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "片段 {} 无法渲染：{}", self.item_id, self.reason)
    }
}
impl std::error::Error for RenderPreflightError {}

#[derive(Default)]
pub struct CancelToken {
    cancelled: AtomicBool,
}
impl CancelToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[allow(dead_code)]
pub fn shared_cancel() -> Arc<CancelToken> {
    Arc::new(CancelToken::default())
}

/// 校验全部交叉淡化：淡化只在「同一轨道」相邻片段之间成立，
/// 按每条轨道各自的真实素材余量计算；任一不可行即返回第一个错误（含可用长度）。
pub fn validate_crossfades(
    project: &Project,
    sources: &HashMap<String, Source>,
) -> std::result::Result<(), RenderError> {
    let sorted = sorted_items(project);
    // 按轨道切分相邻关系；不同轨道的片段即使时间重叠也不构成淡化对。
    let mut by_track: HashMap<String, Vec<&Item>> = HashMap::new();
    for it in &sorted {
        by_track.entry(it.track_id.clone()).or_default().push(it);
    }
    for seq in by_track.values() {
        for w in seq.windows(2) {
            let (it, next) = (&w[0], &w[1]);
            if it.crossfade_out > 0 {
                let m = check_crossfade(it, next, sources, project.sample_rate);
                if !m.feasible {
                    return Err(RenderError {
                        item_id: it.id.clone(),
                        requested: m.requested,
                        max_feasible: m.max_feasible,
                    });
                }
            }
        }
    }
    Ok(())
}

/// 渲染前置检查：所有引用的源文件必须存在且已加载、引用范围必须在素材帧数内。
/// 缺失源文件在这里明确报错，而不是静默当静音处理。
pub fn preflight(
    project: &Project,
    sources_meta: &HashMap<String, Source>,
    loaded: &LoadedSources,
) -> std::result::Result<(), RenderPreflightError> {
    for it in &project.items {
        let Some(meta) = sources_meta.get(&it.source_id) else {
            return Err(RenderPreflightError {
                item_id: it.id.clone(),
                reason: format!("源素材 {} 的元数据缺失", it.source_id),
            });
        };
        if !loaded.contains_key(&it.source_id) {
            return Err(RenderPreflightError {
                item_id: it.id.clone(),
                reason: format!("源文件 {} 缺失或无法读取（路径 {}）", it.source_id, meta.path),
            });
        }
        if it.source_start < 0 || it.source_end > meta.frame_count || it.source_end <= it.source_start
        {
            return Err(RenderPreflightError {
                item_id: it.id.clone(),
                reason: format!(
                    "引用范围 [{},{}) 超出素材帧数 {}",
                    it.source_start, it.source_end, meta.frame_count
                ),
            });
        }
    }
    Ok(())
}

fn sorted_items(project: &Project) -> Vec<Item> {
    let mut v = project.items.clone();
    v.sort_by_key(|i| (i.track_id.clone(), i.timeline_start));
    v
}

/// 已加载的源音频（原始采样率，只读）。
pub type LoadedSources = HashMap<String, Arc<WavData>>;

/// 同轨上一条片段（用于计算该片段的淡化进入量）。
/// 只在同一轨道内查找；不同轨道的片段时间重叠也不产生淡化关系。
fn prev_on_same_track<'a>(sorted: &'a [Item], idx: usize) -> Option<&'a Item> {
    let cur = &sorted[idx];
    sorted[..idx]
        .iter()
        .rev()
        .find(|i| i.track_id == cur.track_id)
}

/// 渲染整条时间线，返回交错 f32（工程采样率、工程声道数）。
pub fn render_timeline(
    project: &Project,
    sources_meta: &HashMap<String, Source>,
    loaded: &LoadedSources,
    cancel: Option<&CancelToken>,
) -> Result<Vec<f32>> {
    preflight(project, sources_meta, loaded).map_err(|e| anyhow::anyhow!(e))?;
    validate_crossfades(project, sources_meta).map_err(|e| anyhow::anyhow!(e))?;

    let rate_map: HashMap<String, u32> = sources_meta
        .iter()
        .map(|(k, v)| (k.clone(), v.original_rate))
        .collect();
    let total = crate::domain::timeline_length(project, &rate_map);
    let ch = project.channels as usize;
    let mut buf = vec![0f32; total as usize * ch];

    // 静音片段显式铺零（保留静音）。
    for s in &project.silences {
        let start = s.timeline_start.max(0) as usize;
        let end = (s.timeline_start + s.length).min(total).max(0) as usize;
        for slot in buf.iter_mut().take(end * ch).skip(start * ch) {
            *slot = 0.0;
        }
    }

    let items = sorted_items(project);
    let pr = project.sample_rate as i64;

    for (idx, item) in items.iter().enumerate() {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            bail!("render cancelled");
        }
        let src = loaded
            .get(&item.source_id)
            .ok_or_else(|| anyhow::anyhow!("missing loaded source {}", item.source_id))?;
        let sr = src.sample_rate as i64;
        let src_ch = src.channels as usize;

        let plen = frames_to_rate(item.source_len(), src.sample_rate, project.sample_rate);
        let xf_out = item.crossfade_out;
        // 淡化进入量只来自同一条轨道上一条片段；不同轨时间重叠互不淡化。
        let xf_in = prev_on_same_track(&items, idx)
            .map(|p| p.crossfade_out)
            .unwrap_or(0);

        // 片段在时间线上实际写入的起点：与同轨上一片段重叠 xf_in 帧。
        let write_base = item.timeline_start - xf_in;

        for pf in 0..plen {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                bail!("render cancelled");
            }
            let out_frame = write_base + pf;
            if out_frame < 0 || out_frame >= total {
                continue;
            }

            // 头部淡入（前 xf_in 帧）与尾部淡出（后 xf_out 帧），等功率曲线。
            let mut gain = 1.0f32;
            if xf_in > 0 && pf < xf_in {
                let t = (pf + 1) as f32 / xf_in as f32;
                gain *= (t * std::f32::consts::PI / 2.0).sin();
            }
            if xf_out > 0 && pf >= plen - xf_out {
                let t = (pf - (plen - xf_out) + 1) as f32 / xf_out as f32;
                gain *= (t * std::f32::consts::PI / 2.0).cos();
            }

            // 每帧独立有理映射回源位置，绝不累加步进。
            let src_num = item.source_start * pr + pf * sr;
            for c in 0..ch {
                let sc = c.min(src_ch - 1);
                let v = interpolate_at(src, src_num, pr, sc) * gain;
                let idxbuf = (out_frame as usize) * ch + c;
                buf[idxbuf] += v;
            }
        }
    }

    // 多轨混音保留真实线性叠加（同帧三轨脉冲应为 3.0），不在渲染端硬钳。
    // 落盘时由 wav::write_wav 在 PCM 转换处统一限幅；分轨各自单轨，天然落在 [-1,1]。
    Ok(buf)
}

/// 预览与导出共用入口：同一函数、同一版本的片段图。
pub fn render(
    project: &Project,
    sources_meta: &HashMap<String, Source>,
    loaded: &LoadedSources,
    cancel: Option<&CancelToken>,
) -> Result<Vec<f32>> {
    render_timeline(project, sources_meta, loaded, cancel)
}

/// 分轨渲染结果：track_id -> 该轨独占的渲染缓冲（与混音等长，未出现的帧为零）。
/// 分轨与混音由同一工程修订、同一 render_timeline 路径产生，保证引用一致。
pub struct Stems {
    #[allow(dead_code)]
    pub frame_count: usize,
    pub tracks: HashMap<String, Vec<f32>>,
}

/// 逐轨渲染：每次只保留一条轨道的片段走完整渲染路径，
/// 因此分轨之和（帧内相加）与整条时间线混音完全一致。
pub fn render_stems(
    project: &Project,
    sources_meta: &HashMap<String, Source>,
    loaded: &LoadedSources,
    cancel: Option<&CancelToken>,
) -> Result<Stems> {
    preflight(project, sources_meta, loaded).map_err(|e| anyhow::anyhow!(e))?;
    // 以实际出现的轨道为准：旧工程/归一化轨即使不在 project.tracks 里也要出分轨。
    let mut track_ids: Vec<String> = project
        .tracks
        .iter()
        .map(|t| t.id.clone())
        .chain(project.items.iter().map(|i| i.track_id.clone()))
        .collect();
    track_ids.sort();
    track_ids.dedup();
    let rate_map: HashMap<String, u32> = sources_meta
        .iter()
        .map(|(k, v)| (k.clone(), v.original_rate))
        .collect();
    let total = crate::domain::timeline_length(project, &rate_map) as usize;
    let mut tracks = HashMap::new();
    for tid in track_ids {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            bail!("render cancelled");
        }
        let mut sub = project.clone();
        sub.items.retain(|i| i.track_id == tid);
        sub.silences.clear();
        let buf = render_timeline(&sub, sources_meta, loaded, cancel)?;
        // 统一到与混音相同的长度，缺失尾部补零。
        let ch = project.channels as usize;
        let mut fixed = vec![0f32; total * ch];
        let n = buf.len().min(fixed.len());
        fixed[..n].copy_from_slice(&buf[..n]);
        tracks.insert(tid, fixed);
    }
    Ok(Stems {
        frame_count: total,
        tracks,
    })
}
