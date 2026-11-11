//! 最小 WAV 读写（PCM 16/24/32-bit 与 IEEE float32）。
//! 原录音只通过本模块只读加载；写出仅用于导出，永不回写源文件。

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct WavData {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub is_float: bool,
    /// 交错样本，f32 归一化到 [-1, 1]。
    pub samples: Vec<f32>,
}

impl WavData {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels as usize
    }

    /// 取第 frame 帧、第 ch 声道的样本（越界返回 0）。
    pub fn sample(&self, frame: usize, ch: usize) -> f32 {
        let idx = frame * self.channels as usize + ch;
        self.samples.get(idx).copied().unwrap_or(0.0)
    }
}

pub fn read_wav(path: &str) -> Result<WavData> {
    let bytes = std::fs::read(path)?;
    parse_wav(&bytes)
}

pub fn parse_wav(bytes: &[u8]) -> Result<WavData> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16, u16)> = None; // tag,ch,rate,bits,blockalign
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        if body_start + size > bytes.len() {
            bail!("truncated chunk {}", String::from_utf8_lossy(id));
        }
        match id {
            b"fmt " => {
                let b = &bytes[body_start..body_start + size];
                let tag = u16::from_le_bytes(b[0..2].try_into().unwrap());
                let ch = u16::from_le_bytes(b[2..4].try_into().unwrap());
                let rate = u32::from_le_bytes(b[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes(b[14..16].try_into().unwrap());
                let block = u16::from_le_bytes(b[12..14].try_into().unwrap());
                fmt = Some((tag, ch, rate, bits, block));
            }
            b"data" => data = Some(&bytes[body_start..body_start + size]),
            _ => {} // LIST/fact 等跳过
        }
        pos = body_start + size + (size & 1); // 块按偶数字节对齐
    }

    let (tag, channels, sample_rate, bits, _block): (u16, u16, u32, u16, u16) =
        fmt.ok_or_else(|| anyhow::anyhow!("missing fmt chunk"))?;
    let data = data.ok_or_else(|| anyhow::anyhow!("missing data chunk"))?;
    let is_float = tag == 3;
    if !is_float && tag != 1 {
        bail!("unsupported format tag {tag}");
    }

    let ch = channels as usize;
    let bytes_per_sample = bits as usize / 8;
    if data.len() % bytes_per_sample != 0 {
        bail!("data size not aligned to sample width");
    }
    let total_samples = data.len() / bytes_per_sample;
    let mut samples = Vec::with_capacity(total_samples);

    for i in 0..total_samples {
        let off = i * bytes_per_sample;
        let v: f32 = if is_float {
            match bits {
                32 => f32::from_le_bytes(data[off..off + 4].try_into().unwrap()),
                64 => {
                    let d = f64::from_le_bytes(data[off..off + 8].try_into().unwrap());
                    d as f32
                }
                _ => bail!("unsupported float bits {bits}"),
            }
        } else {
            match bits {
                16 => i16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as f32 / 32768.0,
                24 => {
                    let lo = data[off] as i32;
                    let mid = data[off + 1] as i32;
                    let hi = data[off + 2] as i32;
                    let raw = lo | (mid << 8) | (hi << 16);
                    let signed = if raw & 0x800000 != 0 { raw | !0xFFFFFF } else { raw };
                    signed as f32 / 8388608.0
                }
                32 => i32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as f32 / 2147483648.0,
                _ => bail!("unsupported PCM bits {bits}"),
            }
        };
        samples.push(v.clamp(-1.0, 1.0));
    }

    Ok(WavData {
        sample_rate,
        channels,
        bits,
        is_float,
        samples: if total_samples % ch == 0 {
            samples
        } else {
            bail!("sample count not divisible by channel count");
        },
    })
}

/// 写出 16-bit PCM WAV。导出统一用 16-bit，确定性强、便于测试边界幅度。
pub fn write_wav(path: &str, sample_rate: u32, channels: u16, frames: &[f32]) -> Result<()> {
    let _ch = channels as usize;
    let data_bytes = frames.len() * 2;
    let mut out = Vec::with_capacity(44 + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data_bytes as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for &v in frames {
        let s = (v.clamp(-1.0, 1.0) * 32767.0).round_ties_even() as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}
