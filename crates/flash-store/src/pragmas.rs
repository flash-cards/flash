//! Per-connection SQLite configuration, tuned for a small VPS (the sample
//! flash.service caps the process at 1 GiB).

use rusqlite::Connection;

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
    // WAL survives in the DB file; the rest are per-connection.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "cache_size", -65536)?; // 64 MB page cache
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // 256 MB is SQLite's own documented starting point (sqlite.org/mmap.html):
    // read-path speedup; the trade-off is that a disk I/O error surfaces as a
    // crash, which a supervisor's restart policy and regular backups cover.
    conn.pragma_update(None, "mmap_size", 268_435_456)?;
    // SQLite's default checkpoint cadence. A WAL replicator, if you run
    // one beside the server, takes checkpointing over on its own terms.
    conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
    Ok(())
}
