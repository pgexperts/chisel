// stats.rs — Database statistics: handle count, page counts, file size.

#[derive(Debug, Clone)]
pub struct Stats {
    pub handle_count: u64,
    pub total_pages: u64,
    pub file_size_bytes: u64,
}
