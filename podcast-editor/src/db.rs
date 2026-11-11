//! SQLite 持久化。
//!
//! 工程时间线以「版本快照」形式整体存储：每次编辑产生新行、version 单调递增。
//! 撤销/重做就是在快照之间切换 current_version，不做破坏性原地改写。
//! 重开工程时读取 current_version 指向的快照，得到与关闭前一致的时间线。
//! 所有位置字段都是采样帧整数；毫秒不进入数据库。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::sync::Mutex;

use crate::domain::{Project, Source};

/// 连接以 Mutex 包裹：rusqlite Connection 本身不是 Sync，
/// 而 AppState 必须跨 axum 工作线程共享。锁粒度为单次调用。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// 测试用内存库。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Db {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS projects (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                sample_rate INTEGER NOT NULL,
                channels    INTEGER NOT NULL,
                current_version INTEGER NOT NULL
            );

            -- 版本快照：时间线整体 JSON（items/silences 全部以工程帧整数表达）。
            CREATE TABLE IF NOT EXISTS project_versions (
                project_id  TEXT NOT NULL REFERENCES projects(id),
                version     INTEGER NOT NULL,
                timeline_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (project_id, version)
            );

            -- 原始素材登记（只读引用；path 指向永不被覆盖的原文件）。
            CREATE TABLE IF NOT EXISTS sources (
                id            TEXT PRIMARY KEY,
                project_id    TEXT NOT NULL REFERENCES projects(id),
                path          TEXT NOT NULL,
                original_rate INTEGER NOT NULL,
                channels      INTEGER NOT NULL,
                frame_count   INTEGER NOT NULL
            );

            -- 导出记录：成功提交才落 completed=1；取消/失败不写成功记录。
            CREATE TABLE IF NOT EXISTS exports (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id TEXT NOT NULL REFERENCES projects(id),
                path       TEXT NOT NULL,
                version    INTEGER NOT NULL,
                status     TEXT NOT NULL CHECK (status IN ('completed','cancelled','failed')),
                created_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn create_project(&self, p: &Project) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO projects (id,name,sample_rate,channels,current_version)
             VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![p.id, p.name, p.sample_rate, p.channels, p.version],
        )?;
        let json = serde_json::to_string(&TimelineSnapshot::from(p))?;
        tx.execute(
            "INSERT INTO project_versions (project_id,version,timeline_json,created_at)
             VALUES (?1,?2,?3,?4)",
            rusqlite::params![p.id, p.version, json, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_source(&self, s: &Source, project_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sources (id,project_id,path,original_rate,channels,frame_count)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET
               path=excluded.path, original_rate=excluded.original_rate,
               channels=excluded.channels, frame_count=excluded.frame_count",
            rusqlite::params![
                s.id,
                project_id,
                s.path,
                s.original_rate,
                s.channels,
                s.frame_count
            ],
        )?;
        Ok(())
    }

    pub fn list_sources(&self, project_id: &str) -> Result<Vec<Source>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,path,original_rate,channels,frame_count
             FROM sources WHERE project_id=?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![project_id], |r| {
            Ok(Source {
                id: r.get(0)?,
                path: r.get(1)?,
                original_rate: r.get(2)?,
                channels: r.get(3)?,
                frame_count: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 追加一个新版本快照并把 current_version 指向它（每次编辑调用）。
    pub fn save_version(&self, p: &Project) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let json = serde_json::to_string(&TimelineSnapshot::from(p))
            .context("serialize timeline")?;
        tx.execute(
            "INSERT INTO project_versions (project_id,version,timeline_json,created_at)
             VALUES (?1,?2,?3,?4)",
            rusqlite::params![p.id, p.version, json, now_ms()],
        )?;
        tx.execute(
            "UPDATE projects SET current_version=?2 WHERE id=?1",
            rusqlite::params![p.id, p.version],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 撤销/重做：把 current_version 切到已存在的快照版本。
    pub fn restore_version(&self, project_id: &str, version: i64) -> Result<Project> {
        {
            let conn = self.conn.lock().unwrap();
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM project_versions WHERE project_id=?1 AND version=?2",
                rusqlite::params![project_id, version],
                |r| r.get(0),
            )?;
            if exists == 0 {
                anyhow::bail!("version {version} 不存在，无法恢复");
            }
            conn.execute(
                "UPDATE projects SET current_version=?2 WHERE id=?1",
                rusqlite::params![project_id, version],
            )?;
        } // 锁在此释放，load_project 再独立取锁，避免同线程重入。
        self.load_project(project_id)
    }

    pub fn list_versions(&self, project_id: &str) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT version FROM project_versions WHERE project_id=?1 ORDER BY version")?;
        let rows = stmt.query_map(rusqlite::params![project_id], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 重开工程：读项目元数据 + current_version 快照，重建完整 Project。
    /// 旧单轨工程（无 tracks/groups 字段）在此归一化：补 legacy 轨道并把
    /// 无 track_id 的片段挂上去，素材与源文件完全不转换。
    pub fn load_project(&self, project_id: &str) -> Result<Project> {
        let conn = self.conn.lock().unwrap();
        let (name, rate, ch, cur): (String, u32, u16, i64) = conn.query_row(
            "SELECT name,sample_rate,channels,current_version FROM projects WHERE id=?1",
            rusqlite::params![project_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let json: String = conn.query_row(
            "SELECT timeline_json FROM project_versions WHERE project_id=?1 AND version=?2",
            rusqlite::params![project_id, cur],
            |r| r.get(0),
        )?;
        let snap: TimelineSnapshot = serde_json::from_str(&json).context("parse timeline")?;
        let mut p = Project {
            id: project_id.to_string(),
            name,
            sample_rate: rate,
            channels: ch,
            version: cur,
            tracks: snap.tracks,
            groups: snap.groups,
            items: snap.items,
            silences: snap.silences,
        };
        p.normalize_legacy();
        Ok(p)
    }

    pub fn list_project_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM projects")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn log_export(
        &self,
        project_id: &str,
        path: &str,
        version: i64,
        status: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO exports (project_id,path,version,status,created_at)
             VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![project_id, path, version, status, now_ms()],
        )?;
        Ok(())
    }

    /// 测试辅助：注入一个触发器，让新版本快照写入必然失败，
    /// 用于验证「批量移动的原子保存」在保存中断时不留半提交。
    /// SQLite 的 DDL 触发器本身随连接存活，drop 连接即消失。
    pub fn inject_save_failure(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TRIGGER IF NOT EXISTS trg_fail_version_insert
            BEFORE INSERT ON project_versions
            BEGIN
                SELECT RAISE(ABORT, 'injected save interruption');
            END;
            "#,
        )?;
        Ok(())
    }

    /// 测试辅助：以旧版快照格式（无 tracks/groups、item 无 track_id）建工程，
    /// 验证旧单轨工程无需转换素材即可打开。
    pub fn create_project_raw_legacy(
        &self,
        id: &str,
        sample_rate: u32,
        channels: u16,
        timeline_json: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO projects (id,name,sample_rate,channels,current_version)
             VALUES (?1,?2,?3,?4,1)",
            rusqlite::params![id, "legacy", sample_rate, channels],
        )?;
        tx.execute(
            "INSERT INTO project_versions (project_id,version,timeline_json,created_at)
             VALUES (?1,1,?2,?3)",
            rusqlite::params![id, timeline_json, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct TimelineSnapshot {
    #[serde(default)]
    tracks: Vec<crate::domain::Track>,
    #[serde(default)]
    groups: Vec<crate::domain::EditGroup>,
    #[serde(default)]
    items: Vec<crate::domain::Item>,
    #[serde(default)]
    silences: Vec<crate::domain::SilenceItem>,
}
impl From<&Project> for TimelineSnapshot {
    fn from(p: &Project) -> Self {
        TimelineSnapshot {
            tracks: p.tracks.clone(),
            groups: p.groups.clone(),
            items: p.items.clone(),
            silences: p.silences.clone(),
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Item, SilenceItem};

    fn sample_project(id: &str, version: i64) -> Project {
        Project {
            id: id.into(),
            name: "t".into(),
            sample_rate: 48000,
            channels: 1,
            version,
            tracks: Project::default_tracks(),
            groups: vec![],
            items: vec![Item {
                id: "i".into(),
                source_id: "s".into(),
                track_id: crate::domain::TRACK_LEGACY.into(),
                source_start: 0,
                source_end: 100,
                timeline_start: 0,
                crossfade_out: 0,
            }],
            silences: vec![SilenceItem {
                id: "z".into(),
                timeline_start: 100,
                length: 50,
            }],
        }
    }

    #[test]
    fn reopen_reproduces_same_timeline() {
        let dir = std::env::temp_dir().join(format!("pe_test_{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let db = Db::open(dir.to_str().unwrap()).unwrap();
        let p0 = sample_project("p", 1);
        db.create_project(&p0).unwrap();
        let mut p1 = p0.clone();
        p1.version = 2;
        p1.items[0].source_end = 80; // 编辑只改引用范围
        db.save_version(&p1).unwrap();

        // 模拟关闭重开：重新打开数据库文件。
        drop(db);
        let db2 = Db::open(dir.to_str().unwrap()).unwrap();
        let reopened = db2.load_project("p").unwrap();
        assert_eq!(reopened.version, 2);
        assert_eq!(reopened, p1);

        // 撤销回版本 1 后再重开，仍是撤销后的状态。
        let restored = db2.restore_version("p", 1).unwrap();
        assert_eq!(restored, p0);
        drop(db2);
        let db3 = Db::open(dir.to_str().unwrap()).unwrap();
        assert_eq!(db3.load_project("p").unwrap(), p0);
        let _ = std::fs::remove_file(&dir);
    }
}
