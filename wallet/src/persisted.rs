use std::fs;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose;
use bdk_wallet::bitcoin::hex::DisplayHex as _;
use bdk_wallet::{ChangeSet, WalletPersister};
use rand::RngCore as _;
use rusqlite::{Connection, OptionalExtension as _, named_params};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::bmp_wallet::ImportedKey;

#[cfg(any(test, feature = "test-utils"))]
static MEMORY_SALT_STORE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
> = std::sync::LazyLock::new(Default::default);

/// Where a wallet's `SQLite` database lives.
/// It could either leaves in memory or in a file
/// The Memory variant is reserve for unit-tests only.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DBStorage {
    File(PathBuf),
    #[cfg(any(test, feature = "test-utils"))]
    Memory(String),
}

pub type Result<T, E = PersistenceError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PersistenceError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("salt decode error: {0}")]
    SaltDecode(#[from] base64::DecodeError),
    #[error("no committed salt path for database {database}")]
    MissingSaltPath { database: String },
    #[error("no salt registered for in-memory database {name}")]
    MissingMemorySalt { name: String },
    #[error("no staged salt is available to commit")]
    MissingStagedSalt,
}

impl DBStorage {
    /// Build the storage for sub-wallets
    #[must_use]
    pub fn sibling(&self, file_name: &str) -> Self {
        match self {
            Self::File(path) => {
                tracing::debug!("Ignoring argument {file_name}");
                Self::File(path.clone())
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => Self::Memory(format!("{name}_{file_name}")),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn open_uri(&self) -> String {
        match self {
            Self::File(path) => path.to_string_lossy().into_owned(),
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => format!("file:{name}?mode=memory&cache=shared"),
        }
    }

    pub fn db_exists(&self, db_name: &str) -> bool {
        match self {
            Self::File(path) => {
                let path = path.join(db_name);
                path.exists()
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(_) => false,
        }
    }

    pub fn open(&self, db_name: &str) -> rusqlite::Result<Connection> {
        match self {
            Self::File(path) => {
                let path = path.join(db_name);
                Connection::open(path)
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(_) => Connection::open_with_flags(
                self.open_uri(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            ),
        }
    }

    /// Opens the database of an imported key. A database file is encrypted with the raw
    /// `SQLCipher` key `key` (64 hex digits). An in-memory database of a test is not encrypted.
    pub fn open_with_raw_key(&self, db_name: &str, key: &str) -> rusqlite::Result<Connection> {
        let db = self.open(db_name)?;
        if self.base_path().is_some() {
            // The form x'..' passes the key as it is, without the slow key derivation that a
            // passphrase needs. The databases of imported keys are opened for every write.
            db.pragma_update(None, "key", format!("x'{key}'"))?;
        }
        Ok(db)
    }

    pub fn base_path(&self) -> Option<PathBuf> {
        match self {
            Self::File(p) => Some(p.clone()),
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(_) => None,
        }
    }

    //// Persist and return the persisted salt
    pub fn persist_salt(&self, db_name: &str) -> Result<Vec<u8>> {
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        match self {
            Self::File(_) => {
                let full_p = self
                    .committed_salt_path(db_name)
                    .expect("file-backed storage should have a committed salt path");
                fs::write(full_p, general_purpose::STANDARD.encode(salt))?;
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => {
                MEMORY_SALT_STORE
                    .lock()
                    .unwrap()
                    .entry(name.clone())
                    .insert_entry(salt.to_vec());
            }
        }
        Ok(salt.to_vec())
    }

    pub fn load_salt(&self, db_name: &str) -> Result<Vec<u8>> {
        match self {
            Self::File(_) => {
                let salt_path = self.committed_salt_path(db_name).ok_or_else(|| {
                    PersistenceError::MissingSaltPath {
                        database: db_name.to_owned(),
                    }
                })?;
                let salt_str = fs::read_to_string(&salt_path)?;
                Ok(general_purpose::STANDARD.decode(salt_str.as_bytes())?)
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => MEMORY_SALT_STORE
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or_else(|| PersistenceError::MissingMemorySalt { name: name.clone() }),
        }
    }

    /// Path to the staged salt file (`<db>.salt.new`) for file-backed storage.
    pub fn staged_salt_path(&self, db_name: &str) -> Option<PathBuf> {
        self.base_path()
            .map(|p| p.join(format!("{db_name}.salt.new")))
    }

    /// Path to the committed salt file (`<db>.salt`) for file-backed storage.
    pub fn committed_salt_path(&self, db_name: &str) -> Option<PathBuf> {
        self.base_path().map(|p| p.join(format!("{db_name}.salt")))
    }

    /// Write a staged salt
    pub fn write_staged_salt(&self, db_name: &str, salt: &[u8]) -> Result<()> {
        match self {
            Self::File(_) => {
                let staged = self
                    .staged_salt_path(db_name)
                    .expect("file-backed storage should have a staged salt path");
                let mut staged_salt_file = fs::File::create(&staged)?;
                staged_salt_file.write_all(general_purpose::STANDARD.encode(salt).as_bytes())?;
                staged_salt_file.sync_all()?;
                Ok(())
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => {
                let mut map = MEMORY_SALT_STORE.lock().unwrap();
                map.insert(format!("{name}.salt.new"), salt.to_vec());
                Ok(())
            }
        }
    }

    /// Read the staged salt if present. Returns `None` when there is no staged salt.
    pub fn read_staged_salt(&self, db_name: &str) -> Option<Vec<u8>> {
        match self {
            Self::File(_) => self.staged_salt_path(db_name).and_then(|staged| {
                fs::read_to_string(staged)
                    .ok()
                    .and_then(|s| general_purpose::STANDARD.decode(s.as_bytes()).ok())
            }),
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => MEMORY_SALT_STORE
                .lock()
                .unwrap()
                .get(&format!("{name}.salt.new"))
                .cloned(),
        }
    }

    pub fn remove_staged_salt(&self, db_name: &str) {
        match self {
            Self::File(_) => {
                let staged = self
                    .staged_salt_path(db_name)
                    .expect("file-backed storage should have a staged salt path");
                let _ = fs::remove_file(staged);
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => {
                let mut map = MEMORY_SALT_STORE.lock().unwrap();
                map.remove(&format!("{name}.salt.new"));
            }
        }
    }

    /// Commit a staged salt rename `<db>.salt.new` to `<db>.salt`
    pub fn commit_staged_salt(&self, db_name: &str) -> Result<()> {
        match self {
            Self::File(_) => {
                let staged = self
                    .staged_salt_path(db_name)
                    .expect("file-backed storage should have a staged salt path");
                let committed = self
                    .committed_salt_path(db_name)
                    .expect("file-backed storage should have a committed salt path");
                fs::rename(staged, committed)?;
                Ok(())
            }
            #[cfg(any(test, feature = "test-utils"))]
            Self::Memory(name) => {
                let mut map = MEMORY_SALT_STORE.lock().unwrap();
                if let Some(s) = map.remove(&format!("{name}.salt.new")) {
                    map.insert(name.clone(), s);
                    Ok(())
                } else {
                    Err(PersistenceError::MissingStagedSalt)
                }
            }
        }
    }
}

impl From<&Path> for DBStorage {
    fn from(p: &Path) -> Self {
        Self::File(p.to_path_buf())
    }
}

pub struct BMPDatabase<C: BMPWalletPersister> {
    storage: DBStorage,
    conn: C,
}

impl<C: BMPWalletPersister> BMPDatabase<C> {
    pub const fn new(storage: DBStorage, conn: C) -> Self {
        Self { storage, conn }
    }

    pub const fn storage(&self) -> &DBStorage {
        &self.storage
    }
}

impl<C: BMPWalletPersister> Deref for BMPDatabase<C> {
    type Target = C;
    fn deref(&self) -> &C {
        &self.conn
    }
}

impl<C: BMPWalletPersister> DerefMut for BMPDatabase<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.conn
    }
}

pub trait BMPWalletPersister: WalletPersister {
    type DB;

    fn new(db_location: DBStorage, db_name: &str) -> Result<Self::DB>;

    fn init(
        db: &mut Self::DB,
        imported_keys_table: Option<&str>,
        seeds_table_name: Option<&str>,
    ) -> Result<()>;

    fn persist_seed_phrase(
        db: &mut Self::DB,
        seeds_table_name: &str,
        seed_phrase: &str,
    ) -> Result<()>;

    fn load_imported_key_rows(
        db: &mut Self::DB,
        keys_table_name: &str,
    ) -> Result<Vec<(String, String)>>;

    fn persist_imported_keys(
        db: &mut Self::DB,
        keys_table_name: &str,
        keys: &[&ImportedKey],
    ) -> Result<()>;

    fn get_seed_phrase(db: &Self::DB, seeds_table_name: &str) -> Result<String>;

    fn has_seed_phrase(db: &Self::DB, seeds_table_name: &str) -> Result<bool>;

    fn persist_staged_changes(db: &mut Self::DB, cs: &ChangeSet) -> Result<()>;

    /// Returns the key that encrypts the databases of the imported keys: 64 hex digits. The key
    /// is random, it is created on first use, and it is stored in this (encrypted) database.
    fn load_or_create_imported_db_key(db: &mut Self::DB) -> Result<Zeroizing<String>>;

    /// Records that the key at `index` of the external keychain was issued to a trade.
    fn persist_trade_key_index(db: &mut Self::DB, table_name: &str, index: u32) -> Result<()>;

    /// The indexes of the external keychain whose keys were issued to trades.
    fn load_trade_key_indexes(db: &mut Self::DB, table_name: &str) -> Result<Vec<u32>>;
}

impl BMPWalletPersister for Connection {
    type DB = Self;

    fn new(db_location: DBStorage, db_name: &str) -> Result<Self::DB> {
        Ok(db_location.open(db_name)?)
    }

    fn persist_staged_changes(db: &mut Self::DB, cs: &ChangeSet) -> Result<()> {
        Ok(Self::persist(db, cs)?)
    }

    fn init(
        db: &mut Self::DB,
        imported_keys_table: Option<&str>,
        seeds_table_name: Option<&str>,
    ) -> Result<()> {
        let create_imported_keys_table = format!(
            "CREATE TABLE {} ( \
                    key TEXT PRIMARY KEY NOT NULL,
                    descriptor TEXT NOT NULL
                ) STRICT",
            imported_keys_table.unwrap(),
        );

        let create_seeds_table = format!(
            "CREATE TABLE {} ( \
                    seed TEXT PRIMARY KEY NOT NULL
                ) STRICT",
            seeds_table_name.unwrap(),
        );

        let query = format!("{create_imported_keys_table}; {create_seeds_table}");

        let trx = db.transaction()?;

        trx.execute_batch(&query)?;
        trx.commit()?;
        Ok(())
    }

    fn persist_seed_phrase(
        db: &mut Self::DB,
        seeds_table_name: &str,
        seed_phrase: &str,
    ) -> Result<()> {
        let trx = db.transaction()?;
        {
            let mut stmt = trx.prepare(&format!(
                "INSERT INTO {seeds_table_name}(seed) VALUES(:seed)"
            ))?;

            stmt.execute(named_params! {
                ":seed": seed_phrase
            })?;
        }

        trx.commit()?;
        Ok(())
    }

    fn load_imported_key_rows(
        db: &mut Self::DB,
        keys_table_name: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut imported_key_rows = vec![];

        let mut statement =
            db.prepare(&format!("SELECT key, descriptor FROM {keys_table_name}"))?;

        let row_iter = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>("key")?,
                row.get::<_, String>("descriptor")?,
            ))
        })?;

        for row in row_iter {
            imported_key_rows.push(row?);
        }

        Ok(imported_key_rows)
    }

    fn persist_imported_keys(
        db: &mut Self::DB,
        keys_table_name: &str,
        keys: &[&ImportedKey],
    ) -> Result<()> {
        let db_trx = db.transaction()?;
        {
            let mut statement = db_trx.prepare_cached(&format!(
                "INSERT OR IGNORE INTO {keys_table_name} (key, descriptor) \
                 VALUES (:key, :descriptor)"
            ))?;

            for key in keys {
                statement.execute(named_params! {
                    ":key": key.secret().serialize().to_lower_hex_string(),
                    ":descriptor": key.descriptor().to_string(),
                })?;
            }
        }

        db_trx.commit()?;
        Ok(())
    }

    fn get_seed_phrase(db: &Self::DB, seeds_table_name: &str) -> Result<String> {
        let mnemonic =
            db.query_row(&format!("SELECT seed FROM {seeds_table_name}"), (), |row| {
                row.get::<_, String>("seed")
            })?;

        Ok(mnemonic)
    }

    fn has_seed_phrase(db: &Self::DB, seeds_table_name: &str) -> Result<bool> {
        let mut stmt = db.prepare(&format!("SELECT 1 FROM {seeds_table_name}"))?;
        Ok(stmt.exists([])?)
    }

    fn load_or_create_imported_db_key(db: &mut Self::DB) -> Result<Zeroizing<String>> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS bmp_settings ( \
                name TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            ) STRICT",
        )?;
        let stored = db
            .query_row(
                "SELECT value FROM bmp_settings WHERE name = 'imported_db_key'",
                (),
                |row| row.get::<_, String>("value"),
            )
            .optional()?;
        if let Some(key) = stored {
            return Ok(Zeroizing::new(key));
        }
        let mut bytes = Zeroizing::new([0u8; 32]);
        rand::rng().fill_bytes(&mut bytes[..]);
        let key = Zeroizing::new(bytes.to_lower_hex_string());
        db.execute(
            "INSERT INTO bmp_settings (name, value) VALUES ('imported_db_key', :key)",
            named_params! { ":key": key.as_str() },
        )?;
        Ok(key)
    }

    fn persist_trade_key_index(db: &mut Self::DB, table_name: &str, index: u32) -> Result<()> {
        create_trade_keys_table(db, table_name)?;
        db.execute(
            &format!("INSERT OR IGNORE INTO {table_name} (keychain_index) VALUES (:index)"),
            named_params! { ":index": index },
        )?;
        Ok(())
    }

    fn load_trade_key_indexes(db: &mut Self::DB, table_name: &str) -> Result<Vec<u32>> {
        create_trade_keys_table(db, table_name)?;
        let mut statement = db.prepare(&format!(
            "SELECT keychain_index FROM {table_name} ORDER BY keychain_index"
        ))?;
        let indexes = statement
            .query_map([], |row| row.get::<_, u32>("keychain_index"))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(indexes)
    }
}

/// The table of the trade keys is created on first use, so that a wallet database that was
/// created before the table existed can be used without a migration step.
fn create_trade_keys_table(db: &Connection, table_name: &str) -> rusqlite::Result<()> {
    db.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table_name} ( \
            keychain_index INTEGER PRIMARY KEY NOT NULL
        ) STRICT"
    ))
}

/// The table of the trade records. The trade engine stores one record per trade in the
/// database of the wallet, which is encrypted. The table is created on first use, so that a
/// wallet database that was created before the table existed can be used without a migration
/// step.
const TRADE_RECORDS_TABLE_NAME: &str = "bmp_trade_records";

fn create_trade_records_table(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {TRADE_RECORDS_TABLE_NAME} ( \
            trade_id TEXT PRIMARY KEY NOT NULL, \
            record BLOB NOT NULL
        ) STRICT"
    ))
}

/// Stores the record of a trade, in place of an earlier record of the same trade.
pub(crate) fn store_trade_record(
    db: &Connection,
    trade_id: &str,
    record: &[u8],
) -> rusqlite::Result<()> {
    create_trade_records_table(db)?;
    db.execute(
        &format!(
            "INSERT INTO {TRADE_RECORDS_TABLE_NAME} (trade_id, record) VALUES (:trade_id, :record) \
             ON CONFLICT (trade_id) DO UPDATE SET record = excluded.record"
        ),
        named_params! { ":trade_id": trade_id, ":record": record },
    )?;
    Ok(())
}

/// All trade records, in the order of their trade IDs.
pub(crate) fn load_trade_records(db: &Connection) -> rusqlite::Result<Vec<(String, Vec<u8>)>> {
    create_trade_records_table(db)?;
    let mut statement = db.prepare(&format!(
        "SELECT trade_id, record FROM {TRADE_RECORDS_TABLE_NAME} ORDER BY trade_id"
    ))?;
    statement
        .query_map([], |row| Ok((row.get("trade_id")?, row.get("record")?)))?
        .collect()
}
