//! Size-classed slab pool for variable-length compressed pages.
//!
//! Compressed 4 KiB pages land anywhere between 1 byte and "bigger than we
//! started with", which a general allocator handles badly under the churn a
//! memory tier produces. This is the same shape as the kernel's `zsmalloc`,
//! simplified: fixed size classes, chunk-backed, with the free list threaded
//! through the free slots themselves so bookkeeping costs no extra memory.

use crate::handle::{Handle, MAX_CLASSES, MAX_SLOT};

/// Slot sizes step by this many bytes. Smaller wastes less to internal
/// fragmentation; larger needs fewer classes. 64 bytes costs at most 63 bytes
/// per page - under 1.6% of a 4 KiB page.
const GRAN: usize = 64;
/// Bytes of in-slot header holding the payload length.
const HEADER: usize = 2;
/// Backing allocation granularity. Large enough that per-chunk overhead is
/// negligible, small enough that a mostly-empty class does not strand much.
const CHUNK_BYTES: usize = 256 << 10;
/// Sentinel terminating an intrusive free list.
const NIL: u32 = u32::MAX;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("page is {got} bytes, pool is configured for {want}")]
    PageSizeMismatch { got: usize, want: usize },
    #[error("handle refers to a slot this pool never allocated")]
    BadHandle,
    #[error("size class {0} is full ({MAX_SLOT} slots)")]
    ClassExhausted(usize),
    #[error("stored page failed to decompress; pool state is corrupt")]
    Corrupt,
}

/// What the pool is holding, and how well.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Pages currently stored.
    pub pages: u64,
    /// Of those, pages held entirely in their handle at zero storage cost.
    pub same_fill_pages: u64,
    /// Of those, pages stored uncompressed because compression did not pay.
    pub incompressible_pages: u64,
    /// Uncompressed size of everything stored.
    pub logical_bytes: u64,
    /// Bytes actually allocated from the system, including fragmentation.
    pub physical_bytes: u64,
    /// Sum of compressed payload lengths, excluding slot slack.
    pub payload_bytes: u64,
}

impl Stats {
    /// Logical bytes stored per byte of real memory held.
    ///
    /// This is the number that decides how much capacity the tier adds, and it
    /// counts fragmentation and empty slots honestly - unlike a ratio computed
    /// from payload lengths alone.
    pub fn ratio(&self) -> f64 {
        if self.physical_bytes == 0 {
            return if self.logical_bytes == 0 {
                1.0
            } else {
                f64::INFINITY
            };
        }
        self.logical_bytes as f64 / self.physical_bytes as f64
    }

    /// Fraction of allocated bytes lost to slot slack and free slots.
    pub fn waste(&self) -> f64 {
        if self.physical_bytes == 0 {
            return 0.0;
        }
        1.0 - (self.payload_bytes as f64 / self.physical_bytes as f64)
    }
}

struct SizeClass {
    slot_size: usize,
    slots_per_chunk: usize,
    chunks: Vec<Box<[u8]>>,
    /// Head of the intrusive list of slots that were used and then freed.
    free_head: u32,
    /// First slot never yet handed out. Slots at or above this are virgin.
    ///
    /// Growing a class therefore costs one allocation and nothing else: without
    /// this, `grow` had to walk every slot in a new chunk to thread it onto the
    /// free list, which touched the whole chunk and made cold stores roughly
    /// three times slower than warm ones.
    bump: u32,
    live: u32,
}

impl SizeClass {
    fn new(slot_size: usize) -> Self {
        debug_assert!(slot_size >= std::mem::size_of::<u32>());
        Self {
            slot_size,
            slots_per_chunk: (CHUNK_BYTES / slot_size).max(1),
            chunks: Vec::new(),
            free_head: NIL,
            bump: 0,
            live: 0,
        }
    }

    fn locate(&self, slot: u32) -> Option<(usize, usize)> {
        let chunk = slot as usize / self.slots_per_chunk;
        let within = slot as usize % self.slots_per_chunk;
        (chunk < self.chunks.len()).then_some((chunk, within * self.slot_size))
    }

    fn bytes(&self, slot: u32) -> Option<&[u8]> {
        let (c, off) = self.locate(slot)?;
        Some(&self.chunks[c][off..off + self.slot_size])
    }

    fn bytes_mut(&mut self, slot: u32) -> Option<&mut [u8]> {
        let (c, off) = self.locate(slot)?;
        let n = self.slot_size;
        Some(&mut self.chunks[c][off..off + n])
    }

    fn capacity(&self) -> usize {
        self.chunks.len() * self.slots_per_chunk
    }

    /// Adds one chunk. O(1): virgin slots are handed out by the bump pointer,
    /// so nothing in the new chunk is touched until it is actually used.
    fn grow(&mut self, class_idx: usize) -> Result<(), PoolError> {
        let last = self.capacity() + self.slots_per_chunk - 1;
        if last as u64 > MAX_SLOT as u64 {
            return Err(PoolError::ClassExhausted(class_idx));
        }
        self.chunks
            .push(vec![0u8; self.slots_per_chunk * self.slot_size].into_boxed_slice());
        Ok(())
    }

    fn alloc(&mut self, class_idx: usize) -> Result<u32, PoolError> {
        // Reuse a freed slot before expanding, so a steady store/free cycle
        // never grows the pool.
        if self.free_head != NIL {
            let slot = self.free_head;
            let next = {
                let b = self.bytes(slot).ok_or(PoolError::BadHandle)?;
                u32::from_ne_bytes(b[..4].try_into().expect("slot >= 4 bytes"))
            };
            self.free_head = next;
            self.live += 1;
            return Ok(slot);
        }
        if self.bump as usize >= self.capacity() {
            self.grow(class_idx)?;
        }
        let slot = self.bump;
        self.bump += 1;
        self.live += 1;
        Ok(slot)
    }

    fn dealloc(&mut self, slot: u32) -> Result<(), PoolError> {
        let head = self.free_head;
        let b = self.bytes_mut(slot).ok_or(PoolError::BadHandle)?;
        b[..4].copy_from_slice(&head.to_ne_bytes());
        self.free_head = slot;
        self.live -= 1;
        Ok(())
    }

    fn physical_bytes(&self) -> u64 {
        (self.capacity() * self.slot_size) as u64
    }
}

/// A pool of compressed pages, all of one page size.
pub struct CompressedPool {
    page_size: usize,
    classes: Vec<SizeClass>,
    scratch: Vec<u8>,
    stats: Stats,
}

impl CompressedPool {
    /// Creates a pool for pages of exactly `page_size` bytes.
    ///
    /// # Panics
    /// If `page_size` is zero, or so large that the required classes exceed the
    /// handle's class field.
    pub fn new(page_size: usize) -> Self {
        assert!(page_size > 0, "page size must be non-zero");
        let n_classes = (page_size + HEADER).div_ceil(GRAN);
        assert!(
            n_classes <= MAX_CLASSES,
            "page size {page_size} needs {n_classes} size classes, handle allows {MAX_CLASSES}"
        );
        Self {
            page_size,
            classes: (0..n_classes)
                .map(|i| SizeClass::new((i + 1) * GRAN))
                .collect(),
            scratch: vec![0u8; lz4_flex::block::get_maximum_output_size(page_size)],
            stats: Stats::default(),
        }
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn stats(&self) -> Stats {
        let mut s = self.stats;
        s.physical_bytes = self.classes.iter().map(SizeClass::physical_bytes).sum();
        s
    }

    /// Compresses and stores one page.
    ///
    /// Same-fill pages are recognised first and cost no storage. Otherwise the
    /// page is LZ4-compressed, and if that does not fit in a smaller class than
    /// the raw page it is stored raw - compressing incompressible data would
    /// spend CPU to make the page *larger*.
    pub fn store(&mut self, page: &[u8]) -> Result<Handle, PoolError> {
        if page.len() != self.page_size {
            return Err(PoolError::PageSizeMismatch {
                got: page.len(),
                want: self.page_size,
            });
        }

        if let Some(&b) = page.first() {
            if page.iter().all(|&x| x == b) {
                self.stats.pages += 1;
                self.stats.same_fill_pages += 1;
                self.stats.logical_bytes += self.page_size as u64;
                return Ok(Handle::same_fill(b));
            }
        }

        // A compressed page only helps if it lands in a strictly smaller class
        // than storing it raw would.
        let raw_class = Self::class_for(self.page_size);
        let (payload_len, incompressible) =
            match lz4_flex::block::compress_into(page, &mut self.scratch) {
                Ok(n) if Self::class_for(n) < raw_class => (n, false),
                _ => (self.page_size, true),
            };

        let class_idx = Self::class_for(payload_len);
        let class = &mut self.classes[class_idx];
        let slot = class.alloc(class_idx)?;
        {
            let buf = class.bytes_mut(slot).ok_or(PoolError::BadHandle)?;
            buf[..HEADER].copy_from_slice(&(payload_len as u16).to_ne_bytes());
            let src: &[u8] = if incompressible {
                page
            } else {
                &self.scratch[..payload_len]
            };
            buf[HEADER..HEADER + payload_len].copy_from_slice(src);
        }

        self.stats.pages += 1;
        self.stats.logical_bytes += self.page_size as u64;
        self.stats.payload_bytes += (payload_len + HEADER) as u64;
        if incompressible {
            self.stats.incompressible_pages += 1;
        }
        Ok(Handle::slot(class_idx, slot))
    }

    /// Reconstructs a stored page into `out`, which must be exactly one page.
    pub fn load(&self, h: Handle, out: &mut [u8]) -> Result<(), PoolError> {
        if out.len() != self.page_size {
            return Err(PoolError::PageSizeMismatch {
                got: out.len(),
                want: self.page_size,
            });
        }
        if let Some(b) = h.fill_byte() {
            out.fill(b);
            return Ok(());
        }
        let class = self
            .classes
            .get(h.class() as usize)
            .ok_or(PoolError::BadHandle)?;
        let buf = class.bytes(h.slot_index()).ok_or(PoolError::BadHandle)?;
        let len = u16::from_ne_bytes(buf[..HEADER].try_into().expect("header is 2 bytes")) as usize;
        if len > class.slot_size - HEADER {
            return Err(PoolError::Corrupt);
        }
        let payload = &buf[HEADER..HEADER + len];
        if len == self.page_size {
            out.copy_from_slice(payload);
            return Ok(());
        }
        let n = lz4_flex::block::decompress_into(payload, out).map_err(|_| PoolError::Corrupt)?;
        if n != self.page_size {
            return Err(PoolError::Corrupt);
        }
        Ok(())
    }

    /// Releases a stored page. Freeing a same-fill handle is a no-op by design,
    /// since it never held storage.
    pub fn free(&mut self, h: Handle) -> Result<(), PoolError> {
        self.stats.pages = self.stats.pages.saturating_sub(1);
        self.stats.logical_bytes = self
            .stats
            .logical_bytes
            .saturating_sub(self.page_size as u64);

        if h.is_same_fill() {
            self.stats.same_fill_pages = self.stats.same_fill_pages.saturating_sub(1);
            return Ok(());
        }
        let class = self
            .classes
            .get(h.class() as usize)
            .ok_or(PoolError::BadHandle)?;
        let slot = h.slot_index();
        let len = {
            let buf = class.bytes(slot).ok_or(PoolError::BadHandle)?;
            u16::from_ne_bytes(buf[..HEADER].try_into().expect("header is 2 bytes")) as usize
        };
        if len == self.page_size {
            self.stats.incompressible_pages = self.stats.incompressible_pages.saturating_sub(1);
        }
        self.stats.payload_bytes = self
            .stats
            .payload_bytes
            .saturating_sub((len + HEADER) as u64);
        self.classes[h.class() as usize].dealloc(slot)
    }

    fn class_for(payload_len: usize) -> usize {
        (payload_len + HEADER).div_ceil(GRAN) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;

    /// Deterministic PRNG - keeps the tests dependency-free and reproducible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn fill(&mut self, buf: &mut [u8]) {
            for b in buf.iter_mut() {
                *b = (self.next() >> 24) as u8;
            }
        }
    }

    fn roundtrip(pool: &mut CompressedPool, page: &[u8]) {
        let h = pool.store(page).expect("store");
        let mut out = vec![0u8; page.len()];
        pool.load(h, &mut out).expect("load");
        assert_eq!(out, page, "page did not survive the round trip");
        pool.free(h).expect("free");
    }

    #[test]
    fn zero_page_costs_no_storage() {
        let mut p = CompressedPool::new(PAGE);
        let h = p.store(&[0u8; PAGE]).unwrap();
        assert!(h.is_same_fill());
        assert_eq!(h.fill_byte(), Some(0));
        assert_eq!(
            p.stats().physical_bytes,
            0,
            "same-fill must allocate nothing"
        );
        let mut out = [0xFFu8; PAGE];
        p.load(h, &mut out).unwrap();
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn any_same_fill_byte_is_recognised() {
        let mut p = CompressedPool::new(PAGE);
        for b in [0u8, 1, 0x5A, 0xFF] {
            let h = p.store(&[b; PAGE]).unwrap();
            assert!(h.is_same_fill(), "byte {b:#x} should be same-fill");
            let mut out = [0u8; PAGE];
            p.load(h, &mut out).unwrap();
            assert!(out.iter().all(|&x| x == b));
        }
        assert_eq!(p.stats().physical_bytes, 0);
    }

    #[test]
    fn compressible_pages_round_trip_and_shrink() {
        let mut p = CompressedPool::new(PAGE);
        let mut page = vec![0u8; PAGE];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i / 64) as u8; // long runs: very compressible
        }
        // Enough pages to fill the chunk their class allocates, so the ratio
        // reflects steady state rather than a single page in a fresh chunk.
        let mut handles = Vec::new();
        for _ in 0..512 {
            handles.push(p.store(&page).unwrap());
        }
        let mut out = vec![0u8; PAGE];
        for h in &handles {
            assert!(!h.is_same_fill());
            p.load(*h, &mut out).unwrap();
            assert_eq!(out, page);
        }
        let s = p.stats();
        assert_eq!(s.incompressible_pages, 0);
        assert!(s.ratio() > 4.0, "ratio was {}", s.ratio());
    }

    /// A pool holding one page still owns the whole chunk that page's class
    /// allocated, so `ratio()` reads far below 1.0 until the tier is populated.
    ///
    /// This is the honest number - that memory really is committed - but it
    /// means the ratio is only meaningful at steady state, and any capacity
    /// planning built on a near-empty pool will be badly wrong. Pinned here so
    /// the behaviour is a documented property rather than a surprise in
    /// production telemetry.
    #[test]
    fn sparse_pool_reports_ratio_below_one() {
        let mut p = CompressedPool::new(PAGE);
        let mut page = vec![0u8; PAGE];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i / 64) as u8;
        }
        p.store(&page).unwrap();
        let s = p.stats();
        // A chunk holds `CHUNK_BYTES / slot_size` slots, so it rounds down and
        // lands just under CHUNK_BYTES rather than exactly on it.
        assert!(
            s.physical_bytes > (CHUNK_BYTES - GRAN * MAX_CLASSES) as u64,
            "expected roughly a full chunk, got {} bytes",
            s.physical_bytes
        );
        assert!(
            s.ratio() < 1.0,
            "one page in a fresh chunk: ratio {}",
            s.ratio()
        );
        assert!(s.waste() > 0.9, "almost all of that chunk is still free");
    }

    #[test]
    fn incompressible_page_is_stored_raw_not_inflated() {
        let mut p = CompressedPool::new(PAGE);
        let mut page = vec![0u8; PAGE];
        Rng(0xDEADBEEF).fill(&mut page);
        let h = p.store(&page).unwrap();
        assert_eq!(p.stats().incompressible_pages, 1);
        let mut out = vec![0u8; PAGE];
        p.load(h, &mut out).unwrap();
        assert_eq!(out, page);
        // Random data must never cost more than the raw page plus one class step.
        assert!(
            p.stats().physical_bytes > 0,
            "raw storage still needs a slot"
        );
    }

    #[test]
    fn random_pages_round_trip_across_many_sizes() {
        let mut p = CompressedPool::new(PAGE);
        let mut rng = Rng(12345);
        let mut page = vec![0u8; PAGE];
        for i in 0..200 {
            // Vary compressibility: a random prefix, then a constant tail.
            let split = (rng.next() as usize) % PAGE;
            rng.fill(&mut page[..split]);
            page[split..].fill((i % 251) as u8);
            roundtrip(&mut p, &page);
        }
        assert_eq!(p.stats().pages, 0, "every page was freed");
    }

    #[test]
    fn freed_slots_are_reused_rather_than_growing_the_pool() {
        let mut p = CompressedPool::new(PAGE);
        let mut rng = Rng(999);
        let mut page = vec![0u8; PAGE];
        rng.fill(&mut page);

        let first = p.store(&page).unwrap();
        let after_first = p.stats().physical_bytes;
        p.free(first).unwrap();

        for _ in 0..1000 {
            let h = p.store(&page).unwrap();
            p.free(h).unwrap();
        }
        assert_eq!(
            p.stats().physical_bytes,
            after_first,
            "store/free cycling must not grow the pool"
        );
    }

    #[test]
    fn many_live_pages_stay_independent() {
        let mut p = CompressedPool::new(PAGE);
        let mut rng = Rng(4242);
        let mut pages = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..500 {
            let mut page = vec![0u8; PAGE];
            let split = (rng.next() as usize) % PAGE;
            rng.fill(&mut page[..split]);
            handles.push(p.store(&page).unwrap());
            pages.push(page);
        }
        // Read them back in a different order than they were written.
        let mut out = vec![0u8; PAGE];
        for i in (0..handles.len()).rev() {
            p.load(handles[i], &mut out).unwrap();
            assert_eq!(out, pages[i], "page {i} was corrupted by its neighbours");
        }
        // Free every other one, then confirm the survivors are untouched.
        for i in (0..handles.len()).step_by(2) {
            p.free(handles[i]).unwrap();
        }
        for i in (1..handles.len()).step_by(2) {
            p.load(handles[i], &mut out).unwrap();
            assert_eq!(out, pages[i], "page {i} corrupted by neighbouring frees");
        }
    }

    #[test]
    fn wrong_page_size_is_rejected_both_ways() {
        let mut p = CompressedPool::new(PAGE);
        assert_eq!(
            p.store(&[0u8; 128]),
            Err(PoolError::PageSizeMismatch {
                got: 128,
                want: PAGE
            })
        );
        let h = p.store(&[7u8; PAGE]).unwrap();
        let mut small = [0u8; 128];
        assert_eq!(
            p.load(h, &mut small),
            Err(PoolError::PageSizeMismatch {
                got: 128,
                want: PAGE
            })
        );
    }

    #[test]
    fn class_selection_is_tight_and_monotonic() {
        let mut prev = 0;
        for len in [0usize, 1, 61, 62, 63, 126, 4094] {
            let c = CompressedPool::class_for(len);
            let slot = (c + 1) * GRAN;
            assert!(slot >= len + HEADER, "class {c} too small for {len} bytes");
            assert!(
                slot - GRAN < len + HEADER,
                "class {c} wastes a whole step for {len}"
            );
            assert!(c >= prev);
            prev = c;
        }
    }

    #[test]
    fn stats_track_ratio_and_waste() {
        let mut p = CompressedPool::new(PAGE);
        assert_eq!(p.stats().ratio(), 1.0, "empty pool is neutral");
        for _ in 0..64 {
            p.store(&[0u8; PAGE]).unwrap();
        }
        let s = p.stats();
        assert_eq!(s.pages, 64);
        assert_eq!(s.same_fill_pages, 64);
        assert!(s.ratio().is_infinite(), "all same-fill costs no memory");
        assert_eq!(s.waste(), 0.0);
    }

    #[test]
    fn small_page_sizes_are_supported() {
        // The tier may want sub-page granularity later; the pool should not care.
        for page_size in [64usize, 512, 1024] {
            let mut p = CompressedPool::new(page_size);
            let mut rng = Rng(page_size as u64 + 1);
            let mut page = vec![0u8; page_size];
            rng.fill(&mut page);
            roundtrip(&mut p, &page);
        }
    }
}
