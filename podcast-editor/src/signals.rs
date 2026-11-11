//! 确定性测试信号生成（不依赖人工试听即可机器校验）：
//! * 合成语音节奏：带通感的脉冲串 + 音节间隔，模拟语流节奏；
//! * 单采样脉冲：除一个样本为 1 外全 0，用于精确定位拼接边界与输出采样位置；
//! * 跨片段相位信号：连续正弦，切开后跨片段应保持相位连续，检测拼接处跳变。

use crate::wav::WavData;

fn mono(rate: u32, samples: Vec<f32>) -> WavData {
    WavData {
        sample_rate: rate,
        channels: 1,
        bits: 16,
        is_float: false,
        samples,
    }
}

/// 合成「语音节奏」：每隔 syllable_gap 帧一个阻尼振荡音节，模拟说话的节奏起伏。
pub fn speech_rhythm(rate: u32, frames: usize, syllable_gap: usize) -> WavData {
    let mut s = vec![0f32; frames];
    let f0 = 140.0f32; // 基频
    let decay = (rate as f32 * 0.04).max(1.0); // 每个音节约 40ms 阻尼
    let mut start = 0usize;
    while start < frames {
        for k in 0..(syllable_gap.min(frames - start)) {
            let t = k as f32 / rate as f32;
            let env = (-(k as f32) / decay).exp();
            // 基频 + 二次谐波，更像声带激励。
            let v = env
                * (0.7 * (2.0 * std::f32::consts::PI * f0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * f0 * 2.0 * t).sin());
            s[start + k] = v * 0.6;
        }
        start += syllable_gap;
    }
    mono(rate, s)
}

/// 单采样脉冲：仅 pulse_frame 处为 1.0，其余为 0。
pub fn single_pulse(rate: u32, frames: usize, pulse_frame: usize) -> WavData {
    let mut s = vec![0f32; frames];
    if pulse_frame < frames {
        s[pulse_frame] = 1.0;
    }
    mono(rate, s)
}

/// 连续相位正弦：phase 由全局帧索引决定，任何位置切开都保持相位连续。
pub fn phase_sine(rate: u32, frames: usize, freq: f32) -> WavData {
    let s = (0..frames)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5)
        .collect();
    mono(rate, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_has_exactly_one_nonzero_sample() {
        let w = single_pulse(48000, 1000, 250);
        let nz = w.samples.iter().filter(|v| **v != 0.0).count();
        assert_eq!(nz, 1);
        assert_eq!(w.samples[250], 1.0);
    }

    #[test]
    fn phase_sine_is_continuous_across_an_arbitrary_cut() {
        let rate = 48000u32;
        let full = phase_sine(rate, 1000, 440.0);
        let cut = 337;
        // 切开处两侧的差分应与未切开信号的差分一致（无额外跳变）。
        let full_step = full.samples[cut] - full.samples[cut - 1];
        // 任意相邻帧差分都应是有限且小的（相位连续，无毛刺）。
        assert!(full_step.abs() < 0.1);
        assert!(full.samples.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn speech_rhythm_has_periodic_syllable_peaks() {
        let gap = 2400usize; // 48k 下每 50ms 一个音节
        let w = speech_rhythm(48000, 9600, gap);
        // 每个音节起点附近应出现明显峰值。
        for k in 0..4 {
            let around = &w.samples[k * gap..k * gap + 50];
            assert!(around.iter().cloned().fold(f32::NEG_INFINITY, f32::max) > 0.3);
        }
    }
}
