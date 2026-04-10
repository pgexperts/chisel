// overflow.rs — Overflow page chains for values exceeding the slotted page body.
// Each overflow page stores up to OVERFLOW_PAYLOAD bytes of the value, plus a
// header linking to the next page. The handle table entry points to the first
// overflow page; the chain is followed to reconstruct the full value.

use crate::error::Result;
use crate::page::{self, PageType, CHECKSUM_OFFSET};
use crate::page_cache::PageCache;

// Overflow page body layout (after common 16-byte header, before 8-byte checksum):
// bytes 16..24: total_length (u64) — full value size (repeated on every page)
// bytes 24..32: next_page (u64) — next overflow page, or 0 if last
// bytes 32..CHECKSUM_OFFSET: payload
const OVERFLOW_HEADER_END: usize = 32;
const OVERFLOW_PAYLOAD: usize = CHECKSUM_OFFSET - OVERFLOW_HEADER_END; // 8152

pub struct Overflow;

impl Overflow {
    /// Write a value as a chain of overflow pages. Returns the page ID of the first page.
    pub fn write(cache: &mut PageCache, value: &[u8]) -> Result<u64> {
        let total_length = value.len() as u64;
        let num_pages = (value.len() + OVERFLOW_PAYLOAD - 1) / OVERFLOW_PAYLOAD;

        let mut page_ids = Vec::with_capacity(num_pages);
        for _ in 0..num_pages {
            page_ids.push(cache.new_page()?);
        }

        for (i, &page_id) in page_ids.iter().enumerate() {
            let start = i * OVERFLOW_PAYLOAD;
            let end = std::cmp::min(start + OVERFLOW_PAYLOAD, value.len());
            let chunk = &value[start..end];

            let next_page = if i + 1 < page_ids.len() {
                page_ids[i + 1]
            } else {
                0
            };

            let buf = cache.get_mut(page_id)?;
            buf.fill(0);
            buf[0] = PageType::Overflow as u8;
            buf[16..24].copy_from_slice(&total_length.to_le_bytes());
            buf[24..32].copy_from_slice(&next_page.to_le_bytes());
            buf[OVERFLOW_HEADER_END..OVERFLOW_HEADER_END + chunk.len()].copy_from_slice(chunk);
            page::stamp_checksum(buf);
        }

        Ok(page_ids[0])
    }

    /// Read a complete value from an overflow chain.
    pub fn read(cache: &mut PageCache, first_page: u64) -> Result<Vec<u8>> {
        let buf = cache.get(first_page)?;
        let total_length = u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize;
        let mut result = Vec::with_capacity(total_length);

        let mut current_page = first_page;
        loop {
            let buf = cache.get(current_page)?;
            let next_page = u64::from_le_bytes(buf[24..32].try_into().unwrap());
            let remaining = total_length - result.len();
            let chunk_len = std::cmp::min(remaining, OVERFLOW_PAYLOAD);
            result.extend_from_slice(&buf[OVERFLOW_HEADER_END..OVERFLOW_HEADER_END + chunk_len]);

            if next_page == 0 {
                break;
            }
            current_page = next_page;
        }

        Ok(result)
    }

    /// Delete an overflow chain. Returns the list of page IDs freed.
    pub fn delete(cache: &mut PageCache, first_page: u64) -> Result<Vec<u64>> {
        let mut freed = Vec::new();
        let mut current_page = first_page;
        loop {
            let buf = cache.get(current_page)?;
            let next_page = u64::from_le_bytes(buf[24..32].try_into().unwrap());
            freed.push(current_page);
            if next_page == 0 {
                break;
            }
            current_page = next_page;
        }
        Ok(freed)
    }
}
