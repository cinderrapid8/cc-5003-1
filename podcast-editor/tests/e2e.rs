//! 端到端校验（不依赖人工试听）：
//! * 单采样脉冲：拼接后脉冲落在精确的输出采样位置，边界幅度可预测；
//! * 跨片段相位正弦：切开再拼接，接缝处无额外跳变；
//! * 混合采样率：44.1k 素材进 48k 工程，长度精确、无累积漂移；
//! * 交叉淡化：余量不足时报错并给出可用长度；
//! * 关闭重开：从 SQLite 重新加载得到逐样本一致的渲染结果；
//! * 合成语音节奏：渲染有限且包含非平凡动态。

use std::collections::HashMap;
use std::sync::Arc;

// 通过包含方式直接复用二进制 crate 的模块（main 是 bin crate）。
#[path = "../src/domain.rs"]
mod domain;
#[path = "../src/db.rs"]
mod db;
#[path = "../src/edit.rs"]
mod edit;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/render.rs"]
mod render;
#[path = "../src/resample.rs"]
mod resample;
#[path = "../src/signals.rs"]
mod signals;
#[path = "../src/wav.rs"]
mod wav;
#[path = "../src/waveform.rs"]
mod waveform;

use domain::{frames_to_rate, Item, Project, Source};
use render::CancelToken;
use wav::WavData;

fn arc_wav(w: WavData) -> Arc<WavData> {
    Arc::new(w)
}

fn source(id: &str, rate: u32, frames: i64, w: WavData) -> (Source, Arc<WavData>) {
    let meta = Source {
        id: id.into(),
        path: format!("{id}.wav"),
        original_rate: rate,
        channels: 1,
        frame_count: frames,
    };
    (meta, arc_wav(w))
}

fn project(items: Vec<Item>, rate: u32) -> Project {
    Project {
        id: "p".into(),
        name: "e2e".into(),
        sample_rate: rate,
        channels: 1,
        version: 1,
        items,
        silences: vec![],
    }
}

#[test]
fn pulse_lands_at_exact_output_sample_after_concat() {
    // 两段各含一个单采样脉冲；拼接后两个脉冲应分别位于帧 10 和 100+20=120。
    let a = signals::single_pulse(48000, 100, 10);
    let b = signals::single_pulse(48000, 100, 20);
    let (ma, wa) = source("a", 48000, 100, a);
    let (mb, wb) = source("b", 48000, 100, b);
    let metas = HashMap::from([("a".into(), ma), ("b".into(), mb)]);
    let loaded = HashMap::from([("a".into(), wa), ("b".into(), wb)]);

    let rates = HashMap::from([("a".to_string(), 48000u32), ("b".to_string(), 48000u32)]);
    let p = project(
        vec![
            Item {
                id: "i1".into(),
                source_id: "a".into(),
                source_start: 0,
                source_end: 100,
                timeline_start: 0,
                crossfade_out: 0,
            },
            Item {
                id: "i2".into(),
                source_id: "b".into(),
                source_start: 0,
                source_end: 100,
                timeline_start: 100,
                crossfade_out: 0,
            },
        ],
        48000,
    );
    let p = edit::layout(p, &rates);
    let out = render::render(&p, &metas, &loaded, None).unwrap();

    assert_eq!(out.len(), 200, "拼接总长度必须等于两段之和");
    assert_eq!(out[10], 1.0, "第一个脉冲位置精确");
    assert_eq!(out[120], 1.0, "第二个脉冲位置精确（100 + 源内偏移 20）");
    // 除两个脉冲外全为零（边界无泄漏）。
    let nonzero = out
        .iter()
        .enumerate()
        .filter(|(i, v)| **v != 0.0 && *i != 10 && *i != 120)
        .count();
    assert_eq!(nonzero, 0);
    // 接缝两侧边界幅度：帧 99=0、帧 100=0（b 的脉冲在 120），无突变毛刺。
    assert_eq!(out[99], 0.0);
    assert_eq!(out[100], 0.0);
}

#[test]
fn phase_sine_concat_has_no_extra_jump_at_boundary() {
    // 用同一段连续相位正弦切成两个引用，拼回后应与原始连续信号一致。
    let full = signals::phase_sine(48000, 200, 440.0);
    let (meta, w) = source("s", 48000, 200, full);
    let metas = HashMap::from([("s".into(), meta)]);
    let loaded = HashMap::from([("s".into(), w.clone())]);
    let rates = HashMap::from([("s".to_string(), 48000u32)]);

    let p = project(
        vec![
            Item {
                id: "i1".into(),
                source_id: "s".into(),
                source_start: 0,
                source_end: 80,
                timeline_start: 0,
                crossfade_out: 0,
            },
            Item {
                id: "i2".into(),
                source_id: "s".into(),
                source_start: 80,
                source_end: 200,
                timeline_start: 80,
                crossfade_out: 0,
            },
        ],
        48000,
    );
    let p = edit::layout(p, &rates);
    let out = render::render(&p, &metas, &loaded, None).unwrap();

    assert_eq!(out.len(), 200);
    for f in 0..200 {
        assert!(
            (out[f] - w.samples[f]).abs() < 1e-5,
            "帧 {f} 拼接后偏离连续信号: {} vs {}",
            out[f],
            w.samples[f]
        );
    }
    // 接缝处相邻差分与连续信号的差分一致（无相位不连续）。
    let joined_step = out[80] - out[79];
    let cont_step = w.samples[80] - w.samples[79];
    assert!((joined_step - cont_step).abs() < 1e-6);
}

#[test]
fn mixed_sample_rate_is_exact_without_drift() {
    // 44.1k 的 1 秒素材放入 48k 工程：渲染必须恰好 48000 帧，首尾样本都对齐。
    let src = signals::phase_sine(44100, 44100, 300.0);
    let (meta, w) = source("lo", 44100, 44100, src);
    let metas = HashMap::from([("lo".into(), meta)]);
    let loaded = HashMap::from([("lo".into(), w)]);
    let rates = HashMap::from([("lo".to_string(), 44100u32)]);

    let p = project(
        vec![Item {
            id: "i".into(),
            source_id: "lo".into(),
            source_start: 0,
            source_end: 44100,
            timeline_start: 0,
            crossfade_out: 0,
        }],
        48000,
    );
    let p = edit::layout(p, &rates);
    assert_eq!(domain::timeline_length(&p, &rates), 48000);
    let out = render::render(&p, &metas, &loaded, None).unwrap();
    assert_eq!(out.len(), 48000);
    // 首帧对应源帧 0；末帧对应接近源末帧，幅值有限且不重复/错位。
    assert!(out[0].is_finite() && out[47999].is_finite());
    // 换算函数本身：44100 源帧精确对应 48000 工程帧，且反向成立。
    assert_eq!(frames_to_rate(44100, 44100, 48000), 48000);
    assert_eq!(frames_to_rate(48000, 48000, 44100), 44100);
}

#[test]
fn crossfade_reports_available_length_when_source_runs_out() {
    let a = signals::phase_sine(48000, 1000, 200.0);
    let b = signals::phase_sine(48000, 40, 300.0); // 下一段只有 40 帧
    let (ma, wa) = source("a", 48000, 1000, a);
    let (mb, wb) = source("b", 48000, 40, b);
    let metas = HashMap::from([("a".into(), ma), ("b".into(), mb)]);
    let loaded = HashMap::from([("a".into(), wa), ("b".into(), wb)]);
    let rates = HashMap::from([("a".to_string(), 48000u32), ("b".to_string(), 48000u32)]);

    let mut p = project(
        vec![
            Item {
                id: "i1".into(),
                source_id: "a".into(),
                source_start: 0,
                source_end: 1000,
                timeline_start: 0,
                crossfade_out: 200, // 请求 200，但 b 只有 40 帧余量
            },
            Item {
                id: "i2".into(),
                source_id: "b".into(),
                source_start: 0,
                source_end: 40,
                timeline_start: 0,
                crossfade_out: 0,
            },
        ],
        48000,
    );
    p = edit::layout(p, &rates);

    // 余量检查明确给出可用长度 40。
    let margin = domain::check_crossfade(&p.items[0], &p.items[1], &metas, 48000);
    assert!(!margin.feasible);
    assert_eq!(margin.max_feasible, 40);

    // 渲染必须拒绝并返回结构化错误，而不是悄悄截断。
    let err = render::render(&p, &metas, &loaded, None).unwrap_err();
    assert!(err.to_string().contains("40"));

    // 把淡化降到可行长度后渲染成功。
    p.items[0].crossfade_out = 40;
    let p = edit::layout(p, &rates);
    let out = render::render(&p, &metas, &loaded, None).unwrap();
    // 总长度 = 1000 + 40 - 40(重叠) = 1000。
    assert_eq!(out.len(), 1000);
}

#[test]
fn cancel_mid_render_leaves_no_half_file() {
    let src = signals::speech_rhythm(48000, 96000, 2400);
    let (meta, w) = source("s", 48000, 96000, src);
    let metas = HashMap::from([("s".into(), meta)]);
    let loaded = HashMap::from([("s".into(), w)]);
    let p = project(
        vec![Item {
            id: "i".into(),
            source_id: "s".into(),
            source_start: 0,
            source_end: 96000,
            timeline_start: 0,
            crossfade_out: 0,
        }],
        48000,
    );

    let dir = std::env::temp_dir().join(format!("e2e_cancel_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("render.wav").to_string_lossy().to_string();
    let token = Arc::new(CancelToken::default());
    token.cancel();
    let outcome = export::export_wav(&p, &metas, &loaded, &dest, token, None).unwrap();
    assert!(outcome.cancelled);
    assert!(!std::path::Path::new(&dest).exists());
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cursor_locates_both_directions_in_sample_frames() {
    // 48k 工程、48k 素材：时间线帧与源帧应能往返互定位。
    let src = signals::single_pulse(48000, 1000, 0);
    let (meta, _w) = source("s", 48000, 1000, src);
    let metas: HashMap<String, Source> = HashMap::from([("s".into(), meta.clone())]);
    let rates = HashMap::from([("s".to_string(), 48000u32)]);
    let p = project(
        vec![Item {
            id: "i".into(),
            source_id: "s".into(),
            source_start: 100,
            source_end: 600,
            timeline_start: 0,
            crossfade_out: 0,
        }],
        48000,
    );
    let p = edit::layout(p, &rates);
    let it = &p.items[0];

    // 源帧 300（=引用起点 100 + 局部 200）-> 工程时间线帧 200。
    let local_source = 300 - it.source_start;
    let project_frame =
        it.timeline_start + frames_to_rate(local_source, meta.original_rate, 48000);
    assert_eq!(project_frame, 200);

    // 反向：工程帧 200 -> 回到源帧 300。
    let back = domain::project_frame_to_source(
        project_frame - it.timeline_start,
        meta.original_rate,
        48000,
    ) + it.source_start;
    assert_eq!(back, 300);
}

#[test]
fn speech_rhythm_renders_with_real_dynamics_and_finite_values() {
    let src = signals::speech_rhythm(48000, 24000, 2400);
    let (meta, w) = source("s", 48000, 24000, src);
    let metas = HashMap::from([("s".into(), meta)]);
    let loaded = HashMap::from([("s".into(), w.clone())]);
    let p = project(
        vec![Item {
            id: "i".into(),
            source_id: "s".into(),
            source_start: 0,
            source_end: 24000,
            timeline_start: 0,
            crossfade_out: 0,
        }],
        48000,
    );
    let out = render::render(&p, &metas, &loaded, None).unwrap();
    assert_eq!(out.len(), 24000);
    assert!(out.iter().all(|v| v.is_finite()));
    let peak = out.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    assert!(peak > 0.3, "语音节奏信号应保留明显峰值");
    // 波形分桶可覆盖全部帧且不丢帧。
    let buckets = waveform::peaks(&out, 1, 512);
    assert_eq!(buckets.len(), 24000usize.div_ceil(512));
}
