use rusqlite::{Connection, params};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread,
};
use tracing::{error, warn};

#[derive(Debug, Clone)]
pub struct PersistedAffinity {
    pub key_hash: [u8; 32],
    pub source: u8,
    pub backend_id: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub absolute_expires_at: i64,
    pub assignment_generation: u64,
    pub failure_count: u32,
}

enum Command {
    Upsert(PersistedAffinity),
    Delete([u8; 32]),
    DeleteExpired(i64),
}

#[derive(Clone)]
pub struct Persistence {
    sender: SyncSender<Command>,
    healthy: Arc<AtomicBool>,
}

impl Persistence {
    pub fn open(path: &Path) -> anyhow::Result<(Self, Vec<PersistedAffinity>)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = open_connection(path)?;
        initialize(&connection)?;
        let entries = load_entries(&connection)?;
        drop(connection);

        let (sender, receiver) = sync_channel::<Command>(1024);
        let healthy = Arc::new(AtomicBool::new(true));
        let worker_health = healthy.clone();
        let worker_path = path.to_path_buf();
        thread::Builder::new()
            .name("affinity-sqlite-writer".into())
            .spawn(move || {
                let connection = match open_connection(&worker_path).and_then(|connection| {
                    initialize(&connection)?;
                    Ok(connection)
                }) {
                    Ok(connection) => connection,
                    Err(error) => {
                        worker_health.store(false, Ordering::Relaxed);
                        error!(error = %error, "affinity persistence writer failed to start");
                        return;
                    }
                };
                while let Ok(command) = receiver.recv() {
                    if let Err(error) = apply(&connection, command) {
                        worker_health.store(false, Ordering::Relaxed);
                        error!(error = %error, "affinity persistence write failed");
                    } else {
                        worker_health.store(true, Ordering::Relaxed);
                    }
                }
            })?;

        Ok((Self { sender, healthy }, entries))
    }

    pub fn upsert(&self, entry: PersistedAffinity) {
        self.try_send(Command::Upsert(entry));
    }

    pub fn delete(&self, key_hash: [u8; 32]) {
        self.try_send(Command::Delete(key_hash));
    }

    pub fn delete_expired(&self, now: i64) {
        self.try_send(Command::DeleteExpired(now));
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    fn try_send(&self, command: Command) {
        match self.sender.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.healthy.store(false, Ordering::Relaxed);
                warn!("affinity persistence queue is full");
            }
            Err(TrySendError::Disconnected(_)) => {
                self.healthy.store(false, Ordering::Relaxed);
                error!("affinity persistence writer is unavailable");
            }
        }
    }
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    Ok(Connection::open(path)?)
}

fn initialize(connection: &Connection) -> anyhow::Result<()> {
    connection.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS affinity (
            key_hash BLOB PRIMARY KEY,
            source INTEGER NOT NULL,
            backend_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            absolute_expires_at INTEGER NOT NULL,
            assignment_generation INTEGER NOT NULL,
            failure_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS affinity_expires_at_idx
            ON affinity(expires_at);
        ",
    )?;
    Ok(())
}

fn load_entries(connection: &Connection) -> anyhow::Result<Vec<PersistedAffinity>> {
    let now = unix_now();
    let mut statement = connection.prepare(
        "SELECT key_hash, source, backend_id, created_at, last_seen_at, expires_at,
                absolute_expires_at, assignment_generation, failure_count
         FROM affinity
         WHERE expires_at > ?1 AND absolute_expires_at > ?1",
    )?;
    let rows = statement.query_map([now], |row| {
        let bytes: Vec<u8> = row.get(0)?;
        let key_hash: [u8; 32] = bytes.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                32,
                rusqlite::types::Type::Blob,
                "invalid affinity key length".into(),
            )
        })?;
        Ok(PersistedAffinity {
            key_hash,
            source: row.get(1)?,
            backend_id: row.get(2)?,
            created_at: row.get(3)?,
            last_seen_at: row.get(4)?,
            expires_at: row.get(5)?,
            absolute_expires_at: row.get(6)?,
            assignment_generation: row.get::<_, i64>(7)? as u64,
            failure_count: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn apply(connection: &Connection, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Upsert(entry) => {
            connection.execute(
                "INSERT INTO affinity (
                    key_hash, source, backend_id, created_at, last_seen_at, expires_at,
                    absolute_expires_at, assignment_generation, failure_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(key_hash) DO UPDATE SET
                    source=excluded.source,
                    backend_id=excluded.backend_id,
                    last_seen_at=excluded.last_seen_at,
                    expires_at=excluded.expires_at,
                    absolute_expires_at=excluded.absolute_expires_at,
                    assignment_generation=excluded.assignment_generation,
                    failure_count=excluded.failure_count",
                params![
                    entry.key_hash.as_slice(),
                    entry.source,
                    entry.backend_id,
                    entry.created_at,
                    entry.last_seen_at,
                    entry.expires_at,
                    entry.absolute_expires_at,
                    entry.assignment_generation as i64,
                    entry.failure_count,
                ],
            )?;
        }
        Command::Delete(key_hash) => {
            connection.execute(
                "DELETE FROM affinity WHERE key_hash = ?1",
                [key_hash.as_slice()],
            )?;
        }
        Command::DeleteExpired(now) => {
            connection.execute(
                "DELETE FROM affinity WHERE expires_at <= ?1 OR absolute_expires_at <= ?1",
                [now],
            )?;
        }
    }
    Ok(())
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_round_trip_restores_unexpired_entry() {
        let path = std::env::temp_dir().join(format!(
            "ds4-smart-proxy-affinity-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        let connection = open_connection(&path).unwrap();
        initialize(&connection).unwrap();
        let now = unix_now();
        let entry = PersistedAffinity {
            key_hash: [7; 32],
            source: 1,
            backend_id: "local".into(),
            created_at: now,
            last_seen_at: now,
            expires_at: now + 60,
            absolute_expires_at: now + 120,
            assignment_generation: 1,
            failure_count: 0,
        };
        apply(&connection, Command::Upsert(entry)).unwrap();
        drop(connection);

        let reopened = open_connection(&path).unwrap();
        let loaded = load_entries(&reopened).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].backend_id, "local");
        drop(reopened);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }
}
