//! 波形峰值分桶。前端按缩放级别请求每桶 N 帧的 (min,max)，
//! 用 Canvas 绘制可缩放波形。计算结果不带时间线语义——版本匹配由状态层负责，
//! 撤销后仍在计算的旧波形响应会因版本号不符被丢弃。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeakBucket {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
}

/// 对单声道/交错缓冲按每 `bucket_frames` 个工程帧分桶。
/// `channels` 表示交错声道数；多声道先在帧内混缩成单声道再统计。
pub fn peaks(frames: &[f32], channels: usize, bucket_frames: usize) -> Vec<PeakBucket> {
    let ch = channels.max(1);
    let total_frames = frames.len() / ch;
    let bucket = bucket_frames.max(1);
    let n = total_frames.div_ceil(bucket);
    let mut out = Vec::with_capacity(n);

    for b in 0..n {
        let start = b * bucket;
        let end = (start + bucket).min(total_frames);
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        let mut sq = 0.0f64;
        let mut count = 0u64;
        for f in start..end {
            // 帧内多声道取平均，避免某声道极端值误导显示。
            let mut acc = 0.0f32;
            for c in 0..ch {
                acc += frames[f * ch + c];
            }
            let v = acc / ch as f32;
            if v < min {
                min = v;
            }
            if v > max {
                max = v;
            }
            sq += (v as f64) * (v as f64);
            count += 1;
        }
        out.push(PeakBucket {
            min,
            max,
            rms: (sq / count.max(1) as f64).sqrt() as f32,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_buckets_cover_all_frames() {
        let frames: Vec<f32> = (0..100).map(|i| (i as f32) * 0.01 - 0.5).collect();
        let p = peaks(&frames, 1, 10);
        assert_eq!(p.len(), 10);
        // 第一桶包含帧 0..10：v=i*0.01-0.5，最大值在 i=9 -> -0.41，最小在 i=0 -> -0.5。
        assert!((p[0].max - (-0.41)).abs() < 1e-6);
        assert!((p[0].min - (-0.5)).abs() < 1e-6);
        // 非整除长度也不丢帧。
        let p2 = peaks(&frames, 1, 30);
        assert_eq!(p2.len(), 4);
        assert!(p2[3].max >= p2[3].min);
    }
}
