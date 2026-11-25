use jammdb::{Error, OpenOptions};

mod common;

#[test]
fn write_stats_track_coalesced_commit_bytes() -> Result<(), Error> {
    let random_file = common::RandomFile::new();
    let page_size = 4096_u64;
    let db = OpenOptions::new()
        .pagesize(page_size)
        .strict_mode(true)
        .open(&random_file.path)?;

    let value = [b'x'; 64];
    let tx = db.tx(true)?;
    let bucket = tx.create_bucket("logs")?;
    for id in 0..128_u64 {
        bucket.put(id.to_be_bytes(), value.as_slice())?;
    }
    tx.commit()?;

    let stats = db.write_stats();
    assert_eq!(stats.commits, 1);
    assert!(stats.pages > 0);
    assert!(stats.write_requests > 0);
    assert!(stats.written_bytes > 0);
    assert!(stats.written_bytes < stats.whole_page_bytes);
    assert!(stats.whole_page_bytes >= (stats.pages + 1) * page_size);

    let tx = db.tx(false)?;
    let bucket = tx.get_bucket("logs")?;
    for id in 0..128_u64 {
        let kv = bucket.get(id.to_be_bytes()).expect("missing key");
        assert_eq!(kv.kv().value(), value);
    }

    Ok(())
}

#[test]
fn repeated_update_uses_small_coalesced_writes() -> Result<(), Error> {
    let random_file = common::RandomFile::new();
    let page_size = 4096_u64;
    let db = OpenOptions::new()
        .pagesize(page_size)
        .strict_mode(true)
        .open(&random_file.path)?;

    let seed_value = vec![b'a'; 64];
    {
        let tx = db.tx(true)?;
        let bucket = tx.create_bucket("logs")?;
        for id in 0..128_u64 {
            bucket.put(id.to_be_bytes(), seed_value.as_slice())?;
        }
        tx.commit()?;
    }

    let before = db.write_stats();
    let mut update_value = vec![b'y'; 64];
    let updates = 50_u64;
    for version in 0..updates {
        update_value[0] = (version % 26) as u8 + b'a';
        let tx = db.tx(true)?;
        let bucket = tx.get_bucket("logs")?;
        bucket.put(64_u64.to_be_bytes(), update_value.as_slice())?;
        tx.commit()?;
    }
    let after = db.write_stats();

    let written_bytes = after.written_bytes - before.written_bytes;
    let write_requests = after.write_requests - before.write_requests;
    let whole_page_bytes = after.whole_page_bytes - before.whole_page_bytes;

    assert!(written_bytes < updates * 512);
    assert!(write_requests <= updates * 4);
    assert!(whole_page_bytes >= updates * page_size);

    let tx = db.tx(false)?;
    let bucket = tx.get_bucket("logs")?;
    let kv = bucket.get(64_u64.to_be_bytes()).expect("missing key");
    assert_eq!(kv.kv().value(), update_value.as_slice());

    Ok(())
}
