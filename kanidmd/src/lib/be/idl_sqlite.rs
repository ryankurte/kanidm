use crate::audit::AuditScope;
use crate::be::{IdEntry, IDL};
use crate::utils::SID;
use crate::value::IndexType;
use idlset::IDLBitRange;
use kanidm_proto::v1::OperationError;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::ToSql;
use rusqlite::OptionalExtension;
use rusqlite::NO_PARAMS;
use std::convert::TryFrom;

// use uuid::Uuid;

static DBV_ID2ENTRY: &'static str = "id2entry";
static DBV_INDEXV: &'static str = "indexv";

#[derive(Clone)]
pub struct IdlSqlite {
    pool: Pool<SqliteConnectionManager>,
}

pub struct IdlSqliteReadTransaction {
    committed: bool,
    conn: r2d2::PooledConnection<SqliteConnectionManager>,
}

pub struct IdlSqliteWriteTransaction {
    committed: bool,
    conn: r2d2::PooledConnection<SqliteConnectionManager>,
}

pub trait IdlSqliteTransaction {
    fn get_conn(&self) -> &r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>;

    fn get_identry(&self, au: &mut AuditScope, idl: &IDL) -> Result<Vec<IdEntry>, OperationError> {
        // is the idl allids?
        match idl {
            IDL::ALLIDS => {
                let mut stmt = try_audit!(
                    au,
                    self.get_conn().prepare("SELECT id, data FROM id2entry"),
                    "SQLite Error {:?}",
                    OperationError::SQLiteError
                );
                let id2entry_iter = try_audit!(
                    au,
                    stmt.query_map(NO_PARAMS, |row| Ok(IdEntry {
                        id: row.get(0)?,
                        data: row.get(1)?,
                    })),
                    "SQLite Error {:?}",
                    OperationError::SQLiteError
                );
                id2entry_iter
                    .map(|v| {
                        v.map_err(|e| {
                            audit_log!(au, "SQLite Error {:?}", e);
                            OperationError::SQLiteError
                        })
                    })
                    .collect()
            }
            IDL::Partial(idli) | IDL::Indexed(idli) => {
                let mut stmt = try_audit!(
                    au,
                    self.get_conn()
                        .prepare("SELECT id, data FROM id2entry WHERE id = :idl"),
                    "SQLite Error {:?}",
                    OperationError::SQLiteError
                );

                // TODO: I have no idea how to make this an iterator chain ... so what
                // I have now is probably really bad :(
                let mut results = Vec::new();

                for id in idli {
                    let iid = i64::try_from(id).map_err(|_| OperationError::InvalidEntryID)?;
                    let id2entry_iter = stmt
                        .query_map(&[&iid], |row| {
                            Ok(IdEntry {
                                id: row.get(0)?,
                                data: row.get(1)?,
                            })
                        })
                        .map_err(|e| {
                            audit_log!(au, "SQLite Error {:?}", e);
                            OperationError::SQLiteError
                        })?;

                    let r: Result<Vec<_>, _> = id2entry_iter
                        .map(|v| {
                            v.map_err(|e| {
                                audit_log!(au, "SQLite Error {:?}", e);
                                OperationError::SQLiteError
                            })
                        })
                        .collect();
                    let mut r = r?;
                    results.append(&mut r);
                }
                Ok(results)
            }
        }
    }

    fn exists_idx(
        &self,
        audit: &mut AuditScope,
        attr: &String,
        itype: &IndexType,
    ) -> Result<bool, OperationError> {
        let tname = format!("idx_{}_{}", itype.as_idx_str(), attr);
        let mut stmt = try_audit!(
            audit,
            self.get_conn()
                .prepare("SELECT COUNT(name) from sqlite_master where name = :tname"),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );
        let i: Option<i64> = try_audit!(
            audit,
            stmt.query_row_named(&[(":tname", &tname as &dyn ToSql)], |row| row.get(0)),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );

        if i.unwrap_or(0) == 0 {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn get_idl(
        &self,
        audit: &mut AuditScope,
        attr: &String,
        itype: &IndexType,
        idx_key: &String,
    ) -> Result<Option<IDLBitRange>, OperationError> {
        if self.exists_idx(audit, attr, itype)? == false {
            audit_log!(audit, "Index {:?} {:?} not found", itype, attr);
            return Ok(None);
        }
        // The table exists - lets now get the actual index itself.

        let query = format!(
            "SELECT idl FROM idx_{}_{} WHERE key = :idx_key",
            itype.as_idx_str(),
            attr
        );
        let mut stmt = try_audit!(
            audit,
            self.get_conn().prepare(query.as_str()),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );
        let idl_raw: Option<Vec<u8>> = try_audit!(
            audit,
            stmt.query_row_named(&[(":idx_key", idx_key)], |row| row.get(0))
                // We don't mind if it doesn't exist
                .optional(),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );

        let idl = match idl_raw {
            Some(d) => {
                serde_cbor::from_slice(d.as_slice()).map_err(|_| OperationError::SerdeCborError)?
            }
            // We don't have this value, it must be empty (or we
            // have a corrupted index .....
            None => IDLBitRange::new(),
        };

        Ok(Some(idl))
    }

    /*
    fn get_name2uuid(&self, name: &str) -> Result<Uuid, OperationError> {
        unimplemented!();
    }

    fn get_uuid2name(&self, uuid: &Uuid) -> Result<String, OperationError> {
        unimplemented!();
    }
    */

    fn get_db_sid(&self) -> Result<Option<SID>, OperationError> {
        // Try to get a value.
        self.get_conn()
            .query_row_named("SELECT data FROM db_sid WHERE id = 1", &[], |row| {
                row.get(0)
            })
            .optional()
            .map(|e_opt| {
                // If we have a row, we try to make it a sid
                e_opt.map(|e| {
                    let y: Vec<u8> = e;
                    assert!(y.len() == 4);
                    let mut sid: [u8; 4] = [0; 4];
                    for i in 0..4 {
                        sid[i] = y[i];
                    }
                    sid
                })
                // If no sid, we return none.
            })
            .map_err(|_| OperationError::SQLiteError)
    }
}

impl IdlSqliteTransaction for IdlSqliteReadTransaction {
    fn get_conn(&self) -> &r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager> {
        &self.conn
    }
}

impl Drop for IdlSqliteReadTransaction {
    // Abort - so far this has proven reliable to use drop here.
    fn drop(self: &mut Self) {
        if !self.committed {
            debug!("Aborting BE RO txn");
            self.conn
                .execute("ROLLBACK TRANSACTION", NO_PARAMS)
                // We can't do this without expect.
                // We may need to change how we do transactions to not rely on drop if
                // it becomes and issue :(
                .expect("Unable to rollback transaction! Can not proceed!!!");
        }
    }
}

impl IdlSqliteReadTransaction {
    pub fn new(conn: r2d2::PooledConnection<SqliteConnectionManager>) -> Self {
        // Start the transaction
        debug!("Starting BE RO txn ...");
        // I'm happy for this to be an expect, because this is a huge failure
        // of the server ... but if it happens a lot we should consider making
        // this a Result<>
        //
        // There is no way to flag this is an RO operation.
        conn.execute("BEGIN TRANSACTION", NO_PARAMS)
            .expect("Unable to begin transaction!");
        IdlSqliteReadTransaction {
            committed: false,
            conn: conn,
        }
    }
}

impl IdlSqliteTransaction for IdlSqliteWriteTransaction {
    fn get_conn(&self) -> &r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager> {
        &self.conn
    }
}

impl Drop for IdlSqliteWriteTransaction {
    // Abort
    fn drop(self: &mut Self) {
        if !self.committed {
            debug!("Aborting BE WR txn");
            self.conn
                .execute("ROLLBACK TRANSACTION", NO_PARAMS)
                .expect("Unable to rollback transaction! Can not proceed!!!");
        }
    }
}

impl IdlSqliteWriteTransaction {
    pub fn new(conn: r2d2::PooledConnection<SqliteConnectionManager>) -> Self {
        // Start the transaction
        debug!("Starting BE WR txn ...");
        conn.execute("BEGIN TRANSACTION", NO_PARAMS)
            .expect("Unable to begin transaction!");
        IdlSqliteWriteTransaction {
            committed: false,
            conn: conn,
        }
    }

    pub fn commit(mut self, audit: &mut AuditScope) -> Result<(), OperationError> {
        audit_log!(audit, "Commiting BE txn");
        assert!(!self.committed);
        self.committed = true;

        self.conn
            .execute("COMMIT TRANSACTION", NO_PARAMS)
            .map(|_| ())
            .map_err(|e| {
                println!("{:?}", e);
                OperationError::BackendEngine
            })
    }

    pub fn get_id2entry_max_id(&self) -> Result<i64, OperationError> {
        let mut stmt = self
            .conn
            .prepare("SELECT MAX(id) as id_max FROM id2entry")
            .map_err(|_| OperationError::SQLiteError)?;
        // This exists checks for if any rows WERE returned
        // that way we know to shortcut or not.
        let v = stmt
            .exists(NO_PARAMS)
            .map_err(|_| OperationError::SQLiteError)?;

        Ok(if v {
            // We have some rows, let get max!
            let i: Option<i64> = stmt
                .query_row(NO_PARAMS, |row| row.get(0))
                .map_err(|_| OperationError::SQLiteError)?;
            i.unwrap_or(0)
        } else {
            // No rows are present, return a 0.
            0
        })
    }

    pub fn write_identries(
        &self,
        au: &mut AuditScope,
        entries: Vec<IdEntry>,
    ) -> Result<(), OperationError> {
        let mut stmt = try_audit!(
            au,
            self.conn
                .prepare("INSERT OR REPLACE INTO id2entry (id, data) VALUES(:id, :data)"),
            "RusqliteError: {:?}",
            OperationError::SQLiteError
        );

        try_audit!(
            au,
            entries.iter().try_for_each(|ser_ent| {
                stmt.execute_named(&[(":id", &ser_ent.id), (":data", &ser_ent.data)])
                    // remove the updated usize
                    .map(|_| ())
            }),
            "RusqliteError: {:?}",
            OperationError::SQLiteError
        );
        Ok(())
    }

    pub fn delete_identry(&self, au: &mut AuditScope, idl: Vec<i64>) -> Result<(), OperationError> {
        let mut stmt = try_audit!(
            au,
            self.conn.prepare("DELETE FROM id2entry WHERE id = :id"),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );

        idl.iter().try_for_each(|id| {
            stmt.execute(&[&id])
                .map(|_| ())
                .map_err(|_| OperationError::SQLiteError)
        })
    }

    pub fn write_idl(
        &self,
        audit: &mut AuditScope,
        attr: &String,
        itype: &IndexType,
        idx_key: &String,
        idl: &IDLBitRange,
    ) -> Result<(), OperationError> {
        if idl.len() == 0 {
            audit_log!(audit, "purging idl -> {:?}", idl);
            // delete it
            // Delete this idx_key from the table.
            let query = format!(
                "DELETE FROM idx_{}_{} WHERE key = :key",
                itype.as_idx_str(),
                attr
            );

            self.conn
                .prepare(query.as_str())
                .and_then(|mut stmt| stmt.execute_named(&[(":key", &idx_key)]))
                .map_err(|e| {
                    audit_log!(audit, "SQLite Error {:?}", e);
                    OperationError::SQLiteError
                })
        } else {
            audit_log!(audit, "writing idl -> {:?}", idl);
            // Serialise the IDL to Vec<u8>
            let idl_raw = serde_cbor::to_vec(idl).map_err(|e| {
                audit_log!(audit, "Serde CBOR Error -> {:?}", e);
                OperationError::SerdeCborError
            })?;

            // update or create it.
            let query = format!(
                "INSERT OR REPLACE INTO idx_{}_{} (key, idl) VALUES(:key, :idl)",
                itype.as_idx_str(),
                attr
            );

            self.conn
                .prepare(query.as_str())
                .and_then(|mut stmt| stmt.execute_named(&[(":key", &idx_key), (":idl", &idl_raw)]))
                .map_err(|e| {
                    audit_log!(audit, "SQLite Error {:?}", e);
                    OperationError::SQLiteError
                })
        }
        // Get rid of the sqlite rows usize
        .map(|_| ())
    }

    pub fn create_name2uuid(&self, audit: &mut AuditScope) -> Result<(), OperationError> {
        try_audit!(
            audit,
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS idx_name2uuid (name TEXT PRIMARY KEY, uuid TEXT)",
                NO_PARAMS
            ),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );
        Ok(())
    }

    pub fn create_uuid2name(&self, audit: &mut AuditScope) -> Result<(), OperationError> {
        try_audit!(
            audit,
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS idx_uuid2name (uuid TEXT PRIMARY KEY, name TEXT)",
                NO_PARAMS
            ),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );
        Ok(())
    }

    pub fn create_idx(
        &self,
        audit: &mut AuditScope,
        attr: &String,
        itype: &IndexType,
    ) -> Result<(), OperationError> {
        // Is there a better way than formatting this? I can't seem
        // to template into the str.
        //
        // We could also re-design our idl storage.
        let idx_stmt = format!(
            "CREATE TABLE IF NOT EXISTS idx_{}_{} (key TEXT PRIMARY KEY, idl BLOB)",
            itype.as_idx_str(),
            attr
        );
        audit_log!(audit, "Creating index -> {}", idx_stmt);

        try_audit!(
            audit,
            self.conn.execute(idx_stmt.as_str(), NO_PARAMS),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );
        Ok(())
    }

    pub fn list_idxs(&self, audit: &mut AuditScope) -> Result<Vec<String>, OperationError> {
        let mut stmt = try_audit!(
            audit,
            self.get_conn()
                .prepare("SELECT name from sqlite_master where type='table' and name LIKE 'idx_%'"),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );
        let idx_table_iter = try_audit!(
            audit,
            stmt.query_map(NO_PARAMS, |row| row.get(0)),
            "SQLite Error {:?}",
            OperationError::SQLiteError
        );

        let r: Result<_, _> = idx_table_iter
            .map(|v| {
                v.map_err(|e| {
                    audit_log!(audit, "SQLite Error {:?}", e);
                    OperationError::SQLiteError
                })
            })
            .collect();

        r
    }

    pub unsafe fn purge_idxs(&self, audit: &mut AuditScope) -> Result<(), OperationError> {
        let idx_table_list = self.list_idxs(audit)?;

        idx_table_list.iter().try_for_each(|idx_table| {
            audit_log!(audit, "removing idx_table -> {:?}", idx_table);
            self.conn
                .prepare(format!("DROP TABLE {}", idx_table).as_str())
                .and_then(|mut stmt| stmt.query(NO_PARAMS).map(|_| ()))
                .map_err(|e| {
                    audit_log!(audit, "sqlite error {:?}", e);
                    OperationError::SQLiteError
                })
        })
    }

    pub unsafe fn purge_id2entry(&self, audit: &mut AuditScope) -> Result<(), OperationError> {
        try_audit!(
            audit,
            self.conn.execute("DELETE FROM id2entry", NO_PARAMS),
            "rustqlite error {:?}",
            OperationError::SQLiteError
        );
        Ok(())
    }

    pub fn write_db_sid(&self, nsid: &SID) -> Result<(), OperationError> {
        let mut data = Vec::new();
        data.extend_from_slice(nsid);

        self.conn
            .execute_named(
                "INSERT OR REPLACE INTO db_sid (id, data) VALUES(:id, :sid)",
                &[(":id", &1), (":sid", &data)],
            )
            .map(|_| ())
            .map_err(|e| {
                debug!("rusqlite error {:?}", e);

                OperationError::SQLiteError
            })
    }

    // ===== inner helpers =====
    // Some of these are not self due to use in new()
    fn get_db_version_key(&self, key: &str) -> i64 {
        match self.conn.query_row_named(
            "SELECT version FROM db_version WHERE id = :id",
            &[(":id", &key)],
            |row| row.get(0),
        ) {
            Ok(e) => e,
            Err(_) => {
                // The value is missing, default to 0.
                0
            }
        }
    }

    fn set_db_version_key(&self, key: &str, v: i64) -> Result<(), rusqlite::Error> {
        self.conn
            .execute_named(
                "INSERT OR REPLACE INTO db_version (id, version) VALUES(:id, :dbv_id2entry)",
                &[(":id", &key), (":dbv_id2entry", &v)],
            )
            .map(|_| ())
    }

    pub(crate) fn get_db_index_version(&self) -> i64 {
        self.get_db_version_key(DBV_INDEXV)
    }

    pub(crate) fn set_db_index_version(&self, v: i64) -> Result<(), OperationError> {
        self.set_db_version_key(DBV_INDEXV, v).map_err(|e| {
            debug!("sqlite error {:?}", e);
            OperationError::SQLiteError
        })
    }

    pub fn setup(&self, audit: &mut AuditScope) -> Result<(), OperationError> {
        // Enable WAL mode, which is just faster and better.
        //
        // We have to use stmt + prepare because execute can't handle
        // the "wal" row on result when this works!
        let mut wal_stmt = try_audit!(
            audit,
            self.conn.prepare("PRAGMA journal_mode=WAL;"),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );
        try_audit!(
            audit,
            wal_stmt.query(NO_PARAMS),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );

        // This stores versions of components. For example:
        // ----------------------
        // | id       | version |
        // | id2entry | 1       |
        // | index    | 1       |
        // | schema   | 1       |
        // ----------------------
        //
        // This allows each component to initialise on it's own, be
        // rolled back individually, by upgraded in isolation, and more
        //
        // NEVER CHANGE THIS DEFINITION.
        try_audit!(
            audit,
            self.conn.execute(
                "CREATE TABLE IF NOT EXISTS db_version (
                    id TEXT PRIMARY KEY,
                    version INTEGER
                )
                ",
                NO_PARAMS,
            ),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );

        // If the table is empty, populate the versions as 0.
        let mut dbv_id2entry = self.get_db_version_key(DBV_ID2ENTRY);
        audit_log!(audit, "dbv_id2entry initial == {}", dbv_id2entry);

        // Check db_version here.
        //   * if 0 -> create v1.
        if dbv_id2entry == 0 {
            try_audit!(
                audit,
                self.conn.execute(
                    "CREATE TABLE IF NOT EXISTS id2entry (
                        id INTEGER PRIMARY KEY ASC,
                        data BLOB NOT NULL
                    )
                    ",
                    NO_PARAMS,
                ),
                "sqlite error {:?}",
                OperationError::SQLiteError
            );
            try_audit!(
                audit,
                self.conn.execute(
                    "CREATE TABLE IF NOT EXISTS db_sid (
                        id INTEGER PRIMARY KEY ASC,
                        data BLOB NOT NULL
                    )
                    ",
                    NO_PARAMS,
                ),
                "sqlite error {:?}",
                OperationError::SQLiteError
            );
            dbv_id2entry = 1;
            audit_log!(audit, "dbv_id2entry migrated -> {}", dbv_id2entry);
        }
        //   * if v1 -> complete.

        try_audit!(
            audit,
            self.set_db_version_key(DBV_ID2ENTRY, dbv_id2entry),
            "sqlite error {:?}",
            OperationError::SQLiteError
        );

        // NOTE: Indexing is configured in a different step!
        // Indexing uses a db version flag to represent the version
        // of the indexes representation on disk in case we change
        // it.
        Ok(())
    }
}

impl IdlSqlite {
    pub fn new(audit: &mut AuditScope, path: &str, pool_size: u32) -> Result<Self, OperationError> {
        let manager = SqliteConnectionManager::file(path);
        let builder1 = Pool::builder();
        let builder2 = if path == "" {
            // We are in a debug mode, with in memory. We MUST have only
            // a single DB thread, else we cause consistency issues.
            builder1.max_size(1)
        } else {
            builder1.max_size(pool_size)
        };
        // Look at max_size and thread_pool here for perf later
        let pool = builder2.build(manager).map_err(|e| {
            audit_log!(audit, "r2d2 error {:?}", e);
            OperationError::SQLiteError
        })?;

        Ok(IdlSqlite { pool: pool })
    }

    pub fn read(&self) -> IdlSqliteReadTransaction {
        let conn = self
            .pool
            .get()
            .expect("Unable to get connection from pool!!!");
        IdlSqliteReadTransaction::new(conn)
    }

    pub fn write(&self) -> IdlSqliteWriteTransaction {
        let conn = self
            .pool
            .get()
            .expect("Unable to get connection from pool!!!");
        IdlSqliteWriteTransaction::new(conn)
    }
}

#[cfg(test)]
mod tests {}
