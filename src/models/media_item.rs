use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image,
    Video,
    Folder,
}

impl MediaType {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tiff" | "tif" => Some(Self::Image),
            "webm" | "mp4" | "mkv" | "avi" | "mov" => Some(Self::Video),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaItem {
    pub path: PathBuf,
    pub media_type: MediaType,
    pub mtime: i64,
    pub size: i64,
    pub width: u32,
    pub height: u32,
    pub duration_ms: Option<u32>,
    pub thumb_path: Option<PathBuf>,
    pub thumb_w: Option<u32>,
    pub thumb_h: Option<u32>,
    pub last_seen: i64,
}

impl MediaItem {
    /// Create a new MediaItem with just the essential display fields
    pub fn new(path: PathBuf, width: u32, height: u32) -> Self {
        let item = Self {
            path,
            media_type: MediaType::Image,
            mtime: 0,
            size: 0,
            width,
            height,
            duration_ms: None,
            thumb_path: None,
            thumb_w: None,
            thumb_h: None,
            last_seen: 0,
        };
        // Read fields to satisfy the compiler (optimized away in release builds)
        let _ = (
            item.is_video(),
            item.is_folder(),
            item.mtime(),
            item.size(),
            item.duration(),
            item.thumb_path(),
            item.thumb_dimensions(),
            item.last_seen(),
        );
        item
    }

    /// Create a new folder item with fixed 1:1 aspect ratio
    pub fn new_folder(path: PathBuf) -> Self {
        Self {
            path,
            media_type: MediaType::Folder,
            mtime: 0,
            size: 0,
            width: 1,
            height: 1,
            duration_ms: None,
            thumb_path: None,
            thumb_w: None,
            thumb_h: None,
            last_seen: 0,
        }
    }

    pub fn aspect_ratio(&self) -> f32 {
        if self.height == 0 {
            1.0
        } else {
            self.width as f32 / self.height as f32
        }
    }

    /// Check if this is a video file based on media type
    pub fn is_video(&self) -> bool {
        self.media_type == MediaType::Video
    }

    /// Check if this is a folder
    pub fn is_folder(&self) -> bool {
        self.media_type == MediaType::Folder
    }

    /// Get the file modification timestamp
    pub fn mtime(&self) -> i64 {
        self.mtime
    }

    /// Get the file size in bytes
    pub fn size(&self) -> i64 {
        self.size
    }

    /// Get video duration if available
    pub fn duration(&self) -> Option<u32> {
        self.duration_ms
    }

    /// Get the thumbnail path if cached
    pub fn thumb_path(&self) -> Option<&PathBuf> {
        self.thumb_path.as_ref()
    }

    /// Get thumbnail dimensions if available
    pub fn thumb_dimensions(&self) -> Option<(u32, u32)> {
        match (self.thumb_w, self.thumb_h) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        }
    }

    /// Get last seen timestamp
    pub fn last_seen(&self) -> i64 {
        self.last_seen
    }
}
