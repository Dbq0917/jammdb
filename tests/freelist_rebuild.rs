mod common;

use jammdb::{Error, OpenOptions};

#[test]
fn rebuilds_large_freelist_after_small_commits() -> Result<(), Error> {
    let random_file = common::RandomFile::new();
    let db = OpenOptions::new()
        .pagesize(1024)
        .num_pages(64)
        .strict_mode(true)
        .open(&random_file)?;

    {
        let tx = db.tx(true)?;
        let bucket = tx.create_bucket("logs")?;
        for i in 0..6_000u32 {
            bucket.put(i.to_be_bytes(), vec![b'x'; 120])?;
        }
        tx.commit()?;
    }

    {
        let tx = db.tx(true)?;
        tx.delete_bucket("logs")?;
        tx.commit()?;
    }
    db.check()?;

    let size_after_purge = random_file.path.metadata()?.len();
    for round in 0..12u32 {
        let tx = db.tx(true)?;
        let bucket = tx.get_or_create_bucket("small")?;
        for offset in 0..12u32 {
            let key = round * 100 + offset;
            bucket.put(key.to_be_bytes(), vec![b'y'; 80])?;
        }
        tx.commit()?;
        db.check()?;

        assert_eq!(random_file.path.metadata()?.len(), size_after_purge);
    }

    drop(db);

    let db = OpenOptions::new()
        .pagesize(1024)
        .strict_mode(true)
        .open(&random_file)?;
    db.check()?;

    for round in 12..24u32 {
        let tx = db.tx(true)?;
        let bucket = tx.get_bucket("small")?;
        for offset in 0..12u32 {
            let key = round * 100 + offset;
            bucket.put(key.to_be_bytes(), vec![b'z'; 64])?;
        }
        tx.commit()?;
        db.check()?;
        assert_eq!(random_file.path.metadata()?.len(), size_after_purge);
    }

    drop(db);
    let db = OpenOptions::new()
        .pagesize(1024)
        .strict_mode(true)
        .open(&random_file)?;
    db.check()?;

    let tx = db.tx(false)?;
    let bucket = tx.get_bucket("small")?;
    assert_eq!(bucket.cursor().count(), 288);
    drop(tx);

    Ok(())
}
