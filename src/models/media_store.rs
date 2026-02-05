//! SQLite-based persistent cache for media metadata and layout information.
//!
//! This module provides the `MediaStore` struct which manages all database operations
//! for the idxd media browser, including:
//! - Media item metadata (path, dimensions, mtime, thumbnail info)
//! - Layout cache (row breaks and heights for different viewport widths)

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use tracing::{debug, info, warn};

use crate::models::{MediaItem, MediaType};

/// SQLite-backed storage for media metadata and layout cache.
///
/// The database is stored at `XDG_CONFIG_HOME/idxd/cache.sqlite` and uses
/// WAL mode for concurrent read/write performance.
pub struct MediaStore {
    conn: Connection,
}

/// Metadata about a cached layout for a specific viewport width and sort order.
#[derive(Debug, Clone)]
pub struct LayoutMeta {
    pub width_bucket: i32,
    pub sort_key: String,
    pub item_count: i32,
    pub list_hash: String,
    pub updated_at: i64,
}

/// A single cached row in the layout.
#[derive(Debug, Clone)]
pub struct LayoutRow {
    pub width_bucket: i32,
    pub sort_key: String,
    pub row_index: i32,
    pub row_height: f64,
    pub start_index: i32,
    pub end_index: i32,
}

/// Information needed to check if a cached media item is still valid.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: i64,
}

impl MediaStore {
    /// Opens or creates the database at the default XDG location.
    ///
    /// The database will be created at `XDG_CONFIG_HOME/idxd/cache.sqlite`.
    /// All necessary tables are created if they don't exist.
    pub fn open_default() -> Result<Self> {
        let db_path = Self::default_db_path()?;
        Self::open(&db_path)
    }

    /// Returns the default database path based on XDG directories.
    pub fn default_db_path() -> Result<PathBuf> {
        let proj_dirs =
            ProjectDirs::from("", "", "idxd").context("Failed to determine project directories")?;

        let config_dir = proj_dirs.config_dir();
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("Failed to create config directory: {:?}", config_dir))?;

        Ok(config_dir.join("cache.sqlite"))
    }

    /// Opens or creates the database at the specified path.
    ///
    /// Configures SQLite for optimal performance:
    /// - journal_mode = WAL (write-ahead logging for concurrent access)
    /// - synchronous = NORMAL (balance between safety and speed)
    /// - temp_store = MEMORY (keep temp tables in RAM)
    /// - cache_size = -64000 (64MB page cache)
    pub fn open(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory: {:?}", parent))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open database at {:?}", path))?;

        // Configure SQLite for performance
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            PRAGMA cache_size = -64000;
            PRAGMA mmap_size = 268435456;
            PRAGMA foreign_keys = ON;
            ",
        )
        .context("Failed to configure SQLite pragmas")?;

        let mut store = Self { conn };
        store.create_tables()?;

        info!("Opened media store at {:?}", path);
        Ok(store)
    }

    /// Creates the database schema if it doesn't exist.
    fn create_tables(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            -- Media items table
            CREATE TABLE IF NOT EXISTS media (
                path TEXT PRIMARY KEY NOT NULL,
                media_type INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                duration_ms INTEGER,
                thumb_path TEXT,
                thumb_w INTEGER,
                thumb_h INTEGER,
                last_seen INTEGER NOT NULL
            );

            -- Index for scanning/cleanup operations
            CREATE INDEX IF NOT EXISTS idx_media_last_seen ON media(last_seen);
            CREATE INDEX IF NOT EXISTS idx_media_mtime ON media(mtime);

            -- Layout metadata table (tracks validity of cached layouts)
            CREATE TABLE IF NOT EXISTS layout_meta (
                width_bucket INTEGER NOT NULL,
                sort_key TEXT NOT NULL,
                item_count INTEGER NOT NULL,
                list_hash TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (width_bucket, sort_key)
            );

            -- Layout rows table (cached row breaks and heights)
            CREATE TABLE IF NOT EXISTS layout_rows (
                width_bucket INTEGER NOT NULL,
                sort_key TEXT NOT NULL,
                row_index INTEGER NOT NULL,
                row_height REAL NOT NULL,
                start_index INTEGER NOT NULL,
                end_index INTEGER NOT NULL,
                PRIMARY KEY (width_bucket, sort_key, row_index),
                FOREIGN KEY (width_bucket, sort_key)
                    REFERENCES layout_meta(width_bucket, sort_key)
                    ON DELETE CASCADE
            );

            -- Index for efficient row retrieval
            CREATE INDEX IF NOT EXISTS idx_layout_rows_bucket_sort
                ON layout_rows(width_bucket, sort_key);

            -- Favorites table
            CREATE TABLE IF NOT EXISTS favorites (
                path TEXT PRIMARY KEY NOT NULL,
                created_at INTEGER NOT NULL
            );

            -- Albums table
            CREATE TABLE IF NOT EXISTS albums (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- Album items join table
            CREATE TABLE IF NOT EXISTS album_items (
                album_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (album_id, path),
                FOREIGN KEY (album_id) REFERENCES albums(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_album_items_path ON album_items(path);
            ",
            )
            .context("Failed to create database tables")?;

        debug!("Database tables created/verified");
        Ok(())
    }

    // =========================================================================
    // Media Item Operations
    // =========================================================================

    /// Inserts or updates a single media item.
    pub fn upsert_media(&self, item: &MediaItem) -> Result<()> {
        self.conn
            .execute(
                "
            INSERT INTO media (
                path, media_type, mtime, size, width, height,
                duration_ms, thumb_path, thumb_w, thumb_h, last_seen
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(path) DO UPDATE SET
                media_type = excluded.media_type,
                mtime = excluded.mtime,
                size = excluded.size,
                width = excluded.width,
                height = excluded.height,
                duration_ms = excluded.duration_ms,
                thumb_path = excluded.thumb_path,
                thumb_w = excluded.thumb_w,
                thumb_h = excluded.thumb_h,
                last_seen = excluded.last_seen
            ",
                params![
                    item.path.to_string_lossy(),
                    media_type_to_int(item.media_type),
                    item.mtime,
                    item.size,
                    item.width,
                    item.height,
                    item.duration_ms,
                    item.thumb_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string()),
                    item.thumb_w,
                    item.thumb_h,
                    item.last_seen,
                ],
            )
            .context("Failed to upsert media item")?;

        Ok(())
    }

    /// Batch inserts or updates multiple media items in a single transaction.
    ///
    /// This is much faster than individual inserts for large batches.
    pub fn upsert_media_batch(&mut self, items: &[MediaItem]) -> Result<usize> {
        if items.is_empty() {
            return Ok(0);
        }

        let tx = self.conn.transaction()?;
        let count = Self::upsert_media_batch_in_tx(&tx, items)?;
        tx.commit()?;

        debug!("Batch upserted {} media items", count);
        Ok(count)
    }

    /// Internal batch upsert within a transaction.
    fn upsert_media_batch_in_tx(tx: &Transaction, items: &[MediaItem]) -> Result<usize> {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO media (
                path, media_type, mtime, size, width, height,
                duration_ms, thumb_path, thumb_w, thumb_h, last_seen
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(path) DO UPDATE SET
                media_type = excluded.media_type,
                mtime = excluded.mtime,
                size = excluded.size,
                width = excluded.width,
                height = excluded.height,
                duration_ms = excluded.duration_ms,
                thumb_path = excluded.thumb_path,
                thumb_w = excluded.thumb_w,
                thumb_h = excluded.thumb_h,
                last_seen = excluded.last_seen
            ",
        )?;

        let mut count = 0;
        for item in items {
            stmt.execute(params![
                item.path.to_string_lossy(),
                media_type_to_int(item.media_type),
                item.mtime,
                item.size,
                item.width,
                item.height,
                item.duration_ms,
                item.thumb_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                item.thumb_w,
                item.thumb_h,
                item.last_seen,
            ])?;
            count += 1;
        }

        Ok(count)
    }

    /// Retrieves a media item by its path.
    pub fn get_media(&self, path: &Path) -> Result<Option<MediaItem>> {
        let path_str = path.to_string_lossy();

        let result = self
            .conn
            .query_row(
                "
            SELECT path, media_type, mtime, size, width, height,
                   duration_ms, thumb_path, thumb_w, thumb_h, last_seen
            FROM media WHERE path = ?1
            ",
                params![path_str.as_ref()],
                |row| {
                    Ok(MediaItem {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        media_type: int_to_media_type(row.get(1)?),
                        mtime: row.get(2)?,
                        size: row.get(3)?,
                        width: row.get(4)?,
                        height: row.get(5)?,
                        duration_ms: row.get(6)?,
                        thumb_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
                        thumb_w: row.get(8)?,
                        thumb_h: row.get(9)?,
                        last_seen: row.get(10)?,
                    })
                },
            )
            .optional()
            .context("Failed to query media item")?;

        Ok(result)
    }

    /// Retrieves multiple media items by their paths.
    ///
    /// Returns items in the same order as the input paths (missing items are skipped).
    pub fn get_media_batch(&self, paths: &[PathBuf]) -> Result<Vec<MediaItem>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut items = Vec::with_capacity(paths.len());

        // For small batches, use individual queries
        // For large batches, this could be optimized with temp tables
        let mut stmt = self.conn.prepare_cached(
            "
            SELECT path, media_type, mtime, size, width, height,
                   duration_ms, thumb_path, thumb_w, thumb_h, last_seen
            FROM media WHERE path = ?1
            ",
        )?;

        for path in paths {
            let path_str = path.to_string_lossy();
            if let Some(item) = stmt
                .query_row(params![path_str.as_ref()], |row| {
                    Ok(MediaItem {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        media_type: int_to_media_type(row.get(1)?),
                        mtime: row.get(2)?,
                        size: row.get(3)?,
                        width: row.get(4)?,
                        height: row.get(5)?,
                        duration_ms: row.get(6)?,
                        thumb_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
                        thumb_w: row.get(8)?,
                        thumb_h: row.get(9)?,
                        last_seen: row.get(10)?,
                    })
                })
                .optional()?
            {
                items.push(item);
            }
        }

        Ok(items)
    }

    // =========================================================================
    // Favorites / Albums
    // =========================================================================

    /// Returns true if the path is marked as favorite.
    pub fn is_favorite(&self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let exists: Option<i32> = self
            .conn
            .query_row(
                "SELECT 1 FROM favorites WHERE path = ?1",
                params![path_str.as_ref()],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query favorite status")?;
        Ok(exists.is_some())
    }

    /// Toggles favorite status for the given path.
    /// Returns true if the item is now favorited.
    pub fn toggle_favorite(&self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let now = Self::now();
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO favorites (path, created_at) VALUES (?1, ?2)",
            params![path_str.as_ref(), now],
        )?;
        if inserted > 0 {
            return Ok(true);
        }
        self.conn.execute(
            "DELETE FROM favorites WHERE path = ?1",
            params![path_str.as_ref()],
        )?;
        Ok(false)
    }

    /// Returns all albums (id, name) ordered by name.
    pub fn list_albums(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, name FROM albums ORDER BY name COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut albums = Vec::new();
        for row in rows {
            albums.push(row?);
        }
        Ok(albums)
    }

    /// Creates a new album (or returns existing id if name already exists).
    pub fn create_album(&self, name: &str) -> Result<i64> {
        let now = Self::now();
        self.conn.execute(
            "INSERT OR IGNORE INTO albums (name, created_at, updated_at) VALUES (?1, ?2, ?2)",
            params![name, now],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM albums WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Adds a path to an album (no-op if already present).
    pub fn add_to_album(&self, album_id: i64, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let now = Self::now();
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO album_items (album_id, path, created_at) VALUES (?1, ?2, ?3)",
            params![album_id, path_str.as_ref(), now],
        )?;
        Ok(inserted > 0)
    }

    /// Retrieves all media items from the database.
    pub fn get_all_media(&self) -> Result<Vec<MediaItem>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT path, media_type, mtime, size, width, height,
                   duration_ms, thumb_path, thumb_w, thumb_h, last_seen
            FROM media
            ORDER BY path
            ",
        )?;

        let items = stmt
            .query_map([], |row| {
                Ok(MediaItem {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    media_type: int_to_media_type(row.get(1)?),
                    mtime: row.get(2)?,
                    size: row.get(3)?,
                    width: row.get(4)?,
                    height: row.get(5)?,
                    duration_ms: row.get(6)?,
                    thumb_path: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
                    thumb_w: row.get(8)?,
                    thumb_h: row.get(9)?,
                    last_seen: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to query all media items")?;

        Ok(items)
    }

    /// Checks if a cached media item is still valid based on mtime.
    ///
    /// Returns `true` if the cached mtime matches the provided mtime.
    pub fn is_cache_valid(&self, path: &Path, current_mtime: i64) -> Result<bool> {
        let path_str = path.to_string_lossy();

        let cached_mtime: Option<i64> = self
            .conn
            .query_row(
                "SELECT mtime FROM media WHERE path = ?1",
                params![path_str.as_ref()],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to check cache validity")?;

        Ok(cached_mtime == Some(current_mtime))
    }

    /// Gets cache entries (path, mtime, size) for checking validity.
    ///
    /// This is useful for scanning: compare filesystem mtime with cached mtime
    /// to determine which items need metadata refresh.
    pub fn get_cache_entries(&self) -> Result<Vec<CacheEntry>> {
        let mut stmt = self.conn.prepare("SELECT path, mtime, size FROM media")?;

        let entries = stmt
            .query_map([], |row| {
                Ok(CacheEntry {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    mtime: row.get(1)?,
                    size: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to get cache entries")?;

        Ok(entries)
    }

    /// Gets cache entries as a map from path to (mtime, size) for fast lookup.
    pub fn get_cache_map(&self) -> Result<std::collections::HashMap<PathBuf, (i64, i64)>> {
        let entries = self.get_cache_entries()?;
        let map = entries
            .into_iter()
            .map(|e| (e.path, (e.mtime, e.size)))
            .collect();
        Ok(map)
    }

    /// Updates the thumbnail information for a media item.
    pub fn update_thumbnail(
        &self,
        path: &Path,
        thumb_path: &Path,
        thumb_w: u32,
        thumb_h: u32,
    ) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let thumb_str = thumb_path.to_string_lossy();

        let rows_affected = self
            .conn
            .execute(
                "
            UPDATE media
            SET thumb_path = ?1, thumb_w = ?2, thumb_h = ?3
            WHERE path = ?4
            ",
                params![thumb_str.as_ref(), thumb_w, thumb_h, path_str.as_ref()],
            )
            .context("Failed to update thumbnail info")?;

        Ok(rows_affected > 0)
    }

    /// Updates the last_seen timestamp for items, used during scanning.
    pub fn touch_last_seen(&self, paths: &[PathBuf], timestamp: i64) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }

        let mut stmt = self
            .conn
            .prepare_cached("UPDATE media SET last_seen = ?1 WHERE path = ?2")?;

        let mut count = 0;
        for path in paths {
            count += stmt.execute(params![timestamp, path.to_string_lossy().as_ref()])?;
        }

        Ok(count)
    }

    /// Deletes media items that haven't been seen since the given timestamp.
    ///
    /// Returns the paths of deleted items (useful for cleaning up thumbnails).
    pub fn delete_stale(&self, older_than: i64) -> Result<Vec<PathBuf>> {
        // First, get the paths that will be deleted
        let mut stmt = self
            .conn
            .prepare("SELECT path, thumb_path FROM media WHERE last_seen < ?1")?;

        let stale: Vec<(PathBuf, Option<PathBuf>)> = stmt
            .query_map(params![older_than], |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    row.get::<_, Option<String>>(1)?.map(PathBuf::from),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let paths: Vec<PathBuf> = stale.iter().map(|(p, _)| p.clone()).collect();

        // Delete the stale entries
        let deleted = self.conn.execute(
            "DELETE FROM media WHERE last_seen < ?1",
            params![older_than],
        )?;

        if deleted > 0 {
            info!("Deleted {} stale media entries", deleted);
        }

        Ok(paths)
    }

    /// Deletes a single media item by path.
    pub fn delete_media(&self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy();
        let rows = self.conn.execute(
            "DELETE FROM media WHERE path = ?1",
            params![path_str.as_ref()],
        )?;
        Ok(rows > 0)
    }

    /// Returns the total count of media items in the database.
    pub fn count_media(&self) -> Result<i64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM media", [], |row| row.get(0))?;
        Ok(count)
    }

    // =========================================================================
    // Layout Cache Operations
    // =========================================================================

    /// Stores layout metadata (hash and item count for a width bucket + sort key).
    pub fn set_layout_meta(&self, meta: &LayoutMeta) -> Result<()> {
        self.conn.execute(
            "
            INSERT INTO layout_meta (width_bucket, sort_key, item_count, list_hash, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(width_bucket, sort_key) DO UPDATE SET
                item_count = excluded.item_count,
                list_hash = excluded.list_hash,
                updated_at = excluded.updated_at
            ",
            params![
                meta.width_bucket,
                meta.sort_key,
                meta.item_count,
                meta.list_hash,
                meta.updated_at,
            ],
        )?;

        Ok(())
    }

    /// Retrieves layout metadata for a given width bucket and sort key.
    pub fn get_layout_meta(&self, width_bucket: i32, sort_key: &str) -> Result<Option<LayoutMeta>> {
        let result = self
            .conn
            .query_row(
                "
            SELECT width_bucket, sort_key, item_count, list_hash, updated_at
            FROM layout_meta
            WHERE width_bucket = ?1 AND sort_key = ?2
            ",
                params![width_bucket, sort_key],
                |row| {
                    Ok(LayoutMeta {
                        width_bucket: row.get(0)?,
                        sort_key: row.get(1)?,
                        item_count: row.get(2)?,
                        list_hash: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()?;

        Ok(result)
    }

    /// Stores layout rows (the actual row breaks and heights).
    ///
    /// This replaces any existing rows for the given width bucket and sort key.
    pub fn set_layout_rows(&mut self, rows: &[LayoutRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;

        // Delete existing rows for this bucket/sort combination
        let first = &rows[0];
        tx.execute(
            "DELETE FROM layout_rows WHERE width_bucket = ?1 AND sort_key = ?2",
            params![first.width_bucket, first.sort_key],
        )?;

        // Insert new rows
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO layout_rows (width_bucket, sort_key, row_index, row_height, start_index, end_index)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
        )?;

        for row in rows {
            stmt.execute(params![
                row.width_bucket,
                row.sort_key,
                row.row_index,
                row.row_height,
                row.start_index,
                row.end_index,
            ])?;
        }

        drop(stmt);
        tx.commit()?;

        debug!(
            "Stored {} layout rows for bucket {} / {}",
            rows.len(),
            first.width_bucket,
            first.sort_key
        );

        Ok(())
    }

    /// Retrieves all layout rows for a given width bucket and sort key.
    pub fn get_layout_rows(&self, width_bucket: i32, sort_key: &str) -> Result<Vec<LayoutRow>> {
        let mut stmt = self.conn.prepare(
            "
            SELECT width_bucket, sort_key, row_index, row_height, start_index, end_index
            FROM layout_rows
            WHERE width_bucket = ?1 AND sort_key = ?2
            ORDER BY row_index
            ",
        )?;

        let rows = stmt
            .query_map(params![width_bucket, sort_key], |row| {
                Ok(LayoutRow {
                    width_bucket: row.get(0)?,
                    sort_key: row.get(1)?,
                    row_index: row.get(2)?,
                    row_height: row.get(3)?,
                    start_index: row.get(4)?,
                    end_index: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Checks if a cached layout is valid for the given parameters.
    ///
    /// A layout is valid if:
    /// - The layout meta exists for this width bucket and sort key
    /// - The list_hash matches (meaning the items haven't changed)
    /// - The item_count matches
    pub fn is_layout_valid(
        &self,
        width_bucket: i32,
        sort_key: &str,
        list_hash: &str,
        item_count: i32,
    ) -> Result<bool> {
        if let Some(meta) = self.get_layout_meta(width_bucket, sort_key)? {
            Ok(meta.list_hash == list_hash && meta.item_count == item_count)
        } else {
            Ok(false)
        }
    }

    /// Deletes all layout data for a specific width bucket and sort key.
    pub fn delete_layout(&self, width_bucket: i32, sort_key: &str) -> Result<()> {
        // Due to ON DELETE CASCADE, deleting from layout_meta will also delete rows
        self.conn.execute(
            "DELETE FROM layout_meta WHERE width_bucket = ?1 AND sort_key = ?2",
            params![width_bucket, sort_key],
        )?;

        Ok(())
    }

    /// Deletes all cached layouts (useful when the item list changes significantly).
    pub fn clear_all_layouts(&self) -> Result<()> {
        self.conn.execute("DELETE FROM layout_meta", [])?;
        // Rows are deleted via CASCADE
        info!("Cleared all cached layouts");
        Ok(())
    }

    // =========================================================================
    // Utility Methods
    // =========================================================================

    /// Computes a width bucket from a viewport width.
    ///
    /// Buckets are in increments of 100px to avoid recomputing layout
    /// for small resize operations.
    pub fn width_to_bucket(width: i32) -> i32 {
        (width / 100) * 100
    }

    /// Returns the current Unix timestamp.
    pub fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Runs VACUUM to compact the database.
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute("VACUUM", [])?;
        info!("Database vacuumed");
        Ok(())
    }

    /// Runs ANALYZE to update query planner statistics.
    pub fn analyze(&self) -> Result<()> {
        self.conn.execute("ANALYZE", [])?;
        debug!("Database analyzed");
        Ok(())
    }

    /// Gets database statistics for debugging.
    pub fn get_stats(&self) -> Result<DbStats> {
        let media_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM media", [], |r| r.get(0))?;

        let layout_meta_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM layout_meta", [], |r| r.get(0))?;

        let layout_rows_count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM layout_rows", [], |r| r.get(0))?;

        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;

        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;

        Ok(DbStats {
            media_count,
            layout_meta_count,
            layout_rows_count,
            db_size_bytes: page_count * page_size,
        })
    }

    /// Handles database corruption by backing up and rebuilding.
    ///
    /// This should be called if database operations fail with corruption errors.
    pub fn handle_corruption(path: &Path) -> Result<Self> {
        warn!("Handling potential database corruption at {:?}", path);

        // Rename the corrupted database
        let backup_path = path.with_extension("sqlite.corrupted");
        if path.exists() {
            std::fs::rename(path, &backup_path).with_context(|| {
                format!("Failed to backup corrupted database to {:?}", backup_path)
            })?;
            warn!("Backed up corrupted database to {:?}", backup_path);
        }

        // Create a fresh database
        Self::open(path)
    }
}

/// Database statistics for debugging and monitoring.
#[derive(Debug, Clone)]
pub struct DbStats {
    pub media_count: i64,
    pub layout_meta_count: i64,
    pub layout_rows_count: i64,
    pub db_size_bytes: i64,
}

// =========================================================================
// Helper Functions
// =========================================================================

/// Converts MediaType enum to integer for storage.
fn media_type_to_int(media_type: MediaType) -> i32 {
    match media_type {
        MediaType::Image => 0,
        MediaType::Video => 1,
        MediaType::Folder => 2,
    }
}

/// Converts stored integer back to MediaType enum.
fn int_to_media_type(value: i32) -> MediaType {
    match value {
        1 => MediaType::Video,
        2 => MediaType::Folder,
        _ => MediaType::Image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;

    fn test_media_item(path: &str) -> MediaItem {
        MediaItem {
            path: PathBuf::from(path),
            media_type: MediaType::Image,
            mtime: 1234567890,
            size: 1024,
            width: 1920,
            height: 1080,
            duration_ms: None,
            thumb_path: None,
            thumb_w: None,
            thumb_h: None,
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        }
    }

    #[test]
    fn test_open_and_create() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");

        let store = MediaStore::open(&db_path).unwrap();
        assert!(db_path.exists());

        let stats = store.get_stats().unwrap();
        assert_eq!(stats.media_count, 0);
    }

    #[test]
    fn test_upsert_and_get_media() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let store = MediaStore::open(&db_path).unwrap();

        let item = test_media_item("/test/image.jpg");
        store.upsert_media(&item).unwrap();

        let retrieved = store.get_media(&item.path).unwrap().unwrap();
        assert_eq!(retrieved.path, item.path);
        assert_eq!(retrieved.width, 1920);
        assert_eq!(retrieved.height, 1080);
    }

    #[test]
    fn test_batch_upsert() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let mut store = MediaStore::open(&db_path).unwrap();

        let items: Vec<MediaItem> = (0..100)
            .map(|i| test_media_item(&format!("/test/image_{}.jpg", i)))
            .collect();

        let count = store.upsert_media_batch(&items).unwrap();
        assert_eq!(count, 100);

        let stats = store.get_stats().unwrap();
        assert_eq!(stats.media_count, 100);
    }

    #[test]
    fn test_cache_validity() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let store = MediaStore::open(&db_path).unwrap();

        let item = test_media_item("/test/image.jpg");
        store.upsert_media(&item).unwrap();

        // Same mtime should be valid
        assert!(store.is_cache_valid(&item.path, item.mtime).unwrap());

        // Different mtime should be invalid
        assert!(!store.is_cache_valid(&item.path, item.mtime + 1).unwrap());

        // Non-existent path should be invalid
        assert!(!store
            .is_cache_valid(&PathBuf::from("/nonexistent"), 0)
            .unwrap());
    }

    #[test]
    fn test_layout_cache() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let mut store = MediaStore::open(&db_path).unwrap();

        let meta = LayoutMeta {
            width_bucket: 1200,
            sort_key: "name_asc".to_string(),
            item_count: 50,
            list_hash: "abc123".to_string(),
            updated_at: MediaStore::now(),
        };
        store.set_layout_meta(&meta).unwrap();

        let rows = vec![
            LayoutRow {
                width_bucket: 1200,
                sort_key: "name_asc".to_string(),
                row_index: 0,
                row_height: 200.0,
                start_index: 0,
                end_index: 4,
            },
            LayoutRow {
                width_bucket: 1200,
                sort_key: "name_asc".to_string(),
                row_index: 1,
                row_height: 220.0,
                start_index: 5,
                end_index: 9,
            },
        ];
        store.set_layout_rows(&rows).unwrap();

        // Check validity
        assert!(store
            .is_layout_valid(1200, "name_asc", "abc123", 50)
            .unwrap());
        assert!(!store
            .is_layout_valid(1200, "name_asc", "different", 50)
            .unwrap());
        assert!(!store
            .is_layout_valid(1200, "name_asc", "abc123", 100)
            .unwrap());

        // Retrieve rows
        let retrieved = store.get_layout_rows(1200, "name_asc").unwrap();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].row_height, 200.0);
        assert_eq!(retrieved[1].start_index, 5);
    }

    #[test]
    fn test_width_bucket() {
        assert_eq!(MediaStore::width_to_bucket(1920), 1900);
        assert_eq!(MediaStore::width_to_bucket(1280), 1200);
        assert_eq!(MediaStore::width_to_bucket(1350), 1300);
        assert_eq!(MediaStore::width_to_bucket(99), 0);
    }

    #[test]
    fn test_delete_stale() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let store = MediaStore::open(&db_path).unwrap();

        let now = MediaStore::now();

        // Insert items with different last_seen times
        let mut old_item = test_media_item("/test/old.jpg");
        old_item.last_seen = now - 1000;
        store.upsert_media(&old_item).unwrap();

        let mut new_item = test_media_item("/test/new.jpg");
        new_item.last_seen = now;
        store.upsert_media(&new_item).unwrap();

        // Delete items older than now - 500
        let deleted = store.delete_stale(now - 500).unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0], old_item.path);

        // Check that only new item remains
        assert!(store.get_media(&old_item.path).unwrap().is_none());
        assert!(store.get_media(&new_item.path).unwrap().is_some());
    }

    #[test]
    fn test_update_thumbnail() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");
        let store = MediaStore::open(&db_path).unwrap();

        let item = test_media_item("/test/image.jpg");
        store.upsert_media(&item).unwrap();

        let thumb_path = PathBuf::from("/cache/thumb_abc.jpg");
        store
            .update_thumbnail(&item.path, &thumb_path, 256, 144)
            .unwrap();

        let updated = store.get_media(&item.path).unwrap().unwrap();
        assert_eq!(updated.thumb_path, Some(thumb_path));
        assert_eq!(updated.thumb_w, Some(256));
        assert_eq!(updated.thumb_h, Some(144));
    }
}
