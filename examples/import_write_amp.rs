use std::{env, fs, path::PathBuf, time::Instant};

use jammdb::OpenOptions;

#[derive(Clone, Copy)]
struct IoCounters {
    written_bytes: u64,
    write_syscalls: u64,
}

fn io_counters() -> IoCounters {
    let mut written_bytes = 0;
    let mut write_syscalls = 0;
    for line in fs::read_to_string("/proc/self/io")
        .expect("/proc/self/io")
        .lines()
    {
        if let Some(value) = line.strip_prefix("wchar: ") {
            written_bytes = value.parse().unwrap();
        } else if let Some(value) = line.strip_prefix("syscw: ") {
            write_syscalls = value.parse().unwrap();
        }
    }
    IoCounters {
        written_bytes,
        write_syscalls,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let count: u64 = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    let batch_size: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let value_len: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(64);
    let path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("jammdb-import-write-amp.db"));

    let _ = fs::remove_file(&path);
    let page_size = 4096_u64;
    let db = OpenOptions::new()
        .pagesize(page_size)
        .num_pages(64)
        .open(&path)?;

    let value = vec![b'x'; value_len];
    let payload_bytes = count * (8 + value_len as u64);
    let before = io_counters();
    let started = Instant::now();

    let mut inserted = 0_u64;
    while inserted < count {
        let batch_end = (inserted + batch_size as u64).min(count);
        let tx = db.tx(true)?;
        {
            let bucket = tx.get_or_create_bucket("logs")?;
            for id in inserted..batch_end {
                bucket.put(id.to_be_bytes(), value.as_slice())?;
            }
        }
        tx.commit()?;
        inserted = batch_end;
    }

    let elapsed = started.elapsed();
    let after = io_counters();
    let written_bytes = after.written_bytes.saturating_sub(before.written_bytes);
    let write_syscalls = after.write_syscalls.saturating_sub(before.write_syscalls);
    let file_bytes = fs::metadata(&path)?.len();
    let stats = db.write_stats();
    println!("records={count} batch_size={batch_size} value_len={value_len}");
    println!("payload_bytes={payload_bytes}");
    println!("written_bytes={written_bytes}");
    println!(
        "write_amplification={:.2}x",
        written_bytes as f64 / payload_bytes.max(1) as f64
    );
    println!("write_syscalls={write_syscalls}");
    println!("db_commits={}", stats.commits);
    println!("db_pages={}", stats.pages);
    println!("db_write_requests={}", stats.write_requests);
    println!("db_whole_page_bytes={}", stats.whole_page_bytes);
    println!("db_written_bytes={}", stats.written_bytes);
    println!(
        "db_write_amplification={:.2}x",
        stats.written_bytes as f64 / payload_bytes.max(1) as f64
    );
    println!("file_bytes={file_bytes}");
    println!("elapsed_seconds={:.3}", elapsed.as_secs_f64());
    println!("path={}", path.display());
    Ok(())
}
