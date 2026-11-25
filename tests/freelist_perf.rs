use jammdb::{OpenOptions, DB};

mod common;

// Regression test for the freelist allocation hot path.
//
// The old freelist stored every free page individually in a BTreeSet and
// `allocate` walked the whole set on every call. With a few hundred thousand
// stable free pages, the hundreds of allocations inside a single write
// transaction turned into hundreds of full scans, cutting write throughput to
// a third of its baseline.
//
// The freelist now keeps free space as contiguous spans indexed both by
// starting page and by span length, so finding a run long enough is a
// logarithmic lookup regardless of how many pages are free. This test drives
// that path through the real database on top of ~200k free pages.
#[test]
fn batch_allocations_over_large_freelist() {
    let random_file = common::RandomFile::new();

    // Phase 1: grow the database past 200k pages by storing large values,
    // then delete almost all of it across many transactions so the freelist
    // accumulates roughly 200k free pages.
    {
        let db = OpenOptions::new()
            .pagesize(4096)
            .open(&random_file.path)
            .unwrap();
        {
            let tx = db.tx(true).unwrap();
            let bucket = tx.create_bucket("bulk").unwrap();
            // each value spans ~6 pages; 32k keys grow the file to ~209k pages
            for i in 0..32_000u64 {
                bucket.put(i.to_be_bytes(), vec![7u8; 4096 * 6]).unwrap();
            }
            tx.commit().unwrap();
        }

        // delete the data in chunks across several transactions; the freed
        // pages accumulate (and coalesce into spans) in the freelist
        let chunk = 1_600usize;
        // keep one key so the bucket is never emptied (emptying a bucket is
        // an unrelated pre-existing edge case in the rebalancer)
        for start in (1..32_000usize).step_by(chunk) {
            let tx = db.tx(true).unwrap();
            let bucket = tx.get_bucket("bulk").unwrap();
            for i in start..(start + chunk).min(32_000) {
                bucket.delete((i as u64).to_be_bytes()).unwrap();
            }
            tx.commit().unwrap();
        }

        let tx = db.tx(false).unwrap();
        let bucket = tx.get_bucket("bulk").unwrap();
        let remaining = bucket.cursor().count();
        assert_eq!(remaining, 1, "only the retained sentinel key should remain");
        let file_size = std::fs::metadata(&random_file.path).unwrap().len();
        assert!(
            file_size >= 200_000 * 4096,
            "database should span at least 200k pages, got {} bytes",
            file_size
        );
    }

    // Phase 2: reopen the persisted database so the freelist is rebuilt from
    // the on-disk page, then run the batch-allocation workload several times.
    // Each write transaction performs hundreds of page allocations; with the
    // old linear scan each call cost O(free_pages), here every lookup must be
    // logarithmic in the number of free spans.
    let db = DB::open(&random_file.path).unwrap();
    for round in 0..3 {
        {
            let tx = db.tx(true).unwrap();
            let bucket = tx.get_bucket("bulk").unwrap();
            for i in 0..500u64 {
                let key = (1_000_000 + round * 1000 + i).to_be_bytes();
                bucket.put(key, vec![0u8; 128]).unwrap();
            }
            tx.commit().unwrap();
        }
    }

    // the reused pages must still hold correct, readable data
    let tx = db.tx(false).unwrap();
    let bucket = tx.get_bucket("bulk").unwrap();
    for round in 0..3u64 {
        for i in 0..500u64 {
            let key = (1_000_000 + round * 1000 + i).to_be_bytes();
            let data = bucket.get(key).expect("reused page lost its data");
            assert_eq!(data.kv().value().len(), 128);
        }
    }
}
