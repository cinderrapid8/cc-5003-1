//! 导出流程。
//!
//! 不变量：
//! * 先写 `目标名.tmp.<pid>` 临时文件，渲染/写盘全程可取消；
//! * 只有完整渲染并成功落盘后才 rename 为目标文件（同目录原子替换）；
//! * 取消或出错时删除临时文件，目标路径上不会出现「看似成功」的半成品；
//! * 预览与导出调用同一个 render::render，片段图完全一致。

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::db::Db;
use crate::domain::{Project, Source};
use crate::render::{render, CancelToken, LoadedSources};
use crate::wav::write_wav;

pub struct ExportOutcome {
    pub path: String,
    pub frames: usize,
    pub cancelled: bool,
}

/// 执行导出。`cancel` 触发时返回 Ok(cancelled=true) 且保证无目标文件残留。
pub fn export_wav(
    project: &Project,
    sources_meta: &std::collections::HashMap<String, Source>,
    loaded: &LoadedSources,
    dest: &str,
    cancel: Arc<CancelToken>,
    db: Option<&Db>,
) -> Result<ExportOutcome> {
    let dest_path = Path::new(dest);
    let dir = dest_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无效导出路径 {dest}"))?;
    let file_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("无效导出文件名"))?;
    let tmp_path = dir.join(format!(
        ".{}.tmp.{}",
        file_name,
        std::process::id()
    ));
    let tmp_str = tmp_path.to_string_lossy().to_string();

    // 渲染阶段（与预览同一函数）。
    let frames = match render(project, sources_meta, loaded, Some(cancel.as_ref())) {
        Ok(buf) => buf,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            if cancel.is_cancelled() {
                if let Some(d) = db {
                    let _ = d.log_export(&project.id, dest, project.version, "cancelled");
                }
                return Ok(ExportOutcome {
                    path: dest.to_string(),
                    frames: 0,
                    cancelled: true,
                });
            }
            if let Some(d) = db {
                let _ = d.log_export(&project.id, dest, project.version, "failed");
            }
            return Err(e);
        }
    };

    // 写临时文件。
    if let Err(e) = write_wav(&tmp_str, project.sample_rate, project.channels, &frames) {
        let _ = std::fs::remove_file(&tmp_path);
        if let Some(d) = db {
            let _ = d.log_export(&project.id, dest, project.version, "failed");
        }
        return Err(e);
    }

    // 写盘后再次检查取消（取消可能发生在渲染末尾）。
    if cancel.is_cancelled() {
        let _ = std::fs::remove_file(&tmp_path);
        if let Some(d) = db {
            let _ = d.log_export(&project.id, dest, project.version, "cancelled");
        }
        return Ok(ExportOutcome {
            path: dest.to_string(),
            frames: 0,
            cancelled: true,
        });
    }

    // 原子提交：rename 在同文件系统上是原子的，目标只会「不存在/完整」。
    std::fs::rename(&tmp_path, dest_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        anyhow::anyhow!("rename 提交失败: {e}")
    })?;

    if let Some(d) = db {
        d.log_export(&project.id, dest, project.version, "completed")?;
    }
    Ok(ExportOutcome {
        path: dest.to_string(),
        frames: frames.len() / project.channels as usize,
        cancelled: false,
    })
}

/// 分轨导出产物：一条混音 + 每条轨道一个 WAV。
/// 所有文件由同一 `project` 快照（同一 version 修订）渲染，
/// 混音与分轨引用同一工程修订；任一失败整体失败、全部临时文件清理。
pub struct StemOutcome {
    pub mix_path: String,
    /// (track_id, path, frames)
    pub stems: Vec<(String, String, usize)>,
    pub version: i64,
    pub cancelled: bool,
}

/// 执行分轨导出。`dest` 为混音目标；分轨文件写在同目录，
/// 命名为 `<混音名去后缀>__stem_<track_id>.wav`。
pub fn export_stems(
    project: &Project,
    sources_meta: &std::collections::HashMap<String, Source>,
    loaded: &LoadedSources,
    dest: &str,
    cancel: Arc<CancelToken>,
    db: Option<&Db>,
) -> Result<StemOutcome> {
    // 先渲染混音（与单文件导出完全同一路径）。
    let mix = export_wav(project, sources_meta, loaded, dest, cancel.clone(), db)?;
    if mix.cancelled {
        return Ok(StemOutcome {
            mix_path: dest.to_string(),
            stems: vec![],
            version: project.version,
            cancelled: true,
        });
    }

    let dest_path = Path::new(dest);
    let dir = dest_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无效导出路径 {dest}"))?;
    let base = dest_path
        .file_stem()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("无效导出文件名"))?
        .to_string();

    let stems_data = match crate::render::render_stems(
        project,
        sources_meta,
        loaded,
        Some(cancel.as_ref()),
    ) {
        Ok(s) => s,
        Err(e) => {
            if cancel.is_cancelled() {
                if let Some(d) = db {
                    let _ = d.log_export(&project.id, dest, project.version, "cancelled");
                }
                return Ok(StemOutcome {
                    mix_path: dest.to_string(),
                    stems: vec![],
                    version: project.version,
                    cancelled: true,
                });
            }
            if let Some(d) = db {
                let _ = d.log_export(&project.id, dest, project.version, "failed");
            }
            return Err(e);
        }
    };

    let mut written = Vec::new();
    let result = (|| -> Result<Vec<(String, String, usize)>> {
        let mut out = Vec::new();
        for (tid, buf) in &stems_data.tracks {
            if cancel.is_cancelled() {
                anyhow::bail!("render cancelled");
            }
            let stem_path = dir.join(format!("{base}__stem_{tid}.wav"));
            let tmp = dir.join(format!(
                ".{}.stem.tmp.{}",
                stem_path.file_name().unwrap().to_string_lossy(),
                std::process::id()
            ));
            write_wav(
                &tmp.to_string_lossy(),
                project.sample_rate,
                project.channels,
                buf,
            )?;
            std::fs::rename(&tmp, &stem_path)?;
            let frames = buf.len() / project.channels as usize;
            written.push(stem_path.clone());
            out.push((tid.clone(), stem_path.to_string_lossy().to_string(), frames));
        }
        Ok(out)
    })();

    match result {
        Ok(stems) => {
            if let Some(d) = db {
                for (_, path, _) in &stems {
                    d.log_export(&project.id, path, project.version, "completed")?;
                }
            }
            Ok(StemOutcome {
                mix_path: dest.to_string(),
                stems,
                version: project.version,
                cancelled: false,
            })
        }
        Err(e) => {
            // 回滚本次产生的分轨临时/目标文件（混音已原子提交，保留作为修订证据）。
            for p in &written {
                let _ = std::fs::remove_file(p);
            }
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains(".stem.tmp.") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            if let Some(d) = db {
                let _ = d.log_export(&project.id, dest, project.version, "failed");
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::WavData;

    fn loaded_ones(rate: u32, n: usize) -> LoadedSources {
        let mut m = LoadedSources::new();
        m.insert(
            "s".into(),
            Arc::new(WavData {
                sample_rate: rate,
                channels: 1,
                bits: 16,
                is_float: false,
                samples: vec![0.5f32; n],
            }),
        );
        m
    }

    fn meta(rate: u32, n: i64) -> std::collections::HashMap<String, Source> {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "s".into(),
            Source {
                id: "s".into(),
                path: "x.wav".into(),
                original_rate: rate,
                channels: 1,
                frame_count: n,
            },
        );
        m
    }

    use crate::domain::{Item, Project};

    fn proj() -> Project {
        Project {
            id: "p".into(),
            name: "t".into(),
            sample_rate: 48000,
            channels: 1,
            version: 1,
            tracks: vec![],
            groups: vec![],
            items: vec![Item {
                id: "i".into(),
                source_id: "s".into(),
                track_id: crate::domain::TRACK_LEGACY.into(),
                source_start: 0,
                source_end: 4800,
                timeline_start: 0,
                crossfade_out: 0,
            }],
            silences: vec![],
        }
    }

    #[test]
    fn completed_export_leaves_valid_wav() {
        let dir = std::env::temp_dir().join(format!("pe_ok_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.wav").to_string_lossy().to_string();
        let out = export_wav(
            &proj(),
            &meta(48000, 4800),
            &loaded_ones(48000, 4800),
            &dest,
            CancelToken::default().into(),
            None,
        )
        .unwrap();
        assert!(!out.cancelled);
        assert_eq!(out.frames, 4800);
        assert!(std::path::Path::new(&dest).exists());
        let reread = crate::wav::read_wav(&dest).unwrap();
        assert_eq!(reread.frames(), 4800);
        // 没有遗留临时文件。
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("out.wav")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_export_leaves_no_target() {
        let dir = std::env::temp_dir().join(format!("pe_cancel_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("half.wav").to_string_lossy().to_string();
        let token = Arc::new(CancelToken::default());
        token.cancel(); // 渲染前即取消
        let out = export_wav(
            &proj(),
            &meta(48000, 4800),
            &loaded_ones(48000, 4800),
            &dest,
            token,
            None,
        )
        .unwrap();
        assert!(out.cancelled);
        assert!(!std::path::Path::new(&dest).exists(), "取消后不得留下目标文件");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "临时文件也应清理");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
