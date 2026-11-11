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
            items: vec![Item {
                id: "i".into(),
                source_id: "s".into(),
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
