use std::{alloc::Layout, collections::BTreeMap, mem::size_of, ptr::NonNull};

use bumpalo::Bump;

use crate::{
    meta::Meta,
    page::{Page, PageID},
    Result,
};

pub(crate) struct TxFreelist {
    pub(crate) meta: Meta,
    pub(crate) inner: Freelist,
    pub(crate) pages: BTreeMap<u64, (NonNull<u8>, usize)>,
    pub(crate) arena: Bump,
}

impl<'a> TxFreelist {
    pub(crate) fn new(meta: Meta, inner: Freelist) -> TxFreelist {
        TxFreelist {
            meta,
            inner,
            pages: BTreeMap::new(),
            arena: Bump::new(),
        }
    }

    pub(crate) fn free(&mut self, page_id: PageID, num_pages: u64) {
        debug_assert!(num_pages > 0, "cannot free zero pages");
        for id in page_id..(page_id + num_pages) {
            self.inner.free(self.meta.tx_id, id);
        }
    }

    pub(crate) fn allocate<'b>(&'b mut self, bytes: u64) -> Result<&'a mut Page> {
        assert!(
            bytes >= (size_of::<Page>() as u64),
            "cannot allocate {} bytes, minimum is {}, {}",
            bytes,
            size_of::<Page>(),
            bytes < (size_of::<Page>() as u64)
        );
        let num_pages = if (bytes % self.meta.pagesize) == 0 {
            bytes / self.meta.pagesize
        } else {
            (bytes / self.meta.pagesize) + 1
        };
        let page_id = match self.inner.allocate(num_pages as usize) {
            Some(page_id) => page_id,
            None => {
                let page_id = self.meta.num_pages;
                self.meta.num_pages += num_pages;
                page_id
            }
        };

        let ptr = self
            .arena
            .alloc_layout(Layout::from_size_align(bytes as usize, 8)?);

        let page = unsafe { &mut *(ptr.as_ptr() as *mut Page) };
        page.id = page_id;
        page.overflow = num_pages - 1;
        self.pages.insert(page_id, (ptr, bytes as usize));

        Ok(page)
    }
}

#[derive(Clone)]
pub(crate) struct Freelist {
    // Every page that is reusable right now is stored as a contiguous span
    // `[start, start + len)`. The two maps are canonical views of the same
    // spans, so every mutation updates both together and neither index can
    // drift out of sync after a free or an allocation.
    //
    // `free_spans` locates a span's neighbors (for merging on release) in
    // log time, while `spans_by_size` lets `allocate` jump straight to a span
    // that is long enough instead of walking every free page.
    free_spans: BTreeMap<PageID, u64>,
    spans_by_size: BTreeMap<(u64, PageID), ()>,
    pending_pages: BTreeMap<u64, Vec<PageID>>,
}

const HEADER_SIZE: u64 = size_of::<Page>() as u64;
const PAGE_ID_SIZE: u64 = size_of::<PageID>() as u64;

impl Freelist {
    pub(crate) fn new() -> Freelist {
        Freelist {
            free_spans: BTreeMap::new(),
            spans_by_size: BTreeMap::new(),
            pending_pages: BTreeMap::new(),
        }
    }

    // builds the span indexes from the flat list of free page ids stored in
    // the freelist page on disk
    pub(crate) fn init(&mut self, free_pages: &[PageID]) {
        for id in free_pages {
            self.insert_page(*id);
        }
        self.debug_assert_consistent();
    }

    // inserts a single reusable page, coalescing it with the spans directly
    // before and after it whenever they are adjacent
    fn insert_page(&mut self, page_id: PageID) {
        // already part of a span, nothing to do (can happen with overlapping
        // pending pages from different transactions)
        if let Some((&start, &len)) = self.free_spans.range(..=page_id).next_back() {
            if page_id < start + len {
                return;
            }
        }

        let mut start = page_id;
        let mut len = 1;

        // absorb every span immediately preceding page_id
        while let Some((&prev_start, &prev_len)) = self.free_spans.range(..start).next_back() {
            if prev_start + prev_len != start {
                break;
            }
            self.remove_span(prev_start, prev_len);
            start = prev_start;
            len += prev_len;
        }

        // absorb every span immediately following the span built so far
        while let Some(&next_len) = self.free_spans.get(&(start + len)) {
            let next_start = start + len;
            self.remove_span(next_start, next_len);
            len += next_len;
        }

        self.add_span(start, len);
    }

    fn add_span(&mut self, start: PageID, len: u64) {
        self.free_spans.insert(start, len);
        self.spans_by_size.insert((len, start), ());
    }

    fn remove_span(&mut self, start: PageID, len: u64) {
        self.free_spans.remove(&start);
        self.spans_by_size.remove(&(len, start));
    }

    // adds the page to the transaction's set of free pages
    pub(crate) fn free(&mut self, tx_id: u64, page_id: PageID) {
        debug_assert!(
            page_id > 1,
            "cannot free page {}, reserved for meta",
            page_id
        );
        let pages = self.pending_pages.entry(tx_id).or_insert_with(Vec::new);
        pages.push(page_id);
    }

    // frees all pages from old transactions that have lower ids than the given tx_id
    pub(crate) fn release(&mut self, tx_id: u64) {
        let pending_ids: Vec<u64> = self.pending_pages.keys().cloned().collect();
        for other_tx_id in pending_ids {
            if other_tx_id < tx_id {
                let pages = self.pending_pages.remove(&other_tx_id).unwrap();
                pages.into_iter().for_each(|p| self.insert_page(p));
            } else {
                break;
            }
        }
        self.debug_assert_consistent();
    }

    // finds a contiguous run of `num_pages` free pages without scanning the
    // whole free set. The size index yields the shortest span that fits, with
    // the lowest starting page as a tie-breaker; a span longer than requested
    // is split instead of forcing file growth.
    pub(crate) fn allocate(&mut self, num_pages: usize) -> Option<PageID> {
        let num_pages = num_pages as u64;
        if self.free_spans.is_empty() {
            return None;
        }

        // smallest (len, start) with len >= num_pages
        let &(len, start) = self
            .spans_by_size
            .range((num_pages, PageID::MIN)..)
            .next()?
            .0;

        debug_assert!(
            start > 1,
            "pageID {} cannot be in freelist, reserved for meta",
            start
        );

        self.remove_span(start, len);
        if len > num_pages {
            self.add_span(start + num_pages, len - num_pages);
        }
        Some(start)
    }

    // flattens the spans back into the sorted page-id list written to the
    // freelist page; pending pages are appended and the whole thing is sorted
    pub(crate) fn pages(&self) -> Vec<PageID> {
        let mut page_ids: Vec<PageID> = Vec::new();
        for (&start, &len) in self.free_spans.iter() {
            page_ids.extend(start..start + len);
        }
        for (_, pages) in self.pending_pages.iter() {
            let mut pages = pages.to_vec();
            page_ids.append(&mut pages);
        }
        page_ids.sort_unstable();
        page_ids
    }

    pub(crate) fn size(&self) -> u64 {
        let count = self.pages().len() as u64;
        HEADER_SIZE + (PAGE_ID_SIZE * count)
    }

    #[cfg(debug_assertions)]
    fn debug_assert_consistent(&self) {
        assert_eq!(self.free_spans.len(), self.spans_by_size.len());
        for (start, len) in self.free_spans.iter() {
            assert!(self.spans_by_size.contains_key(&(*len, *start)));
        }
    }

    #[cfg(not(debug_assertions))]
    fn debug_assert_consistent(&self) {}

    #[cfg(test)]
    fn indexes_agree(&self) -> bool {
        self.free_spans.len() == self.spans_by_size.len()
            && self
                .free_spans
                .iter()
                .all(|(start, len)| self.spans_by_size.contains_key(&(*len, *start)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{errors::Result, testutil::RandomFile, OpenOptions};

    fn freelist_from_vec(v: Vec<PageID>) -> Freelist {
        let mut freelist = Freelist {
            free_spans: BTreeMap::new(),
            spans_by_size: BTreeMap::new(),
            pending_pages: BTreeMap::new(),
        };
        freelist.init(v.as_slice());
        freelist
    }

    #[test]
    fn test_allocate() {
        let mut freelist = freelist_from_vec(vec![2, 4, 6, 8, 9, 10]);
        assert_eq!(freelist.allocate(4), None);
        assert_eq!(freelist.allocate(1), Some(2));
        assert_eq!(freelist.pages(), vec![4, 6, 8, 9, 10]);
        assert_eq!(freelist.allocate(1), Some(4));
        assert_eq!(freelist.pages(), vec![6, 8, 9, 10]);
        assert_eq!(freelist.allocate(3), Some(8));
        assert_eq!(freelist.pages(), vec![6]);
        assert_eq!(freelist.allocate(1), Some(6));
        assert_eq!(freelist.pages(), vec![]);
        assert_eq!(freelist.allocate(1), None);
    }

    #[test]
    fn test_allocate_splits_long_span() {
        // a single span of 8 pages should satisfy smaller requests instead of
        // forcing the caller to extend the file, and keep the remainder usable
        let mut freelist = freelist_from_vec(vec![10, 11, 12, 13, 14, 15, 16, 17]);
        assert_eq!(freelist.allocate(3), Some(10));
        assert_eq!(freelist.pages(), vec![13, 14, 15, 16, 17]);
        assert_eq!(freelist.allocate(5), Some(13));
        assert_eq!(freelist.pages(), vec![]);
        assert_eq!(freelist.allocate(1), None);
    }

    #[test]
    fn test_allocate_prefers_smallest_fitting_span() {
        // spans of length 1, 3 and 5; a request for 2 must take the length-3
        // span rather than chewing up the length-5 one
        let mut freelist = freelist_from_vec(vec![2, 8, 9, 10, 20, 21, 22, 23, 24]);
        assert_eq!(freelist.allocate(2), Some(8));
        assert_eq!(freelist.pages(), vec![2, 10, 20, 21, 22, 23, 24]);
    }

    #[test]
    fn test_insert_page_merges_adjacent_spans() {
        let mut freelist = Freelist::new();
        freelist.insert_page(10);
        assert_eq!(freelist.free_spans.get(&10), Some(&1));
        // extends the span to the right
        freelist.insert_page(11);
        assert_eq!(freelist.free_spans.len(), 1);
        assert_eq!(freelist.free_spans.get(&10), Some(&2));
        // extends the span to the left
        freelist.insert_page(9);
        assert_eq!(freelist.free_spans.len(), 1);
        assert_eq!(freelist.free_spans.get(&9), Some(&3));
        // bridges two separate spans
        freelist.insert_page(5);
        freelist.insert_page(7);
        assert_eq!(freelist.free_spans.get(&5), Some(&1));
        assert_eq!(freelist.free_spans.get(&7), Some(&1));
        freelist.insert_page(6);
        // 6 bridges [5] and [7] into [5, 6, 7]; [9, 10, 11] stays separate
        // because page 8 is still allocated
        assert_eq!(freelist.free_spans.len(), 2);
        assert_eq!(freelist.free_spans.get(&5), Some(&3));
        assert_eq!(freelist.free_spans.get(&9), Some(&3));
        assert_eq!(freelist.pages(), vec![5, 6, 7, 9, 10, 11]);
        // inserting page 8 now bridges the two remaining spans
        freelist.insert_page(8);
        assert_eq!(freelist.free_spans.len(), 1);
        assert_eq!(freelist.free_spans.get(&5), Some(&7));
        assert_eq!(freelist.pages(), vec![5, 6, 7, 8, 9, 10, 11]);
        // inserting an already free page is a no-op
        freelist.insert_page(8);
        assert_eq!(freelist.free_spans.len(), 1);
        assert_eq!(freelist.free_spans.get(&5), Some(&7));
    }

    #[test]
    fn test_release_merges_pending_pages() {
        let mut freelist = freelist_from_vec(vec![2, 3]);
        freelist.free(1, 4);
        freelist.free(1, 5);
        freelist.release(2);
        // 2..=5 is one contiguous span now
        assert_eq!(freelist.free_spans.len(), 1);
        assert_eq!(freelist.free_spans.get(&2), Some(&4));
        assert_eq!(freelist.pages(), vec![2, 3, 4, 5]);
    }

    #[test]
    fn test_init_coalesces_out_of_order_pages() {
        // the on-disk freelist ids are normally sorted, but building the span
        // indexes must not depend on insertion order
        let mut freelist = Freelist::new();
        freelist.init(&[11, 4, 9, 2, 10, 3, 8]);
        assert_eq!(freelist.free_spans.len(), 2);
        assert_eq!(freelist.free_spans.get(&2), Some(&3));
        assert_eq!(freelist.free_spans.get(&8), Some(&4));
        assert_eq!(freelist.pages(), vec![2, 3, 4, 8, 9, 10, 11]);
        assert!(freelist.indexes_agree());
    }

    #[test]
    fn test_large_freelist_allocation_is_logarithmic() {
        use std::time::Instant;

        // model a long-lived database with 200k stable, scattered free pages:
        // no two free pages are adjacent, so any multi-page request cannot be
        // served and must not fall back to a full scan of the free set.
        let free_pages: Vec<PageID> = (0..200_000u64).map(|i| i * 2 + 2).collect();
        let mut freelist = freelist_from_vec(free_pages);
        assert_eq!(freelist.pages().len(), 200_000);
        assert_eq!(freelist.free_spans.len(), 200_000);

        let start = Instant::now();
        for _ in 0..500 {
            assert_eq!(freelist.allocate(2), None);
        }
        let elapsed = start.elapsed();
        // nothing was allocated, every lookup must be an index miss
        assert_eq!(freelist.pages().len(), 200_000);
        // an O(n) scan takes tens of milliseconds here; log-time lookups must
        // finish well below that
        assert!(
            elapsed.as_millis() < 50,
            "500 allocations over 200k free pages took {:?}",
            elapsed
        );

        // successful single-page allocations are log-time too and leave the
        // indexes internally consistent
        let start = Instant::now();
        for _ in 0..500 {
            freelist.allocate(1).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 50,
            "500 single-page allocations over 200k free pages took {:?}",
            elapsed
        );
        assert_eq!(freelist.pages().len(), 199_500);
        assert!(freelist.indexes_agree());
    }

    #[test]
    fn test_free() {
        let mut freelist = Freelist::new();

        freelist.free(1, 5);
        assert_eq!(freelist.pending_pages.len(), 1);
        assert_eq!(freelist.pending_pages.get(&1), Some(&vec![5]));
        freelist.free(1, 4);
        assert_eq!(freelist.pending_pages.len(), 1);
        assert_eq!(freelist.pending_pages.get(&1), Some(&vec![5, 4]));
        freelist.free(1, 3);
        assert_eq!(freelist.pending_pages.len(), 1);
        assert_eq!(freelist.pending_pages.get(&1), Some(&vec![5, 4, 3]));
        freelist.free(2, 7);
        assert_eq!(freelist.pending_pages.len(), 2);
        assert_eq!(freelist.pending_pages.get(&1), Some(&vec![5, 4, 3]));
        assert_eq!(freelist.pending_pages.get(&2), Some(&vec![7]));
        assert!(freelist.free_spans.is_empty());
    }

    #[test]
    fn test_pages() {
        let mut freelist = freelist_from_vec(vec![1, 2, 3, 4, 5]);

        assert_eq!(freelist.pages(), vec![1, 2, 3, 4, 5]);
        freelist.free(2, 9);
        freelist.free(2, 10);
        freelist.free(2, 11);
        assert_eq!(freelist.pages(), vec![1, 2, 3, 4, 5, 9, 10, 11]);

        freelist.free(1, 6);
        freelist.free(1, 7);
        freelist.free(1, 8);
        assert_eq!(freelist.pages(), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn test_release() {
        let mut freelist = Freelist::new();

        freelist.free(1, 5);
        freelist.free(1, 10);
        freelist.free(1, 7);

        assert_eq!(freelist.free_spans.len(), 0);
        assert_eq!(freelist.pending_pages.len(), 1);
        freelist.release(1);
        assert_eq!(freelist.free_spans.len(), 0);
        assert_eq!(freelist.pending_pages.len(), 1);

        freelist.release(2);
        assert_eq!(freelist.free_spans.len(), 3);
        assert_eq!(freelist.pending_pages.len(), 0);
        assert_eq!(freelist.pages(), vec![5, 7, 10]);
    }

    #[test]
    fn test_size() {
        let freelist = freelist_from_vec(vec![1, 2, 3]);
        assert_eq!(freelist.size(), HEADER_SIZE + (PAGE_ID_SIZE * 3));
    }

    #[test]
    fn test_allocate_no_freelist() -> Result<()> {
        let random_file = RandomFile::new();
        let db = OpenOptions::new()
            .pagesize(1024)
            .num_pages(4)
            .open(&random_file)?;
        let tx = db.tx(false)?;
        let tx = tx.inner.borrow_mut();
        let mut freelist = tx.freelist.borrow_mut();
        // make sure we have an empty freelist and only four pages
        assert_eq!(freelist.inner.pages().len(), 0);
        assert_eq!(tx.meta.num_pages, 4);
        // allocate one page worth of bytes
        let page = freelist.allocate(1024)?;
        assert!(page.id == 4);
        assert!(page.overflow == 0);
        // allocate a half page worth of bytes
        let page = freelist.allocate(512)?;
        assert!(page.id == 5);
        assert!(page.overflow == 0);

        // allocate ten pages worth of bytes
        let page = freelist.allocate(10240)?;
        assert!(page.id == 6);
        assert!(page.overflow == 9);

        // allocate a non pagesize number of bytes
        let page = freelist.allocate(1234)?;
        assert!(page.id == 16);
        assert!(page.overflow == 1);

        Ok(())
    }

    #[test]
    fn test_allocate_freelist() -> Result<()> {
        let random_file = RandomFile::new();
        let db = OpenOptions::new()
            .pagesize(1024)
            .num_pages(100)
            .open(&random_file)?;
        let tx = db.tx(false)?;
        let tx = tx.inner.borrow_mut();
        let mut freelist = tx.freelist.borrow_mut();

        // setup the freelist and num_pages to simulate a used database
        for page in [10_u64, 11, 13, 14, 15].iter() {
            freelist.free(*page, 1);
        }
        freelist.inner.release(1);
        freelist.meta.num_pages = 99;

        // allocate one page worth of bytes (should come from freelist)
        let page = freelist.allocate(1024)?;
        assert!(page.id == 10);
        assert!(page.overflow == 0);
        // allocate a half page worth of bytes (should come from freelist)
        let page = freelist.allocate(512)?;
        assert!(page.id == 11);
        assert!(page.overflow == 0);

        // allocate three-ish pages worth of bytes (should come from freelist)
        let page = freelist.allocate(3000)?;
        assert!(page.id == 13);
        assert!(page.overflow == 2);

        // allocate a small number of bytes
        let page = freelist.allocate(100)?;
        assert!(page.id == 99);
        assert!(page.overflow == 0);
        Ok(())
    }

    #[test]
    fn test_tx_free() -> Result<()> {
        let random_file = RandomFile::new();
        let db = OpenOptions::new()
            .pagesize(1024)
            .num_pages(100)
            .open(&random_file)?;
        let tx = db.tx(false)?;
        let tx = tx.inner.borrow_mut();
        let mut freelist = tx.freelist.borrow_mut();

        assert_eq!(tx.meta.tx_id, 0);
        assert_eq!(freelist.inner.pages().len(), 0);
        freelist.free(80, 1);
        assert_eq!(freelist.inner.pages(), vec![80]);
        freelist.free(100, 6);
        assert_eq!(
            freelist.inner.pages(),
            vec![80, 100, 101, 102, 103, 104, 105]
        );

        Ok(())
    }
}
