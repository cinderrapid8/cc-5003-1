//! 重采样策略（导入时即确定，全工程唯一）。
//!
//! 策略：线性插值。映射关系为
//!   源位置(有理) = 工程帧 * source_rate / project_rate
//! 每个输出帧独立用整数有理运算计算其对应的源位置，再在两个相邻源样本间
//! 线性插值。不存在「累加步长」，因此浮点/取整误差不会随时间累积成漂移。

use crate::wav::WavData;

/// 取源素材在（源采样率下的）有理位置 `num/den` 处、某声道的线性插值样本。
/// num/den 是源帧坐标；den 固定为 project_rate，num = out_frame * source_rate。
#[inline]
pub fn interpolate_at(src: &WavData, num: i64, den: i64, ch: usize) -> f32 {
    let total = src.frames() as i64;
    if total == 0 {
        return 0.0;
    }
    let i0 = num / den;
    let frac = (num % den) as f32 / den as f32;
    let i1 = i0 + 1;
    let a = if i0 >= 0 && i0 < total {
        src.sample(i0 as usize, ch)
    } else if i0 < 0 {
        src.sample(0, ch)
    } else {
        src.sample((total - 1) as usize, ch)
    };
    let b = if i1 >= 0 && i1 < total {
        src.sample(i1 as usize, ch)
    } else if i1 < 0 {
        src.sample(0, ch)
    } else {
        src.sample((total - 1) as usize, ch)
    };
    a + (b - a) * frac
}

/// 将整段源素材重采样到目标采样率，返回交错 f32（声道数与源一致）。
#[allow(dead_code)]
pub fn resample_wav(src: &WavData, target_rate: u32) -> WavData {
    if src.sample_rate == target_rate {
        return src.clone();
    }
    let ch = src.channels as usize;
    let out_frames =
        ((src.frames() as i128 * target_rate as i128) / src.sample_rate as i128) as usize;
    let mut out = vec![0f32; out_frames * ch];
    let sr = src.sample_rate as i64;
    let pr = target_rate as i64;
    for of in 0..out_frames as i64 {
        // 每帧独立有理定位，绝不累加。
        let num = of * sr;
        for c in 0..ch {
            out[(of as usize) * ch + c] = interpolate_at(src, num, pr, c);
        }
    }
    WavData {
        sample_rate: target_rate,
        channels: src.channels,
        bits: src.bits,
        is_float: src.is_float,
        samples: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, samples: Vec<f32>) -> WavData {
        WavData {
            sample_rate: rate,
            channels: 1,
            bits: 16,
            is_float: false,
            samples,
        }
    }

    #[test]
    fn resample_lengths_are_exact() {
        // 44100 -> 48000：1 秒素材必须恰好得到 48000 帧。
        let src = wav(44100, vec![0.0; 44100]);
        let out = resample_wav(&src, 48000);
        assert_eq!(out.frames(), 48000);
        // 8000 -> 48000：整数倍，100 帧 -> 600 帧。
        let src2 = wav(8000, vec![0.0; 100]);
        assert_eq!(resample_wav(&src2, 48000).frames(), 600);
    }

    #[test]
    fn no_drift_across_many_frames() {
        // 直流信号重采样后幅值必须保持，末帧不能因累积步进错位。
        let src = wav(44100, vec![0.5f32; 44100]);
        let out = resample_wav(&src, 48000);
        assert_eq!(out.frames(), 48000);
        assert!((out.sample(0, 0) - 0.5).abs() < 1e-6);
        assert!((out.sample(47999, 0) - 0.5).abs() < 1e-6);
        // 线性斜坡：末帧应近似源末帧值，不出现相位漂移造成的跳变。
        let ramp: Vec<f32> = (0..44100).map(|i| i as f32 / 44100.0).collect();
        let src3 = wav(44100, ramp);
        let out3 = resample_wav(&src3, 48000);
        assert!((out3.sample(47999, 0) - 1.0).abs() < 2e-4);
    }
}
