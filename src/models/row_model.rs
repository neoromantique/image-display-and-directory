use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RowItem {
    pub media_path: PathBuf,
    pub display_w: f32,
    pub display_h: f32,
    pub offset_top: f32,
    pub is_folder: bool,
}

#[derive(Debug, Clone)]
pub struct RowModel {
    pub row_index: u32,
    pub height_px: f32,
    pub items: Vec<RowItem>,
}

impl RowModel {
    pub fn new(row_index: u32, height_px: f32, items: Vec<RowItem>) -> Self {
        Self {
            row_index,
            height_px,
            items,
        }
    }
}
