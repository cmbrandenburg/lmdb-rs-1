use libc::{c_uint, size_t, mode_t};
use std::io::FilePermission;
use std::ptr;
use std::sync::Mutex;

use error::{LmdbResult, lmdb_result};
use database::Database;
use ffi::*;
use transaction::{RoTransaction, RwTransaction, Transaction, TransactionExt};

/// An LMDB environment.
///
/// An environment supports multiple databases, all residing in the same shared-memory map.
pub struct Environment {
    env: *mut MDB_env,
    dbi_open_mutex: Mutex<()>,
}

impl Environment {

    /// Creates a new builder for specifying options for opening an LMDB environment.
    pub fn new() -> EnvironmentBuilder {
        EnvironmentBuilder {
            flags: EnvironmentFlags::empty(),
            max_readers: None,
            max_dbs: None,
            map_size: None
        }
    }

    /// Returns a raw pointer to the underlying LMDB environment.
    ///
    /// The caller **must** ensure that the pointer is not dereferenced after the lifetime of the
    /// environment.
    pub fn env(&self) -> *mut MDB_env {
        self.env
    }

    /// Opens a handle to an LMDB database.
    ///
    /// If `name` is `None`, then the returned handle will be for the default database.
    ///
    /// If `name` is not `None`, then the returned handle will be for a named database. In this
    /// case the environment must be configured to allow named databases through
    /// `EnvironmentBuilder::set_max_dbs`.
    ///
    /// The returned database handle may be shared among transactions.
    pub fn open_db<'env>(&'env self, name: Option<&str>) -> LmdbResult<Database<'env>> {
        let mutex = self.dbi_open_mutex.lock();
        let txn = try!(self.begin_read_txn());
        let db = unsafe { try!(Database::open(&txn, name)) };
        try!(txn.commit());
        drop(mutex);
        Ok(db)
    }

    /// Opens a handle to an LMDB database, opening the database if necessary.
    ///
    /// If the database is already created, the given option flags will be added to it.
    ///
    /// If `name` is `None`, then the returned handle will be for the default database.
    ///
    /// If `name` is not `None`, then the returned handle will be for a named database. In this
    /// case the environment must be configured to allow named databases through
    /// `EnvironmentBuilder::set_max_dbs`.
    ///
    /// The returned database handle may be shared among transactions.
    pub fn create_db<'env>(&'env self,
                           name: Option<&str>,
                           flags: DatabaseFlags)
                           -> LmdbResult<Database<'env>> {
        let mutex = self.dbi_open_mutex.lock();
        let txn = try!(self.begin_write_txn());
        let db = unsafe { try!(Database::create(&txn, name, flags)) };
        try!(txn.commit());
        drop(mutex);
        Ok(db)
    }

    pub fn get_db_flags<'env>(&'env self, db: Database<'env>) -> LmdbResult<DatabaseFlags> {
        let txn = try!(self.begin_read_txn());
        let mut flags: c_uint = 0;
        unsafe {
            try!(lmdb_result(mdb_dbi_flags(txn.txn(), db.dbi(), &mut flags)));
        }
        Ok(DatabaseFlags::from_bits(flags).unwrap())
    }

    /// Create a read-only transaction for use with the environment.
    pub fn begin_read_txn<'env>(&'env self) -> LmdbResult<RoTransaction<'env>> {
        RoTransaction::new(self)
    }

    /// Create a read-write transaction for use with the environment. This method will block while
    /// there are any other read-write transactions open on the environment.
    pub fn begin_write_txn<'env>(&'env self) -> LmdbResult<RwTransaction<'env>> {
        RwTransaction::new(self)
    }

    /// Flush data buffers to disk.
    ///
    /// Data is always written to disk when `Transaction::commit()` is called, but the operating
    /// system may keep it buffered. LMDB always flushes the OS buffers upon commit as well, unless
    /// the environment was opened with `MDB_NOSYNC` or in part `MDB_NOMETASYNC`.
    pub fn sync(&self, force: bool) -> LmdbResult<()> {
        unsafe {
            lmdb_result(mdb_env_sync(self.env(), if force { 1 } else { 0 }))
        }
    }

    /// Close a database handle. Normally unnecessary.
    ///
    /// This call is not mutex protected. Handles should only be closed by a single thread, and only
    /// if no other threads are going to reference the database handle or one of its cursors any
    /// further. Do not close a handle if an existing transaction has modified its database. Doing
    /// so can cause misbehavior from database corruption to errors like `MDB_BAD_VALSIZE` (since the
    /// DB name is gone).
    ///
    /// Closing a database handle is not necessary, but lets `Transaction::open_database` reuse the
    /// handle value. Usually it's better to set a bigger `EnvironmentBuilder::set_max_dbs`, unless
    /// that value would be large.
    pub unsafe fn close_db(&self, db: Database) {
        mdb_dbi_close(self.env, db.dbi())
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        unsafe { mdb_env_close(self.env) }
    }
}

///////////////////////////////////////////////////////////////////////////////////////////////////
//// Environment Builder
///////////////////////////////////////////////////////////////////////////////////////////////////

/// Options for opening or creating an environment.
#[deriving(Show, PartialEq, Eq, Copy, Clone)]
pub struct EnvironmentBuilder {
    flags: EnvironmentFlags,
    max_readers: Option<c_uint>,
    max_dbs: Option<c_uint>,
    map_size: Option<size_t>,
}

impl EnvironmentBuilder {

    /// Open an environment.
    pub fn open(&self, path: &Path, mode: FilePermission) -> LmdbResult<Environment> {
        let mut env: *mut MDB_env = ptr::null_mut();
        unsafe {
            lmdb_try!(mdb_env_create(&mut env));
            if let Some(max_readers) = self.max_readers {
                lmdb_try_with_cleanup!(mdb_env_set_maxreaders(env, max_readers),
                                       mdb_env_close(env))
            }
            if let Some(max_dbs) = self.max_dbs {
                lmdb_try_with_cleanup!(mdb_env_set_maxdbs(env, max_dbs),
                                       mdb_env_close(env))
            }
            if let Some(map_size) = self.map_size {
                lmdb_try_with_cleanup!(mdb_env_set_mapsize(env, map_size),
                                       mdb_env_close(env))
            }
            lmdb_try_with_cleanup!(mdb_env_open(env,
                                                     path.to_c_str().as_ptr(),
                                                     self.flags.bits(),
                                                     mode.bits() as mode_t),
                                   mdb_env_close(env));
        }
        Ok(Environment { env: env,
                         dbi_open_mutex: Mutex::new(()) })
    }

    pub fn set_flags(&mut self, flags: EnvironmentFlags) -> &mut EnvironmentBuilder {
        self.flags = flags;
        self
    }

    /// Sets the maximum number of threads or reader slots for the environment.
    ///
    /// This defines the number of slots in the lock table that is used to track readers in the
    /// the environment. The default is 126. Starting a read-only transaction normally ties a lock
    /// table slot to the current thread until the environment closes or the thread exits. If
    /// `MDB_NOTLS` is in use, `Environment::open_txn` instead ties the slot to the `Transaction`
    /// object until it or the `Environment` object is destroyed.
    pub fn set_max_readers(&mut self, max_readers: c_uint) -> &mut EnvironmentBuilder {
        self.max_readers = Some(max_readers);
        self
    }

    /// Sets the maximum number of named databases for the environment.
    ///
    /// This function is only needed if multiple databases will be used in the
    /// environment. Simpler applications that use the environment as a single
    /// unnamed database can ignore this option.
    ///
    /// Currently a moderate number of slots are cheap but a huge number gets
    /// expensive: 7-120 words per transaction, and every `Transaction::open_db`
    /// does a linear search of the opened slots.
    pub fn set_max_dbs(&mut self, max_readers: c_uint) -> &mut EnvironmentBuilder {
        self.max_dbs = Some(max_readers);
        self
    }

    /// Sets the size of the memory map to use for the environment.
    ///
    /// The size should be a multiple of the OS page size. The default is
    /// 10485760 bytes. The size of the memory map is also the maximum size
    /// of the database. The value should be chosen as large as possible,
    /// to accommodate future growth of the database. It may be increased at
    /// later times.
    ///
    /// Any attempt to set a size smaller than the space already consumed
    /// by the environment will be silently changed to the current size of the used space.
    pub fn set_map_size(&mut self, map_size: size_t) -> &mut EnvironmentBuilder {
        self.map_size = Some(map_size);
        self
    }
}

#[cfg(test)]
mod test {

    use std::io;

    use ffi::*;
    use super::*;

    #[test]
    fn test_open() {
        let dir = io::TempDir::new("test").unwrap();

        // opening non-existent env with read-only should fail
        assert!(Environment::new().set_flags(MDB_RDONLY)
                                  .open(dir.path(), io::USER_RWX)
                                  .is_err());

        // opening non-existent env should not fail
        assert!(Environment::new().open(dir.path(), io::USER_RWX).is_ok());

        // opening env with read-only should not fail
        assert!(Environment::new().set_flags(MDB_RDONLY)
                                  .open(dir.path(), io::USER_RWX)
                                  .is_ok());
    }

    #[test]
    fn test_begin_txn() {
        let dir = io::TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path(), io::USER_RWX).unwrap();

        {
            // Mutable env, mutable txn
            assert!(env.begin_write_txn().is_ok());
        } {
            // Mutable env, read-only txn
            assert!(env.begin_read_txn().is_ok());
        } {
            // Read-only env, mutable txn
            let env = Environment::new().set_flags(MDB_RDONLY)
                                        .open(dir.path(), io::USER_RWX)
                                        .unwrap();
            assert!(env.begin_write_txn().is_err());
        } {
            // Read-only env, read-only txn
            let env = Environment::new().set_flags(MDB_RDONLY)
                                        .open(dir.path(), io::USER_RWX)
                                        .unwrap();
            assert!(env.begin_read_txn().is_ok());
        }
    }

    #[test]
    fn test_open_db() {
        let dir = io::TempDir::new("test").unwrap();
        let env = Environment::new().set_max_dbs(10)
                                    .open(dir.path(), io::USER_RWX)
                                    .unwrap();
        assert!(env.open_db(None).is_ok());
        assert!(env.open_db(Some("testdb")).is_err());
    }

    #[test]
    fn test_create_db() {
        let dir = io::TempDir::new("test").unwrap();
        let env = Environment::new().set_max_dbs(10)
                                    .open(dir.path(), io::USER_RWX)
                                    .unwrap();
        assert!(env.open_db(Some("testdb")).is_err());
        assert!(env.create_db(Some("testdb"), DatabaseFlags::empty()).is_ok());
        assert!(env.open_db(Some("testdb")).is_ok())
    }

    #[test]
    fn test_sync() {
        let dir = io::TempDir::new("test").unwrap();
        {
            let env = Environment::new().open(dir.path(), io::USER_RWX).unwrap();
            assert!(env.sync(true).is_ok());
        } {
            let env = Environment::new().set_flags(MDB_RDONLY)
                                        .open(dir.path(), io::USER_RWX)
                                        .unwrap();
            assert!(env.sync(true).is_ok());
        }
    }
}
