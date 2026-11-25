use std::{env, path::PathBuf};

use jammdb::OpenOptions;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let iterations: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let key_count: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("jammdb-update-write-amp.db"));

    let _ = std::fs::remove_file(&path);
    let db = OpenOptions::new().pagesize(4096).open(&path)?;
    let initial_value = vec![b'a'; 64];
    {
        let tx = db.tx(true)?;
        let bucket = tx.create_bucket("logs")?;
        for id in 0..key_count {
            bucket.put(id.to_be_bytes(), initial_value.as_slice())?;
        }
        tx.commit()?;
    }

    let before = db.write_stats();
    let mut update_value = vec![b'y'; 64];
    for version in 0..iterations {
        update_value[0] = (version % 26) as u8 + b'a';
        let tx = db.tx(true)?;
        let bucket = tx.get_bucket("logs")?;
        bucket.put((key_count / 2).to_be_bytes(), update_value.as_slice())?;
        tx.commit()?;
    }
    let after = db.write_stats();

    let commits = after.commits - before.commits;
    let bytes = after.written_bytes - before.written_bytes;
    let whole_page_bytes = after.whole_page_bytes - before.whole_page_bytes;
    let requests = after.write_requests - before.write_requests;

    println!("iterations={iterations}");
    println!("written_bytes={bytes}");
    println!("written_per_commit={}", bytes / commits.max(1));
    println!("whole_page_bytes={whole_page_bytes}");
    println!(
        "whole_page_per_commit={}",
        whole_page_bytes / commits.max(1)
    );
    println!("write_requests={requests}");
    println!(
        "write_requests_per_commit={}",
        requests as f64 / commits.max(1) as f64
    );
    println!(
        "reduction_vs_whole_pages={:.1}x",
        whole_page_bytes as f64 / bytes.max(1) as f64
    );
    println!("path={}", path.display());
    Ok(())
}
