//! Attachment, video-job and file-annotation persistence.
//!
//! Row-level queries live on [`Db`] (like the other `store_*` modules).
//! [`AttachmentStore`] adds the filesystem side: bytes are stored under
//! `attachments/{conversation_id}/{attachment_id}.{ext}` where both path
//! segments are server-generated UUIDs and the extension is derived from a
//! validated media type — never from user input. All reads are confined to
//! the storage root.

use crate::db::{now, Db};
use crate::error::{CoreError, Result};
use std::path::{Component, Path, PathBuf};
use vortex_types::{
    AttachmentInfo, AttachmentKind, AttachmentOrigin, AttachmentRef, AttachmentStatus,
    VideoJobInfo, VideoJobStatus,
};

/// Full attachment row, including the server-side storage location.
#[derive(Debug, Clone)]
pub struct AttachmentRecord {
    pub id: String,
    pub conversation_id: String,
    pub kind: AttachmentKind,
    pub origin: AttachmentOrigin,
    pub status: AttachmentStatus,
    pub filename: String,
    pub media_type: String,
    pub size_bytes: u64,
    /// Relative path under the storage root (`{conv}/{id}.{ext}`); empty
    /// while a generated attachment is still pending.
    pub storage_path: String,
    pub job_id: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

impl AttachmentRecord {
    pub fn info(&self) -> AttachmentInfo {
        AttachmentInfo {
            id: self.id.clone(),
            conversation_id: self.conversation_id.clone(),
            kind: self.kind,
            origin: self.origin,
            status: self.status,
            filename: self.filename.clone(),
            media_type: self.media_type.clone(),
            size_bytes: self.size_bytes,
            job_id: self.job_id.clone(),
            error: self.error.clone(),
            created_at: self.created_at.clone(),
        }
    }

    pub fn attachment_ref(&self) -> AttachmentRef {
        AttachmentRef {
            id: self.id.clone(),
            kind: self.kind,
            filename: self.filename.clone(),
            media_type: self.media_type.clone(),
            status: self.status,
        }
    }
}

/// Full video-job row.
#[derive(Debug, Clone)]
pub struct VideoJobRecord {
    pub id: String,
    pub provider_job_id: Option<String>,
    pub conversation_id: String,
    pub attachment_id: String,
    pub model: String,
    pub status: VideoJobStatus,
    pub prompt: String,
    pub params_json: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl VideoJobRecord {
    pub fn info(&self) -> VideoJobInfo {
        VideoJobInfo {
            id: self.id.clone(),
            conversation_id: self.conversation_id.clone(),
            status: self.status,
            model: self.model.clone(),
            prompt: self.prompt.clone(),
            attachment_id: self.attachment_id.clone(),
            error: self.error.clone(),
            created_at: self.created_at.clone(),
        }
    }
}

fn parse_kind(s: &str) -> AttachmentKind {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or_default()
}
fn parse_origin(s: &str) -> AttachmentOrigin {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(AttachmentOrigin::Upload)
}
fn parse_status(s: &str) -> AttachmentStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or_default()
}
fn parse_job_status(s: &str) -> VideoJobStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(VideoJobStatus::Failed)
}

/// Derive a safe storage extension from a validated media type. The fallback
/// is `txt` (only allowlisted text types reach storage); user-supplied
/// extensions are never used.
pub fn ext_for_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/mpeg" => "mpeg",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "application/yaml" | "text/yaml" | "application/x-yaml" | "text/x-yaml" => "yaml",
        "application/toml" | "text/toml" => "toml",
        _ => "txt",
    }
}

/// Sanitize a user-supplied filename for display only: strip any path
/// components and control characters, bound the length. Never used for
/// storage paths.
pub fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("file")
        .trim()
        .to_string();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().to_string();
    let out: String = cleaned.chars().take(200).collect();
    if out.is_empty() {
        "file".to_string()
    } else {
        out
    }
}

fn row_to_attachment(r: &rusqlite::Row<'_>) -> rusqlite::Result<AttachmentRecord> {
    Ok(AttachmentRecord {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        kind: parse_kind(&r.get::<_, String>(2)?),
        origin: parse_origin(&r.get::<_, String>(3)?),
        status: parse_status(&r.get::<_, String>(4)?),
        filename: r.get(5)?,
        media_type: r.get(6)?,
        size_bytes: r.get::<_, i64>(7)?.max(0) as u64,
        storage_path: r.get(8)?,
        job_id: r.get(9)?,
        error: r.get(10)?,
        created_at: r.get(11)?,
    })
}

const ATTACHMENT_COLS: &str = "id, conversation_id, kind, origin, status, filename, media_type, \
     size_bytes, storage_path, job_id, error, created_at";

fn row_to_video_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<VideoJobRecord> {
    Ok(VideoJobRecord {
        id: r.get(0)?,
        provider_job_id: r.get(1)?,
        conversation_id: r.get(2)?,
        attachment_id: r.get(3)?,
        model: r.get(4)?,
        status: parse_job_status(&r.get::<_, String>(5)?),
        prompt: r.get(6)?,
        params_json: r.get(7)?,
        error: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

const VIDEO_JOB_COLS: &str = "id, provider_job_id, conversation_id, attachment_id, model, status, \
     prompt, params_json, error, created_at, updated_at";

impl Db {
    // ---- attachments ------------------------------------------------------

    pub async fn insert_attachment(&self, rec: AttachmentRecord) -> Result<()> {
        self.with_conn(move |c| {
            let inserted = c.execute(
                "INSERT INTO attachments
                     (id, conversation_id, kind, origin, status, filename, media_type,
                      size_bytes, storage_path, job_id, error, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
                 WHERE EXISTS (SELECT 1 FROM conversations WHERE id = ?2)",
                rusqlite::params![
                    rec.id,
                    rec.conversation_id,
                    rec.kind.as_str(),
                    rec.origin.as_str(),
                    rec.status.as_str(),
                    rec.filename,
                    rec.media_type,
                    rec.size_bytes as i64,
                    rec.storage_path,
                    rec.job_id,
                    rec.error,
                    rec.created_at,
                ],
            )?;
            if inserted == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
    }

    pub async fn get_attachment(&self, id: String) -> Result<Option<AttachmentRecord>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {ATTACHMENT_COLS} FROM attachments WHERE id = ?1"
            ))?;
            let row = st
                .query_map(rusqlite::params![id], row_to_attachment)?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    pub async fn list_attachments(&self, conversation_id: String) -> Result<Vec<AttachmentRecord>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {ATTACHMENT_COLS} FROM attachments
                 WHERE conversation_id = ?1 ORDER BY created_at, id"
            ))?;
            let rows = st
                .query_map(rusqlite::params![conversation_id], row_to_attachment)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn set_attachment_status(
        &self,
        id: String,
        status: AttachmentStatus,
        error: Option<String>,
    ) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE attachments SET status = ?2, error = ?3 WHERE id = ?1",
                rusqlite::params![id, status.as_str(), error],
            )? > 0)
        })
        .await
    }

    /// Mark an attachment ready with its stored file location and size.
    pub async fn set_attachment_ready(
        &self,
        id: String,
        storage_path: String,
        size_bytes: u64,
    ) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE attachments
                 SET status = 'ready', error = NULL, storage_path = ?2, size_bytes = ?3
                 WHERE id = ?1",
                rusqlite::params![id, storage_path, size_bytes as i64],
            )? > 0)
        })
        .await
    }

    /// Delete the row and return it (so the caller can remove the file).
    pub async fn delete_attachment(&self, id: String) -> Result<Option<AttachmentRecord>> {
        let rec = self.get_attachment(id.clone()).await?;
        if rec.is_some() {
            self.with_conn(move |c| {
                let tx = c.unchecked_transaction()?;
                tx.execute(
                    "DELETE FROM file_annotations WHERE attachment_id = ?1",
                    rusqlite::params![id],
                )?;
                tx.execute(
                    "DELETE FROM attachments WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        }
        Ok(rec)
    }

    // ---- video jobs -------------------------------------------------------

    pub async fn insert_video_job(&self, job: VideoJobRecord) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO video_jobs
                     (id, provider_job_id, conversation_id, attachment_id, model, status,
                      prompt, params_json, error, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    job.id,
                    job.provider_job_id,
                    job.conversation_id,
                    job.attachment_id,
                    job.model,
                    job.status.as_str(),
                    job.prompt,
                    job.params_json,
                    job.error,
                    job.created_at,
                    job.updated_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_video_job(&self, id: String) -> Result<Option<VideoJobRecord>> {
        self.with_conn(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {VIDEO_JOB_COLS} FROM video_jobs WHERE id = ?1"
            ))?;
            let row = st
                .query_map(rusqlite::params![id], row_to_video_job)?
                .next()
                .transpose()?;
            Ok(row)
        })
        .await
    }

    pub async fn list_video_jobs_by_status(
        &self,
        statuses: Vec<VideoJobStatus>,
    ) -> Result<Vec<VideoJobRecord>> {
        self.with_conn(move |c| {
            let mut out = Vec::new();
            for status in statuses {
                let mut st = c.prepare(&format!(
                    "SELECT {VIDEO_JOB_COLS} FROM video_jobs WHERE status = ?1 ORDER BY created_at"
                ))?;
                let rows = st.query_map(rusqlite::params![status.as_str()], row_to_video_job)?;
                for row in rows {
                    out.push(row?);
                }
            }
            Ok(out)
        })
        .await
    }

    pub async fn update_video_job(
        &self,
        id: String,
        status: VideoJobStatus,
        provider_job_id: Option<String>,
        error: Option<String>,
    ) -> Result<bool> {
        self.with_conn(move |c| {
            Ok(c.execute(
                "UPDATE video_jobs
                 SET status = ?2,
                     provider_job_id = COALESCE(?3, provider_job_id),
                     error = ?4,
                     updated_at = ?5
                 WHERE id = ?1",
                rusqlite::params![id, status.as_str(), provider_job_id, error, now()],
            )? > 0)
        })
        .await
    }

    // ---- file annotations -------------------------------------------------

    pub async fn get_file_annotations(
        &self,
        conversation_id: String,
        attachment_id: String,
        max_bytes: usize,
    ) -> Result<Option<serde_json::Value>> {
        self.with_conn(move |c| {
            let row: Option<String> = c
                .query_row(
                    "SELECT f.annotations_json FROM file_annotations f
                     JOIN attachments a ON a.id = f.attachment_id
                     WHERE a.id = ?1 AND a.conversation_id = ?2
                     AND a.kind = 'pdf' AND a.media_type = 'application/pdf' AND a.status = 'ready'
                     AND length(CAST(f.annotations_json AS BLOB)) <= ?3",
                    rusqlite::params![attachment_id, conversation_id, max_bytes],
                    |r| r.get(0),
                )
                .map(Some)
                .or_else(|e| {
                    if e == rusqlite::Error::QueryReturnedNoRows {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                })?;
            Ok(row.and_then(|s| serde_json::from_str(&s).ok()))
        })
        .await
    }

    pub async fn save_file_annotations(
        &self,
        conversation_id: String,
        attachment_id: String,
        assistant_id: i64,
        annotations: serde_json::Value,
    ) -> Result<bool> {
        let json = serde_json::to_string(&annotations)
            .map_err(|e| CoreError::Config(format!("serializing annotations: {e}")))?;
        self.with_conn(move |c| {
            // Check both rows in the same statement as the upsert so a
            // response arriving after deletion cannot resurrect the cache.
            Ok(c.execute(
                "INSERT INTO file_annotations (attachment_id, annotations_json, created_at)
                 SELECT a.id, ?4, ?5 FROM attachments a
                 WHERE a.id = ?1 AND a.conversation_id = ?2
                 AND a.kind = 'pdf' AND a.media_type = 'application/pdf' AND a.status = 'ready'
                 AND EXISTS (SELECT 1 FROM messages WHERE id = ?3 AND conversation_id = ?2 AND role = 'assistant')
                 ON CONFLICT(attachment_id) DO UPDATE SET annotations_json = ?4, created_at = ?5",
                rusqlite::params![attachment_id, conversation_id, assistant_id, json, now()],
            )? != 0)
        })
        .await
    }
}

/// Filesystem-backed attachment storage. Cloneable shared handle.
#[derive(Clone)]
pub struct AttachmentStore {
    root: PathBuf,
    db: Db,
}

impl AttachmentStore {
    pub fn new(root: PathBuf, db: Db) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        db.set_attachments_root(root.clone());
        Ok(Self { root, db })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Build a storage path from validated UUID segments + a safe extension.
    fn path_for(root: &Path, conversation_id: &str, id: &str, ext: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(conversation_id)
            .map_err(|_| CoreError::Config("invalid conversation id for storage".into()))?;
        uuid::Uuid::parse_str(id)
            .map_err(|_| CoreError::Config("invalid attachment id for storage".into()))?;
        if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(CoreError::Config("invalid storage extension".into()));
        }
        Ok(root.join(conversation_id).join(format!("{id}.{ext}")))
    }

    /// Resolve a stored relative path, refusing anything that escapes the
    /// root (path traversal, absolute paths, non-normal components).
    fn confined_path(&self, rel: &str) -> Result<PathBuf> {
        if rel.is_empty() {
            return Err(CoreError::Config("empty storage path".into()));
        }
        let p = Path::new(rel);
        let mut count = 0;
        for comp in p.components() {
            match comp {
                Component::Normal(_) => count += 1,
                _ => {
                    return Err(CoreError::Config(
                        "attachment storage path must be relative with normal components".into(),
                    ))
                }
            }
        }
        if count != 2 {
            return Err(CoreError::Config(
                "attachment storage path must have exactly two segments".into(),
            ));
        }
        let full = self.root.join(p);
        let canon_full = full.canonicalize()?;
        if canon_full != full {
            return Err(CoreError::Config(
                "attachment path must not resolve through a symlink".into(),
            ));
        }
        Ok(canon_full)
    }

    async fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let parent = path.parent().expect("attachment path has a parent");
        tokio::fs::create_dir_all(parent).await?;
        if tokio::fs::canonicalize(parent).await? != parent {
            return Err(CoreError::Config("invalid attachment directory".into()));
        }
        // Publish only a complete file. A retry may replace bytes written by
        // a process that stopped before marking a pending attachment ready.
        let tmp = parent.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp).await?;
        let result = async {
            file.write_all(bytes).await?;
            file.flush().await?;
            drop(file);
            tokio::fs::rename(&tmp, path).await
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error.into());
        }
        Ok(())
    }

    /// Store uploaded bytes as a ready attachment.
    pub async fn save_bytes(
        &self,
        conversation_id: &str,
        kind: AttachmentKind,
        origin: AttachmentOrigin,
        filename: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<AttachmentInfo> {
        let id = uuid::Uuid::new_v4().to_string();
        let ext = ext_for_media_type(media_type);
        let path = Self::path_for(&self.root, conversation_id, &id, ext)?;
        self.write_bytes(&path, bytes).await?;
        let rec = AttachmentRecord {
            id: id.clone(),
            conversation_id: conversation_id.to_string(),
            kind,
            origin,
            status: AttachmentStatus::Ready,
            filename: sanitize_filename(filename),
            media_type: media_type.to_string(),
            size_bytes: bytes.len() as u64,
            storage_path: format!("{conversation_id}/{id}.{ext}"),
            job_id: None,
            error: None,
            created_at: now(),
        };
        if let Err(error) = self.db.insert_attachment(rec.clone()).await {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(error);
        }
        Ok(rec.info())
    }

    /// Create a pending attachment (generated video) with no file yet.
    pub async fn create_pending(
        &self,
        conversation_id: &str,
        kind: AttachmentKind,
        origin: AttachmentOrigin,
        filename: &str,
        media_type: &str,
        job_id: Option<String>,
    ) -> Result<AttachmentInfo> {
        uuid::Uuid::parse_str(conversation_id)
            .map_err(|_| CoreError::Config("invalid conversation id for storage".into()))?;
        let rec = AttachmentRecord {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: conversation_id.to_string(),
            kind,
            origin,
            status: AttachmentStatus::Pending,
            filename: sanitize_filename(filename),
            media_type: media_type.to_string(),
            size_bytes: 0,
            storage_path: String::new(),
            job_id,
            error: None,
            created_at: now(),
        };
        self.db.insert_attachment(rec.clone()).await?;
        Ok(rec.info())
    }

    /// Write bytes for an existing (pending) attachment and mark it ready.
    pub async fn complete_with_bytes(&self, id: &str, bytes: &[u8]) -> Result<bool> {
        let Some(rec) = self.db.get_attachment(id.to_string()).await? else {
            return Ok(false);
        };
        let ext = ext_for_media_type(&rec.media_type);
        let path = Self::path_for(&self.root, &rec.conversation_id, &rec.id, ext)?;
        self.write_bytes(&path, bytes).await?;
        let rel = format!("{}/{}.{}", rec.conversation_id, rec.id, ext);
        self.db
            .set_attachment_ready(id.to_string(), rel, bytes.len() as u64)
            .await
    }

    /// Read stored bytes; `None` when missing, not ready, or the file is gone.
    pub async fn read_bytes(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let Some(rec) = self.db.get_attachment(id.to_string()).await? else {
            return Ok(None);
        };
        if rec.status != AttachmentStatus::Ready || rec.storage_path.is_empty() {
            return Ok(None);
        }
        let path = match self.confined_path(&rec.storage_path) {
            Ok(p) => p,
            Err(CoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                tracing::warn!("refusing invalid attachment storage path");
                return Err(e);
            }
        };
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get_meta(&self, id: &str) -> Result<Option<AttachmentInfo>> {
        Ok(self
            .db
            .get_attachment(id.to_string())
            .await?
            .map(|r| r.info()))
    }

    /// Model reads are scoped and size-limited. Recheck ownership and the
    /// row's expected size at the point of the read, not just in preflight.
    pub async fn read_model_bytes(
        &self,
        meta: &AttachmentInfo,
        limit: usize,
    ) -> Result<Option<Vec<u8>>> {
        use tokio::io::AsyncReadExt;

        let Some(rec) = self.db.get_attachment(meta.id.clone()).await? else {
            return Ok(None);
        };
        let ext = ext_for_media_type(&meta.media_type);
        if rec.conversation_id != meta.conversation_id
            || rec.status != AttachmentStatus::Ready
            || rec.size_bytes != meta.size_bytes
            || rec.kind != meta.kind
            || rec.media_type != meta.media_type
            || rec.storage_path != format!("{}/{}.{ext}", meta.conversation_id, meta.id)
        {
            return Ok(None);
        }
        let path = Self::path_for(&self.root, &rec.conversation_id, &rec.id, ext)?;
        if tokio::fs::canonicalize(&path).await? != path {
            return Ok(None);
        }
        let metadata = tokio::fs::metadata(&path).await?;
        if !metadata.is_file() || metadata.len() != rec.size_bytes {
            return Ok(None);
        }
        let file = tokio::fs::File::open(path).await?;
        let expected = (limit as u64).min(rec.size_bytes);
        let mut bytes = Vec::new();
        // Read one extra byte only for full reads, to detect file growth.
        file.take(expected + u64::from(expected == rec.size_bytes))
            .read_to_end(&mut bytes)
            .await?;
        Ok((bytes.len() as u64 == expected).then_some(bytes))
    }

    pub async fn list_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AttachmentInfo>> {
        Ok(self
            .db
            .list_attachments(conversation_id.to_string())
            .await?
            .into_iter()
            .map(|r| r.info())
            .collect())
    }

    /// Delete one attachment's row, annotations and file.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let Some(rec) = self.db.delete_attachment(id.to_string()).await? else {
            return Ok(false);
        };
        if !rec.storage_path.is_empty() {
            if let Ok(path) = self.confined_path(&rec.storage_path) {
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!("could not remove attachment file");
                    }
                }
            }
        }
        Ok(true)
    }

    /// Best-effort removal of every file for a conversation (used after the
    /// row cascade commits). The directory name is a validated UUID.
    pub async fn delete_files_for_conversation(&self, conversation_id: &str) -> Result<()> {
        uuid::Uuid::parse_str(conversation_id)
            .map_err(|_| CoreError::Config("invalid conversation id for storage".into()))?;
        let dir = self.root.join(conversation_id);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(
                    conversation_id = %conversation_id,
                    error = %e,
                    "could not remove attachment directory"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_types::Mode;

    fn store() -> (AttachmentStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let root = dir.path().join("attachments");
        (AttachmentStore::new(root, db).unwrap(), dir)
    }

    async fn conversation(s: &AttachmentStore) -> String {
        s.db()
            .create_conversation("c", Mode::Chat, None)
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn save_read_list_delete_roundtrip() {
        let (s, _dir) = store();
        let conv = conversation(&s).await;
        let info = s
            .save_bytes(
                &conv,
                AttachmentKind::Image,
                AttachmentOrigin::Upload,
                "cat.png",
                "image/png",
                b"\x89PNG\r\n\x1a\nfake",
            )
            .await
            .unwrap();
        assert_eq!(info.status, AttachmentStatus::Ready);
        assert_eq!(info.size_bytes, 12);
        let rec = s
            .db()
            .get_attachment(info.id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(rec.storage_path.ends_with(".png"));
        assert!(rec.storage_path.starts_with(&conv));
        let bytes = s.read_bytes(&info.id).await.unwrap().unwrap();
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\nfake");
        let list = s.list_for_conversation(&conv).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, info.id);
        assert!(s.delete(&info.id).await.unwrap());
        assert!(s.read_bytes(&info.id).await.unwrap().is_none());
        assert!(s.list_for_conversation(&conv).await.unwrap().is_empty());
        assert!(!s.delete(&info.id).await.unwrap());
    }

    #[tokio::test]
    async fn pending_then_complete_lifecycle() {
        let (s, _dir) = store();
        let conv = conversation(&s).await;
        let info = s
            .create_pending(
                &conv,
                AttachmentKind::Video,
                AttachmentOrigin::Generated,
                "clip.mp4",
                "video/mp4",
                Some("job-1".into()),
            )
            .await
            .unwrap();
        assert_eq!(info.status, AttachmentStatus::Pending);
        assert!(s.read_bytes(&info.id).await.unwrap().is_none());
        assert!(s.complete_with_bytes(&info.id, b"mp4bytes").await.unwrap());
        let meta = s.get_meta(&info.id).await.unwrap().unwrap();
        assert_eq!(meta.status, AttachmentStatus::Ready);
        assert_eq!(meta.size_bytes, 8);
        assert_eq!(s.read_bytes(&info.id).await.unwrap().unwrap(), b"mp4bytes");
    }

    #[tokio::test]
    async fn path_confinement_rejects_traversal() {
        let (s, _dir) = store();
        let conv = conversation(&s).await;
        let info = s
            .save_bytes(
                &conv,
                AttachmentKind::Text,
                AttachmentOrigin::Upload,
                "a.txt",
                "text/plain",
                b"hello",
            )
            .await
            .unwrap();
        // Tamper with the stored path to attempt an escape.
        s.db()
            .with_conn({
                let id = info.id.clone();
                move |c| {
                    c.execute(
                        "UPDATE attachments SET storage_path = ?2 WHERE id = ?1",
                        rusqlite::params![id, "../../etc/passwd"],
                    )?;
                    Ok(())
                }
            })
            .await
            .unwrap();
        assert!(s.read_bytes(&info.id).await.is_err());
        // Non-UUID path segments are refused at construction.
        assert!(AttachmentStore::path_for(s.root(), "not-a-uuid", &info.id, "png").is_err());
        assert!(AttachmentStore::path_for(s.root(), &conv, "not-a-uuid", "png").is_err());
        assert!(AttachmentStore::path_for(s.root(), &conv, &info.id, "../x").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn save_refuses_symlinked_conversation_directory() {
        let (s, dir) = store();
        let conv = conversation(&s).await;
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, s.root().join(&conv)).unwrap();

        let result = s
            .save_bytes(
                &conv,
                AttachmentKind::Text,
                AttachmentOrigin::Upload,
                "notes.txt",
                "text/plain",
                b"notes",
            )
            .await;
        assert!(result.is_err());
        assert!(outside.read_dir().unwrap().next().is_none());
        assert!(s.list_for_conversation(&conv).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn conversation_delete_cascades_attachment_rows_and_files() {
        let (s, _dir) = store();
        let conv = conversation(&s).await;
        let info = s
            .save_bytes(
                &conv,
                AttachmentKind::Pdf,
                AttachmentOrigin::Upload,
                "x.pdf",
                "application/pdf",
                b"data",
            )
            .await
            .unwrap();
        let assistant = s
            .db()
            .add_message(conv.clone(), vortex_types::ChatMessage::assistant("reply"))
            .await
            .unwrap();
        let annotations = serde_json::json!({"parser":"cloudflare-ai","assistant_id":assistant,"annotation":{"type":"file"}});
        assert!(s
            .db()
            .save_file_annotations(
                conv.clone(),
                info.id.clone(),
                assistant,
                annotations.clone()
            )
            .await
            .unwrap());
        assert!(s
            .db()
            .get_file_annotations(conv.clone(), info.id.clone(), 1024)
            .await
            .unwrap()
            .is_some());
        let file = s.root().join(&conv).join(format!("{}.pdf", info.id));
        assert!(file.exists());
        assert!(s.db().delete_conversation(conv.clone()).await.unwrap());
        assert!(s
            .db()
            .get_attachment(info.id.clone())
            .await
            .unwrap()
            .is_none());
        assert!(s
            .db()
            .get_file_annotations(conv.clone(), info.id.clone(), 1024)
            .await
            .unwrap()
            .is_none());
        assert!(!s
            .db()
            .save_file_annotations(conv.clone(), info.id, assistant, annotations)
            .await
            .unwrap());
        assert!(!file.exists(), "files removed after commit");
        assert!(!s.root().join(&conv).exists());
    }

    #[tokio::test]
    async fn video_job_crud_and_status_filter() {
        let (s, _dir) = store();
        let conv = conversation(&s).await;
        let att = s
            .create_pending(
                &conv,
                AttachmentKind::Video,
                AttachmentOrigin::Generated,
                "v.mp4",
                "video/mp4",
                None,
            )
            .await
            .unwrap();
        let job = VideoJobRecord {
            id: uuid::Uuid::new_v4().to_string(),
            provider_job_id: None,
            conversation_id: conv.clone(),
            attachment_id: att.id.clone(),
            model: "google/veo-3.1-lite".into(),
            status: VideoJobStatus::Submitted,
            prompt: "a cat".into(),
            params_json: "{}".into(),
            error: None,
            created_at: now(),
            updated_at: now(),
        };
        s.db().insert_video_job(job.clone()).await.unwrap();
        let live = s
            .db()
            .list_video_jobs_by_status(vec![VideoJobStatus::Submitted, VideoJobStatus::InProgress])
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        s.db()
            .update_video_job(
                job.id.clone(),
                VideoJobStatus::Completed,
                Some("provider-123".into()),
                None,
            )
            .await
            .unwrap();
        let got = s.db().get_video_job(job.id.clone()).await.unwrap().unwrap();
        assert_eq!(got.status, VideoJobStatus::Completed);
        assert_eq!(got.provider_job_id.as_deref(), Some("provider-123"));
        assert!(s
            .db()
            .list_video_jobs_by_status(vec![VideoJobStatus::Submitted])
            .await
            .unwrap()
            .is_empty());
        // Conversation delete removes the job too.
        s.db().delete_conversation(conv).await.unwrap();
        assert!(s.db().get_video_job(job.id).await.unwrap().is_none());
    }

    #[test]
    fn filename_sanitization_strips_paths_and_bounds_length() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\fake\\a.png"), "a.png");
        assert_eq!(sanitize_filename("  "), "file");
        assert_eq!(sanitize_filename(""), "file");
        let long = "a".repeat(500);
        assert_eq!(sanitize_filename(&long).chars().count(), 200);
        assert_eq!(sanitize_filename("a\u{0}b.txt"), "ab.txt");
    }

    #[test]
    fn extension_derives_from_media_type_only() {
        assert_eq!(ext_for_media_type("image/png"), "png");
        assert_eq!(ext_for_media_type("image/jpeg"), "jpg");
        assert_eq!(ext_for_media_type("application/pdf"), "pdf");
        assert_eq!(ext_for_media_type("video/mp4"), "mp4");
        assert_eq!(ext_for_media_type("text/plain"), "txt");
        assert_eq!(ext_for_media_type("application/evil"), "txt");
    }
}
