//! The opaque page cursor handed to the host between paged searches.
//!
//! MediathekViewWeb pages by result offset, so the cursor records the offset
//! of the next page and how many pages this search has already read.
//!
//! Format: `mvw1.<pages read>.<offset>`.

use crate::api::MAX_OFFSET;

const PREFIX: &str = "mvw1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCursor {
    /// Pages this search has already read, including the one that produced
    /// this cursor.
    pub pages_read: u32,
    /// The offset of the next page.
    pub offset: usize,
}

impl PageCursor {
    pub fn encode(&self) -> String {
        format!("{PREFIX}.{}.{}", self.pages_read, self.offset)
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split('.');
        if parts.next()? != PREFIX {
            return None;
        }
        let pages_read = digits(parts.next()?)?.parse::<u32>().ok()?;
        let offset = digits(parts.next()?)?.parse::<usize>().ok()?;
        if parts.next().is_some() || pages_read == 0 || offset == 0 || offset > MAX_OFFSET {
            return None;
        }
        Some(Self { pages_read, offset })
    }
}

fn digits(part: &str) -> Option<&str> {
    (!part.is_empty() && part.len() <= 6 && part.chars().all(|ch| ch.is_ascii_digit()))
        .then_some(part)
}
