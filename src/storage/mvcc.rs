//! This module implements MVCC (Multi-Version Concurrency Control), a widely
//! used method for ACID transactions and concurrency control. It allows
//! multiple concurrent transactions to access and modify the same dataset,
//! isolates them from each other, detects and handles conflicts, and commits
//! their writes atomically as a single unit. It uses an underlying storage
//! engine to store raw keys and values.
//!
//! VERSIONS
//! ========
//!
//! MVCC handles concurrency control by managing multiple historical versions of
//! keys, identified by a timestamp. Every write adds a new version at a higher
//! timestamp, with deletes having a special tombstone value. For example, the
//! keys a,b,c,d may have the following values at various logical timestamps (x
//! is tombstone):
//!
//! Time
//! 5
//! 4  a4          
//! 3      b3      x
//! 2            
//! 1  a1      c1  d1
//!    a   b   c   d   Keys
//!
//! A transaction t2 that started at T=2 will see the values a=a1, c=c1, d=d1. A
//! different transaction t5 running at T=5 will see a=a4, b=b3, c=c1.
//!
//! ToyDB uses logical timestamps with a sequence number stored in
//! Key::NextVersion. Each new read-write transaction takes its timestamp from
//! the current value of Key::NextVersion and then increments the value for the
//! next transaction.
//!
//! ISOLATION
//! =========
//!
//! MVCC provides an isolation level called snapshot isolation. Briefly,
//! transactions see a consistent snapshot of the database state as of their
//! start time. Writes made by concurrent or subsequent transactions are never
//! visible to it. If two concurrent transactions write to the same key they
//! will conflict and one of them must retry. A transaction's writes become
//! atomically visible to subsequent transactions only when they commit, and are
//! rolled back on failure. Read-only transactions never conflict with other
//! transactions.
//!
//! Transactions write new versions at their timestamp, storing them as
//! Key::Version(key, version) => value. If a transaction writes to a key and
//! finds a newer version, it returns an error and the client must retry.
//!
//! Active (uncommitted) read-write transactions record their version in the
//! active set, stored as Key::Active(version). When new transactions begin, they
//! take a snapshot of this active set, and any key versions that belong to a
//! transaction in the active set are considered invisible (to anyone except that
//! transaction itself). Writes to keys that already have a past version in the
//! active set will also return an error.
//!
//! To commit, a transaction simply deletes its record in the active set. This
//! will immediately (and, crucially, atomically) make all of its writes visible
//! to subsequent transactions, but not ongoing ones. If the transaction is
//! cancelled and rolled back, it maintains a record of all keys it wrote as
//! Key::TxnWrite(version, key), so that it can find the corresponding versions
//! and delete them before removing itself from the active set.
//!
//! Consider the following example, where we have two ongoing transactions at
//! time T=2 and T=5, with some writes that are not yet committed marked in
//! parentheses.
//!
//! Active set: [2, 5]
//!
//! Time
//! 5 (a5)
//! 4  a4          
//! 3      b3      x
//! 2         (x)     (e2)
//! 1  a1      c1  d1
//!    a   b   c   d   e   Keys
//!
//! Here, t2 will see a=a1, d=d1, e=e2 (it sees its own writes). t5 will see
//! a=a5, b=b3, c=c1. t2 does not see any newer versions, and t5 does not see
//! the tombstone at c@2 nor the value e=e2, because version=2 is in its active
//! set.
//!
//! If t2 tries to write b=b2, it receives an error and must retry, because a
//! newer version exists. Similarly, if t5 tries to write e=e5, it receives an
//! error and must retry, because the version e=e2 is in its active set.
//!
//! To commit, t2 can remove itself from the active set. A new transaction t6
//! starting after the commit will then see c as deleted and e=e2. t5 will still
//! not see any of t2's writes, because it's still in its local snapshot of the
//! active set at the time it began.
//!
//! READ-ONLY AND TIME TRAVEL QUERIES
//! =================================
//!
//! Since MVCC stores historical versions, it can trivially support time travel
//! queries where a transaction reads at a past timestamp and has a consistent
//! view of the database at that time.
//!
//! This is done by a transaction simply using a past version, as if it had
//! started far in the past, ignoring newer versions like any other transaction.
//! This transaction cannot write, as it does not have a unique timestamp (the
//! original read-write transaction originally owned this timestamp).
//!
//! The only wrinkle is that the time-travel query must also know what the active
//! set was at that version. Otherwise, it may see past transactions that committed
//! after that time, which were not visible to the original transaction that wrote
//! at that version. Similarly, if a time-travel query reads at a version that is
//! still active, it should not see its in-progress writes, and after it commits
//! a different time-travel query should not see those writes either, to maintain
//! version consistency.
//!
//! To achieve this, every read-write transaction stores its active set snapshot
//! in the storage engine as well, as Key::TxnActiveSnapshot, such that later
//! time-travel queries can restore its original snapshot. Furthermore, a
//! time-travel query can only see versions below the snapshot version, otherwise
//! it could see spurious in-progress or since-committed versions.
//!
//! In the following example, a time-travel query at version=3 would see a=a1,
//! c=c1, d=d1.
//!
//! Time
//! 5
//! 4  a4          
//! 3      b3      x
//! 2            
//! 1  a1      c1  d1
//!    a   b   c   d   Keys
//!
//! Read-only queries work similarly to time-travel queries, with one exception:
//! they read at the next (current) version, i.e. Key::NextVersion, and use the
//! current active set, storing the snapshot in memory only. Read-only queries
//! do not increment the version sequence number in Key::NextVersion.
//!
//! GARBAGE COLLECTION
//! ==================
//!
//! Normally, old versions would be garbage collected regularly, when they are
//! no longer needed by active transactions or time-travel queries. However,
//! ToyDB does not implement garbage collection, instead keeping all history
//! forever, both out of laziness and also because it allows unlimited time
//! travel queries (it's a feature, not a bug!).

use super::{keycode, Engine};
use crate::error::{Error, Result};

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::{Bound, RangeBounds};
use std::sync::{Arc, Mutex, MutexGuard};

/// An MVCC version represents a logical timestamp. The latest version
/// is incremented when beginning each read-write transaction.
type Version = u64;

/// MVCC keys, using the KeyCode encoding which preserves the ordering and
/// grouping of keys. Cow byte slices allow encoding borrowed values and
/// decoding into owned values.
#[derive(Debug, Deserialize, Serialize)]
enum Key<'a> {
    /// The next available version.
    NextVersion,
    /// Active (uncommitted) transactions by version.
    TxnActive(Version),
    /// A snapshot of the active set at each version. Only written for
    /// versions where the active set is non-empty (excluding itself).
    TxnActiveSnapshot(Version),
    /// Keeps track of all keys written to by an active transaction (identified
    /// by its version), in case it needs to roll back.
    TxnWrite(
        Version,
        #[serde(with = "serde_bytes")]
        #[serde(borrow)]
        Cow<'a, [u8]>,
    ),
    /// A versioned key/value pair.
    Version(
        #[serde(with = "serde_bytes")]
        #[serde(borrow)]
        Cow<'a, [u8]>,
        Version,
    ),
    /// Unversioned non-transactional key/value pairs. These exist separately
    /// from versioned keys, i.e. the unversioned key "foo" is entirely
    /// independent of the versioned key "foo@7". These are mostly used
    /// for metadata.
    Unversioned(
        #[serde(with = "serde_bytes")]
        #[serde(borrow)]
        Cow<'a, [u8]>,
    ),
}

impl<'a> Key<'a> {
    fn decode(bytes: &'a [u8]) -> Result<Self> {
        keycode::deserialize(bytes)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        keycode::serialize(&self)
    }
}

/// MVCC key prefixes, for prefix scans. These must match the keys above,
/// including the enum variant index.
#[derive(Debug, Deserialize, Serialize)]
enum KeyPrefix<'a> {
    NextVersion,
    TxnActive,
    TxnActiveSnapshot,
    TxnWrite(Version),
    Version(
        #[serde(with = "serde_bytes")]
        #[serde(borrow)]
        Cow<'a, [u8]>,
    ),
    Unversioned,
}

impl<'a> KeyPrefix<'a> {
    fn encode(&self) -> Result<Vec<u8>> {
        keycode::serialize(&self)
    }
}

/// An MVCC-based transactional key-value engine. It wraps an underlying storage
/// engine that's used for raw key/value storage.
///
/// While it supports any number of concurrent transactions, individual read or
/// write operations are executed sequentially, serialized via a mutex. There
/// are two reasons for this: the storage engine itself is not thread-safe,
/// requiring serialized access, and the Raft state machine that manages the
/// MVCC engine applies commands one at a time from the Raft log, which will
/// serialize them anyway.
pub struct MVCC<E: Engine> {
    engine: Arc<Mutex<E>>,
}

impl<E: Engine> Clone for MVCC<E> {
    fn clone(&self) -> Self {
        MVCC { engine: self.engine.clone() }
    }
}

impl<E: Engine> MVCC<E> {
    /// Creates a new MVCC engine with the given storage engine.
    pub fn new(engine: E) -> Self {
        Self { engine: Arc::new(Mutex::new(engine)) }
    }

    /// Begins a new read-write transaction.
    pub fn begin(&self) -> Result<Transaction<E>> {
        Transaction::begin(self.engine.clone())
    }

    /// Begins a new read-only transaction at the latest version.
    pub fn begin_read_only(&self) -> Result<Transaction<E>> {
        Transaction::begin_read_only(self.engine.clone(), None)
    }

    /// Begins a new read-only transaction as of the given version.
    pub fn begin_as_of(&self, version: Version) -> Result<Transaction<E>> {
        Transaction::begin_read_only(self.engine.clone(), Some(version))
    }

    /// Resumes a transaction from the given transaction state.
    pub fn resume(&self, state: TransactionState) -> Result<Transaction<E>> {
        Transaction::resume(self.engine.clone(), state)
    }

    /// Fetches the value of an unversioned key.
    pub fn get_unversioned(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.engine.lock()?.get(&Key::Unversioned(key.into()).encode()?)
    }

    /// Sets the value of an unversioned key.
    pub fn set_unversioned(&self, key: &[u8], value: Vec<u8>) -> Result<()> {
        self.engine.lock()?.set(&Key::Unversioned(key.into()).encode()?, value)
    }

    /// Returns the status of the MVCC and storage engines.
    pub fn status(&self) -> Result<Status> {
        let mut engine = self.engine.lock()?;
        let storage = engine.to_string();
        let versions = match engine.get(&Key::NextVersion.encode()?)? {
            Some(ref v) => bincode::deserialize::<u64>(v)? - 1,
            None => 0,
        };
        let active_txns = engine.scan_prefix(&KeyPrefix::TxnActive.encode()?).count() as u64;
        Ok(Status { storage, versions, active_txns })
    }
}

/// MVCC engine status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The total number of MVCC versions (i.e. read-write transactions).
    pub versions: u64,
    /// Number of currently active transactions.
    pub active_txns: u64,
    /// The storage engine.
    /// TODO: export engine status instead.
    pub storage: String,
}

/// An MVCC transaction.
pub struct Transaction<E: Engine> {
    /// The underlying engine, shared by all transactions.
    engine: Arc<Mutex<E>>,
    /// The transaction state.
    st: TransactionState,
}

/// A Transaction's state, which determines its write version and isolation. It
/// is separate from Transaction to allow it to be passed around independently
/// of the engine. There are two main motivations for this:
///
/// - It can be exported via Transaction.state(), (de)serialized, and later used
///   to instantiate a new functionally equivalent Transaction via
///   Transaction::resume(). This allows passing the transaction between the
///   storage engine and SQL engine (potentially running on a different node)
///   across the Raft state machine boundary.
///
/// - It can be borrowed independently of Engine, allowing references to it
///   in VisibleIterator, which would otherwise result in self-references.
#[derive(Clone, Serialize, Deserialize)]
pub struct TransactionState {
    /// The version this transaction is running at. Only one read-write
    /// transaction can run at a given version, since this identifies its
    /// writes.
    pub version: Version,
    /// If true, the transaction is read only.
    pub read_only: bool,
    /// The set of concurrent active (uncommitted) transactions, as of the start
    /// of this transaction. Their writes should be invisible to this
    /// transaction even if they're writing at a lower version, since they're
    /// not committed yet.
    pub active: HashSet<Version>,
}

impl TransactionState {
    /// Checks whether the given version is visible to this transaction.
    ///
    /// Future versions, and versions belonging to active transactions as of
    /// the start of this transaction, are never isible.
    ///
    /// Read-write transactions see their own writes at their version.
    ///
    /// Read-only queries only see versions below the transaction's version,
    /// excluding the version itself. This is to ensure time-travel queries see
    /// a consistent version both before and after any active transaction at
    /// that version commits its writes. See the module documentation for
    /// details.
    fn is_visible(&self, version: Version) -> bool {
        if self.active.get(&version).is_some() {
            false
        } else if self.read_only {
            version < self.version
        } else {
            version <= self.version
        }
    }
}

impl<E: Engine> Transaction<E> {
    /// Begins a new transaction in read-write mode. This will allocate a new
    /// version that the transaction can write at, add it to the active set, and
    /// record its active snapshot for time-travel queries.
    fn begin(engine: Arc<Mutex<E>>) -> Result<Self> {
        let mut session = engine.lock()?;

        // Allocate a new version to write at.
        let version = match session.get(&Key::NextVersion.encode()?)? {
            Some(ref v) => bincode::deserialize(v)?,
            None => 1,
        };
        session.set(&Key::NextVersion.encode()?, bincode::serialize(&(version + 1))?)?;

        // Fetch the current set of active transactions, persist it for
        // time-travel queries if non-empty, then add this txn to it.
        let active = Self::scan_active(&mut session)?;
        if !active.is_empty() {
            session.set(&Key::TxnActiveSnapshot(version).encode()?, bincode::serialize(&active)?)?
        }
        session.set(&Key::TxnActive(version).encode()?, vec![])?;
        drop(session);

        Ok(Self { engine, st: TransactionState { version, read_only: false, active } })
    }

    /// Begins a new read-only transaction. If version is given it will see the
    /// state as of the beginning of that version (ignoring writes at that
    /// version). In other words, it sees the same state as the read-write
    /// transaction at that version saw when it began.
    fn begin_read_only(engine: Arc<Mutex<E>>, as_of: Option<Version>) -> Result<Self> {
        let mut session = engine.lock()?;

        // Fetch the latest version.
        let mut version = match session.get(&Key::NextVersion.encode()?)? {
            Some(ref v) => bincode::deserialize(v)?,
            None => 1,
        };

        // If requested, create the transaction as of a past version, restoring
        // the active snapshot as of the beginning of that version. Otherwise,
        // use the latest version and get the current, real-time snapshot.
        let mut active = HashSet::new();
        if let Some(as_of) = as_of {
            if as_of >= version {
                return Err(Error::Value(format!("Version {} does not exist", as_of)));
            }
            version = as_of;
            if let Some(value) = session.get(&Key::TxnActiveSnapshot(version).encode()?)? {
                active = bincode::deserialize(&value)?;
            }
        } else {
            active = Self::scan_active(&mut session)?;
        }

        drop(session);

        Ok(Self { engine, st: TransactionState { version, read_only: true, active } })
    }

    /// Resumes a transaction from the given state.
    fn resume(engine: Arc<Mutex<E>>, s: TransactionState) -> Result<Self> {
        // For read-write transactions, verify that the transaction is still
        // active before making further writes.
        if !s.read_only && engine.lock()?.get(&Key::TxnActive(s.version).encode()?)?.is_none() {
            return Err(Error::Internal(format!("No active transaction at version {}", s.version)));
        }
        Ok(Self { engine, st: s })
    }

    /// Fetches the set of currently active transactions.
    fn scan_active(session: &mut MutexGuard<E>) -> Result<HashSet<Version>> {
        let mut active = HashSet::new();
        let mut scan = session.scan_prefix(&KeyPrefix::TxnActive.encode()?);
        while let Some((key, _)) = scan.next().transpose()? {
            match Key::decode(&key)? {
                Key::TxnActive(version) => active.insert(version),
                _ => return Err(Error::Internal(format!("Expected TxnActive key, got {:?}", key))),
            };
        }
        Ok(active)
    }

    /// Returns the version the transaction is running at.
    pub fn version(&self) -> Version {
        self.st.version
    }

    /// Returns whether the transaction is read-only.
    pub fn read_only(&self) -> bool {
        self.st.read_only
    }

    /// Returns the transaction's state. This can be used to instantiate a
    /// functionally equivalent transaction via resume().
    pub fn state(&self) -> &TransactionState {
        &self.st
    }

    /// Commits the transaction, by removing it from the active set. This will
    /// immediately make its writes visible to subsequent transactions.
    pub fn commit(self) -> Result<()> {
        if self.st.read_only {
            return Ok(());
        }
        self.engine.lock()?.delete(&Key::TxnActive(self.st.version).encode()?)
    }

    /// Rolls back the transaction, by undoing all written versions and removing
    /// it from the active set. The active set snapshot is left behind, since
    /// this is needed for time travel queries at this version.
    pub fn rollback(self) -> Result<()> {
        if self.st.read_only {
            return Ok(());
        }
        let mut session = self.engine.lock()?;
        let mut rollback = Vec::new();
        let mut scan = session.scan_prefix(&KeyPrefix::TxnWrite(self.st.version).encode()?);
        while let Some((key, _)) = scan.next().transpose()? {
            match Key::decode(&key)? {
                Key::TxnWrite(_, key) => {
                    rollback.push(Key::Version(key, self.st.version).encode()?) // the version
                }
                key => return Err(Error::Internal(format!("Expected TxnWrite, got {:?}", key))),
            };
            rollback.push(key); // the TxnWrite record
        }
        drop(scan);
        for key in rollback.into_iter() {
            session.delete(&key)?;
        }
        session.delete(&Key::TxnActive(self.st.version).encode()?) // remove from active set
    }

    /// Deletes a key.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.write_version(key, None)
    }

    /// Sets a value for a key.
    pub fn set(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        self.write_version(key, Some(value))
    }

    /// Writes a new version for a key at the transaction's version. None writes
    /// a deletion tombstone. If a write conflict is found (either a newer or
    /// uncommitted version), a serialization error is returned.  Replacing our
    /// own uncommitted write is fine.
    fn write_version(&self, key: &[u8], value: Option<Vec<u8>>) -> Result<()> {
        if self.st.read_only {
            return Err(Error::ReadOnly);
        }
        let mut session = self.engine.lock()?;

        // Check for write conflicts, i.e. if the latest key is invisible to us
        // (either a newer version, or an uncommitted version in our past). We
        // can only conflict with the latest key, since all transactions enforce
        // the same invariant.
        let from = Key::Version(
            key.into(),
            self.st.active.iter().min().copied().unwrap_or(self.st.version + 1),
        )
        .encode()?;
        let to = Key::Version(key.into(), u64::MAX).encode()?;
        if let Some((key, _)) = session.scan(from..=to).last().transpose()? {
            match Key::decode(&key)? {
                Key::Version(_, version) => {
                    if !self.st.is_visible(version) {
                        return Err(Error::Serialization);
                    }
                }
                key => return Err(Error::Internal(format!("Expected Key::Version got {:?}", key))),
            }
        }

        // Write the new version and its write record.
        //
        // NB: TxnWrite contains the provided user key, not the encoded engine
        // key, since we can construct the engine key using the version.
        session.set(&Key::TxnWrite(self.st.version, key.into()).encode()?, vec![])?;
        session
            .set(&Key::Version(key.into(), self.st.version).encode()?, bincode::serialize(&value)?)
    }

    /// Fetches a key's value, or None if it does not exist.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut session = self.engine.lock()?;
        let from = Key::Version(key.into(), 0).encode()?;
        let to = Key::Version(key.into(), self.st.version).encode()?;
        let mut scan = session.scan(from..=to).rev();
        while let Some((key, value)) = scan.next().transpose()? {
            match Key::decode(&key)? {
                Key::Version(_, version) => {
                    if self.st.is_visible(version) {
                        return Ok(bincode::deserialize(&value)?);
                    }
                }
                key => return Err(Error::Internal(format!("Expected Key::Version got {:?}", key))),
            };
        }
        Ok(None)
    }

    /// Returns an iterator over the latest visible key/value pairs at the
    /// transaction's version.
    pub fn scan<R: RangeBounds<Vec<u8>>>(&self, range: R) -> Result<Scan<E>> {
        let start = match range.start_bound() {
            Bound::Excluded(k) => Bound::Excluded(Key::Version(k.into(), u64::MAX).encode()?),
            Bound::Included(k) => Bound::Included(Key::Version(k.into(), 0).encode()?),
            Bound::Unbounded => Bound::Included(Key::Version(vec![].into(), 0).encode()?),
        };
        let end = match range.end_bound() {
            Bound::Excluded(k) => Bound::Excluded(Key::Version(k.into(), 0).encode()?),
            Bound::Included(k) => Bound::Included(Key::Version(k.into(), u64::MAX).encode()?),
            Bound::Unbounded => Bound::Excluded(KeyPrefix::Unversioned.encode()?),
        };
        Ok(Scan::from_range(self.engine.lock()?, self.state(), start, end))
    }

    /// Scans keys under a given prefix.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Scan<E>> {
        // Normally, KeyPrefix::Version will only match all versions of the
        // exact given key. We want all keys maching the prefix, so we chop off
        // the KeyCode byte slice terminator 0x0000 at the end.
        let mut prefix = KeyPrefix::Version(prefix.into()).encode()?;
        prefix.truncate(prefix.len() - 2);
        Ok(Scan::from_prefix(self.engine.lock()?, self.state(), prefix))
    }
}

/// A scan result. Can produce an iterator or collect an owned Vec.
///
/// This intermediate struct is unfortunately needed to hold the MutexGuard for
/// the scan() caller, since placing it in ScanIterator along with the inner
/// iterator borrowing from it would create a self-referential struct.
///
/// TODO: is there a better way?
pub struct Scan<'a, E: Engine + 'a> {
    /// Access to the locked engine.
    engine: MutexGuard<'a, E>,
    /// The transaction state.
    txn: &'a TransactionState,
    /// The scan type and parameter.
    param: ScanType,
}

enum ScanType {
    Range((Bound<Vec<u8>>, Bound<Vec<u8>>)),
    Prefix(Vec<u8>),
}

impl<'a, E: Engine + 'a> Scan<'a, E> {
    /// Runs a normal range scan.
    fn from_range(
        engine: MutexGuard<'a, E>,
        txn: &'a TransactionState,
        start: Bound<Vec<u8>>,
        end: Bound<Vec<u8>>,
    ) -> Self {
        Self { engine, txn, param: ScanType::Range((start, end)) }
    }

    /// Runs a prefix scan.
    fn from_prefix(engine: MutexGuard<'a, E>, txn: &'a TransactionState, prefix: Vec<u8>) -> Self {
        Self { engine, txn, param: ScanType::Prefix(prefix) }
    }

    /// Returns an iterator over the result.
    pub fn iter(&mut self) -> ScanIterator<'_, E> {
        let inner = match &self.param {
            ScanType::Range(range) => self.engine.scan(range.clone()),
            ScanType::Prefix(prefix) => self.engine.scan_prefix(prefix),
        };
        ScanIterator::new(self.txn, inner)
    }

    /// Collects the result to a vector.
    pub fn to_vec(&mut self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.iter().collect()
    }
}

/// An iterator over the latest live and visible key/value pairs at the txn
/// version.
pub struct ScanIterator<'a, E: Engine + 'a> {
    /// Decodes and filters visible MVCC versions from the inner engine iterator.
    inner: std::iter::Peekable<VersionIterator<'a, E>>,
    /// The previous key emitted by try_next_back(). Note that try_next() does
    /// not affect reverse positioning: double-ended iterators consume from each
    /// end independently.
    last_back: Option<Vec<u8>>,
}

impl<'a, E: Engine + 'a> ScanIterator<'a, E> {
    /// Creates a new scan iterator.
    fn new(txn: &'a TransactionState, inner: E::ScanIterator<'a>) -> Self {
        Self { inner: VersionIterator::new(txn, inner).peekable(), last_back: None }
    }

    /// Fallible next(), emitting the next item, or None if exhausted.
    fn try_next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        while let Some((key, _version, value)) = self.inner.next().transpose()? {
            // If the next key equals this one, we're not at the latest version.
            match self.inner.peek() {
                Some(Ok((next, _, _))) if next == &key => continue,
                Some(Err(err)) => return Err(err.clone()),
                Some(Ok(_)) | None => {}
            }
            // If the key is live (not a tombstone), emit it.
            if let Some(value) = bincode::deserialize(&value)? {
                return Ok(Some((key, value)));
            }
        }
        Ok(None)
    }

    /// Fallible next_back(), emitting the next item from the back, or None if
    /// exhausted.
    fn try_next_back(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        while let Some((key, _version, value)) = self.inner.next_back().transpose()? {
            // If this key is the same as the last emitted key from the back,
            // this must be an older version, so skip it.
            if let Some(last) = &self.last_back {
                if last == &key {
                    continue;
                }
            }
            self.last_back = Some(key.clone());

            // If the key is live (not a tombstone), emit it.
            if let Some(value) = bincode::deserialize(&value)? {
                return Ok(Some((key, value)));
            }
        }
        Ok(None)
    }
}

impl<'a, E: Engine> Iterator for ScanIterator<'a, E> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().transpose()
    }
}

impl<'a, E: Engine> DoubleEndedIterator for ScanIterator<'a, E> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.try_next_back().transpose()
    }
}

/// An iterator that decodes raw engine key/value pairs into MVCC key/value
/// versions, and skips invisible versions. Helper for ScanIterator.
struct VersionIterator<'a, E: Engine + 'a> {
    /// The transaction the scan is running in.
    txn: &'a TransactionState,
    /// The inner engine scan iterator.
    inner: E::ScanIterator<'a>,
}

#[allow(clippy::type_complexity)]
impl<'a, E: Engine + 'a> VersionIterator<'a, E> {
    /// Creates a new MVCC version iterator for the given engine iterator.
    fn new(txn: &'a TransactionState, inner: E::ScanIterator<'a>) -> Self {
        Self { txn, inner }
    }

    /// Decodes a raw engine key into an MVCC key and version, returning None if
    /// the version is not visible.
    fn decode_visible(&self, key: &[u8]) -> Result<Option<(Vec<u8>, Version)>> {
        let (key, version) = match Key::decode(key)? {
            Key::Version(key, version) => (key.into_owned(), version),
            key => return Err(Error::Internal(format!("Expected Key::Version got {:?}", key))),
        };
        if self.txn.is_visible(version) {
            Ok(Some((key, version)))
        } else {
            Ok(None)
        }
    }

    // Fallible next(), emitting the next item, or None if exhausted.
    fn try_next(&mut self) -> Result<Option<(Vec<u8>, Version, Vec<u8>)>> {
        while let Some((key, value)) = self.inner.next().transpose()? {
            if let Some((key, version)) = self.decode_visible(&key)? {
                return Ok(Some((key, version, value)));
            }
        }
        Ok(None)
    }

    // Fallible next_back(), emitting the previous item, or None if exhausted.
    fn try_next_back(&mut self) -> Result<Option<(Vec<u8>, Version, Vec<u8>)>> {
        while let Some((key, value)) = self.inner.next_back().transpose()? {
            if let Some((key, version)) = self.decode_visible(&key)? {
                return Ok(Some((key, version, value)));
            }
        }
        Ok(None)
    }
}

impl<'a, E: Engine> Iterator for VersionIterator<'a, E> {
    type Item = Result<(Vec<u8>, Version, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().transpose()
    }
}

impl<'a, E: Engine> DoubleEndedIterator for VersionIterator<'a, E> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.try_next_back().transpose()
    }
}

#[cfg(test)]
pub mod tests {
    use super::super::engine::Memory;
    use super::*;

    fn setup() -> MVCC<Memory> {
        MVCC::new(Memory::new())
    }

    #[test]
    /// Tests that key prefixes are actually prefixes of keys.
    fn test_key_prefix() -> Result<()> {
        let cases = vec![
            (KeyPrefix::NextVersion, Key::NextVersion),
            (KeyPrefix::TxnActive, Key::TxnActive(1)),
            (KeyPrefix::TxnActiveSnapshot, Key::TxnActiveSnapshot(1)),
            (KeyPrefix::TxnWrite(1), Key::TxnWrite(1, b"foo".as_slice().into())),
            (
                KeyPrefix::Version(b"foo".as_slice().into()),
                Key::Version(b"foo".as_slice().into(), 1),
            ),
            (KeyPrefix::Unversioned, Key::Unversioned(b"foo".as_slice().into())),
        ];

        for (prefix, key) in cases {
            let prefix = prefix.encode()?;
            let key = key.encode()?;
            assert_eq!(prefix, key[..prefix.len()])
        }
        Ok(())
    }

    #[test]
    fn test_begin() -> Result<()> {
        let mvcc = setup();

        let txn = mvcc.begin()?;
        assert_eq!(1, txn.version());
        assert!(!txn.read_only());
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(2, txn.version());
        txn.rollback()?;

        let txn = mvcc.begin()?;
        assert_eq!(3, txn.version());
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_begin_read_only() -> Result<()> {
        let mvcc = setup();
        let txn = mvcc.begin_read_only()?;
        assert_eq!(txn.version(), 1);
        assert!(txn.read_only());
        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_begin_as_of() -> Result<()> {
        let mvcc = setup();

        // Start a concurrent transaction that should be invisible.
        let mut t1 = mvcc.begin()?;
        t1.set(b"other", vec![1])?;

        // Write a couple of versions for a key. Commit the concurrent one in between.
        let mut t2 = mvcc.begin()?;
        t2.set(b"key", vec![2])?;
        t2.commit()?;

        let mut t3 = mvcc.begin()?;
        t3.set(b"key", vec![3])?;
        t3.commit()?;

        t1.commit()?;

        let mut t4 = mvcc.begin()?;
        t4.set(b"key", vec![4])?;
        t4.commit()?;

        // Check that we can start a snapshot as of version 3. It should see
        // key=2 and other=None (because it hadn't committed yet).
        let txn = mvcc.begin_as_of(3)?;
        assert_eq!(txn.version(), 3);
        assert!(txn.read_only());
        assert_eq!(txn.get(b"key")?, Some(vec![2]));
        assert_eq!(txn.get(b"other")?, None);
        txn.commit()?;

        // A snapshot as of version 4 should see key=3 and Other=2.
        let txn = mvcc.begin_as_of(4)?;
        assert_eq!(txn.version(), 4);
        assert!(txn.read_only());
        assert_eq!(txn.get(b"key")?, Some(vec![3]));
        assert_eq!(txn.get(b"other")?, Some(vec![1]));
        txn.commit()?;

        // Check that any future versions are invalid.
        assert_eq!(
            mvcc.begin_as_of(9).err(),
            Some(Error::Value("Version 9 does not exist".into()))
        );

        Ok(())
    }

    #[test]
    fn test_resume() -> Result<()> {
        let mvcc = setup();

        // We first write a set of values that should be visible
        let mut t1 = mvcc.begin()?;
        t1.set(b"a", b"t1".to_vec())?;
        t1.set(b"b", b"t1".to_vec())?;
        t1.commit()?;

        // We then start three transactions, of which we will resume t3.
        // We commit t2 and t4's changes, which should not be visible,
        // and write a change for t3 which should be visible.
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;
        let mut t4 = mvcc.begin()?;

        t2.set(b"a", b"t2".to_vec())?;
        t3.set(b"b", b"t3".to_vec())?;
        t4.set(b"c", b"t4".to_vec())?;

        t2.commit()?;
        t4.commit()?;

        // We now resume t3, who should see it's own changes but none
        // of the others'
        let state = t3.state().clone();
        std::mem::drop(t3);
        let tr = mvcc.resume(state.clone())?;
        assert_eq!(3, tr.version());
        assert!(!tr.read_only());

        assert_eq!(Some(b"t1".to_vec()), tr.get(b"a")?);
        assert_eq!(Some(b"t3".to_vec()), tr.get(b"b")?);
        assert_eq!(None, tr.get(b"c")?);

        // A separate transaction should not see t3's changes, but should see the others
        let t = mvcc.begin()?;
        assert_eq!(Some(b"t2".to_vec()), t.get(b"a")?);
        assert_eq!(Some(b"t1".to_vec()), t.get(b"b")?);
        assert_eq!(Some(b"t4".to_vec()), t.get(b"c")?);
        t.rollback()?;

        // Once tr commits, a separate transaction should see t3's changes
        tr.commit()?;

        // Resuming an inactive transaction should error.
        assert_eq!(
            mvcc.resume(state).err(),
            Some(Error::Internal("No active transaction at version 3".into()))
        );

        let t = mvcc.begin()?;
        assert_eq!(Some(b"t2".to_vec()), t.get(b"a")?);
        assert_eq!(Some(b"t3".to_vec()), t.get(b"b")?);
        assert_eq!(Some(b"t4".to_vec()), t.get(b"c")?);
        t.rollback()?;

        // It should also be possible to start a snapshot transaction and resume it.
        let ts = mvcc.begin_as_of(2)?;
        assert_eq!(2, ts.version());
        assert_eq!(Some(b"t1".to_vec()), ts.get(b"a")?);

        let state = ts.state().clone();
        std::mem::drop(ts);
        let ts = mvcc.resume(state)?;
        assert_eq!(2, ts.version());
        assert!(ts.read_only());
        assert_eq!(Some(b"t1".to_vec()), ts.get(b"a")?);
        ts.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_delete_conflict() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"key", vec![0x00])?;
        txn.commit()?;

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.delete(b"key")?;
        assert_eq!(Err(Error::Serialization), t1.delete(b"key"));
        assert_eq!(Err(Error::Serialization), t3.delete(b"key"));
        t2.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_delete_idempotent() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.delete(b"key")?;
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_get() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        assert_eq!(None, txn.get(b"a")?);
        txn.set(b"a", vec![0x01])?;
        assert_eq!(Some(vec![0x01]), txn.get(b"a")?);
        txn.set(b"a", vec![0x02])?;
        assert_eq!(Some(vec![0x02]), txn.get(b"a")?);
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_get_deleted() -> Result<()> {
        let mvcc = setup();
        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.delete(b"a")?;
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(None, txn.get(b"a")?);
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_get_hides_newer() -> Result<()> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t1.set(b"a", vec![0x01])?;
        t1.commit()?;
        t3.set(b"c", vec![0x03])?;
        t3.commit()?;

        assert_eq!(None, t2.get(b"a")?);
        assert_eq!(None, t2.get(b"c")?);

        Ok(())
    }

    #[test]
    fn test_txn_get_hides_uncommitted() -> Result<()> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        t1.set(b"a", vec![0x01])?;
        let t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;
        t3.set(b"c", vec![0x03])?;

        assert_eq!(None, t2.get(b"a")?);
        assert_eq!(None, t2.get(b"c")?);

        Ok(())
    }

    #[test]
    fn test_txn_get_readonly_historical() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.set(b"b", vec![0x02])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.set(b"c", vec![0x03])?;
        txn.commit()?;

        let tr = mvcc.begin_as_of(3)?;
        assert_eq!(Some(vec![0x01]), tr.get(b"a")?);
        assert_eq!(Some(vec![0x02]), tr.get(b"b")?);
        assert_eq!(None, tr.get(b"c")?);

        Ok(())
    }

    #[test]
    fn test_txn_get_serial() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(Some(vec![0x01]), txn.get(b"a")?);

        Ok(())
    }

    #[test]
    fn test_txn_scan() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.delete(b"b")?;
        txn.set(b"c", vec![0x01])?;
        txn.set(b"d", vec![0x01])?;
        txn.set(b"e", vec![0x01])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.set(b"c", vec![0x02])?;
        txn.set(b"d", vec![0x02])?;
        txn.set(b"e", vec![0x02])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.delete(b"c")?;
        txn.set(b"d", vec![0x03])?;
        txn.set(b"e", vec![0x03])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.set(b"c", vec![0x04])?;
        txn.set(b"d", vec![0x04])?;
        txn.delete(b"e")?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.delete(b"d")?;
        txn.set(b"e", vec![0x05])?;
        txn.commit()?;

        // Forward scan
        let txn = mvcc.begin()?;
        assert_eq!(
            vec![
                (b"a".to_vec(), vec![0x01]),
                (b"c".to_vec(), vec![0x04]),
                (b"e".to_vec(), vec![0x05]),
            ],
            txn.scan(..)?.to_vec()?
        );

        // Reverse scan
        assert_eq!(
            vec![
                (b"e".to_vec(), vec![0x05]),
                (b"c".to_vec(), vec![0x04]),
                (b"a".to_vec(), vec![0x01]),
            ],
            txn.scan(..)?.iter().rev().collect::<Result<Vec<_>>>()?
        );

        // Alternate forward/backward scan
        let mut scan = txn.scan(..)?;
        let mut iter = scan.iter();
        assert_eq!(Some((b"a".to_vec(), vec![0x01])), iter.next().transpose()?);
        assert_eq!(Some((b"e".to_vec(), vec![0x05])), iter.next_back().transpose()?);
        assert_eq!(Some((b"c".to_vec(), vec![0x04])), iter.next_back().transpose()?);
        assert_eq!(None, iter.next().transpose()?);
        drop(scan);

        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_txn_scan_key_version_overlap() -> Result<()> {
        // The idea here is that with a naive key/version concatenation
        // we get overlapping entries that mess up scans. For example:
        //
        // 00|00 00 00 00 00 00 00 01
        // 00 00 00 00 00 00 00 00 02|00 00 00 00 00 00 00 02
        // 00|00 00 00 00 00 00 00 03
        //
        // The key encoding should be resistant to this.
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(&[0], vec![0])?; // v0
        txn.set(&[0], vec![1])?; // v1
        txn.set(&[0, 0, 0, 0, 0, 0, 0, 0, 2], vec![2])?; // v2
        txn.set(&[0], vec![3])?; // v3
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(
            vec![(vec![0].to_vec(), vec![3]), (vec![0, 0, 0, 0, 0, 0, 0, 0, 2].to_vec(), vec![2]),],
            txn.scan(..)?.to_vec()?,
        );
        Ok(())
    }

    #[test]
    fn test_txn_scan_prefix() -> Result<()> {
        let mvcc = setup();
        let mut txn = mvcc.begin()?;

        txn.set(b"a", vec![0x01])?;
        txn.set(b"az", vec![0x01, 0x1a])?;
        txn.set(b"b", vec![0x02])?;
        txn.set(b"ba", vec![0x02, 0x01])?;
        txn.set(b"bb", vec![0x02, 0x02])?;
        txn.set(b"bc", vec![0x02, 0x03])?;
        txn.set(b"c", vec![0x03])?;
        txn.commit()?;

        // Forward scan
        let txn = mvcc.begin()?;
        assert_eq!(
            vec![
                (b"b".to_vec(), vec![0x02]),
                (b"ba".to_vec(), vec![0x02, 0x01]),
                (b"bb".to_vec(), vec![0x02, 0x02]),
                (b"bc".to_vec(), vec![0x02, 0x03]),
            ],
            txn.scan_prefix(b"b")?.to_vec()?,
        );

        // Reverse scan
        assert_eq!(
            vec![
                (b"bc".to_vec(), vec![0x02, 0x03]),
                (b"bb".to_vec(), vec![0x02, 0x02]),
                (b"ba".to_vec(), vec![0x02, 0x01]),
                (b"b".to_vec(), vec![0x02]),
            ],
            txn.scan_prefix(b"b")?.iter().rev().collect::<Result<Vec<_>>>()?
        );

        // Alternate forward/backward scan
        let mut scan = txn.scan_prefix(b"b")?;
        let mut iter = scan.iter();
        assert_eq!(Some((b"b".to_vec(), vec![0x02])), iter.next().transpose()?);
        assert_eq!(Some((b"bc".to_vec(), vec![0x02, 0x03])), iter.next_back().transpose()?);
        assert_eq!(Some((b"bb".to_vec(), vec![0x02, 0x02])), iter.next_back().transpose()?);
        assert_eq!(Some((b"ba".to_vec(), vec![0x02, 0x01])), iter.next().transpose()?);
        assert_eq!(None, iter.next().transpose()?);
        drop(scan);

        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_txn_set_conflict() -> Result<()> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.set(b"key", vec![0x02])?;
        assert_eq!(Err(Error::Serialization), t1.set(b"key", vec![0x01]));
        assert_eq!(Err(Error::Serialization), t3.set(b"key", vec![0x03]));
        t2.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_set_conflict_committed() -> Result<()> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.set(b"key", vec![0x02])?;
        t2.commit()?;
        assert_eq!(Err(Error::Serialization), t1.set(b"key", vec![0x01]));
        assert_eq!(Err(Error::Serialization), t3.set(b"key", vec![0x03]));

        Ok(())
    }

    #[test]
    fn test_txn_set_rollback() -> Result<()> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"key", vec![0x00])?;
        txn.commit()?;

        let t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.set(b"key", vec![0x02])?;
        t2.rollback()?;
        assert_eq!(Some(vec![0x00]), t1.get(b"key")?);
        t1.commit()?;
        t3.set(b"key", vec![0x03])?;
        t3.commit()?;

        Ok(())
    }

    #[test]
    // A dirty write is when t2 overwrites an uncommitted value written by t1.
    fn test_txn_anomaly_dirty_write() -> Result<()> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        t1.set(b"key", b"t1".to_vec())?;
        assert_eq!(t2.set(b"key", b"t2".to_vec()), Err(Error::Serialization));

        Ok(())
    }

    #[test]
    // A dirty read is when t2 can read an uncommitted value set by t1.
    fn test_txn_anomaly_dirty_read() -> Result<()> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let t2 = mvcc.begin()?;

        t1.set(b"key", b"t1".to_vec())?;
        assert_eq!(None, t2.get(b"key")?);

        Ok(())
    }

    #[test]
    // A lost update is when t1 and t2 both read a value and update it, where t2's update replaces t1.
    fn test_txn_anomaly_lost_update() -> Result<()> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"key", b"t0".to_vec())?;
        t0.commit()?;

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        t1.get(b"key")?;
        t2.get(b"key")?;

        t1.set(b"key", b"t1".to_vec())?;
        assert_eq!(t2.set(b"key", b"t2".to_vec()), Err(Error::Serialization));

        Ok(())
    }

    #[test]
    // A fuzzy (or unrepeatable) read is when t2 sees a value change after t1 updates it.
    fn test_txn_anomaly_fuzzy_read() -> Result<()> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"key", b"t0".to_vec())?;
        t0.commit()?;

        let mut t1 = mvcc.begin()?;
        let t2 = mvcc.begin()?;

        assert_eq!(Some(b"t0".to_vec()), t2.get(b"key")?);
        t1.set(b"key", b"t1".to_vec())?;
        t1.commit()?;
        assert_eq!(Some(b"t0".to_vec()), t2.get(b"key")?);

        Ok(())
    }

    #[test]
    // Read skew is when t1 reads a and b, but t2 modifies b in between the reads.
    fn test_txn_anomaly_read_skew() -> Result<()> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"a", b"t0".to_vec())?;
        t0.set(b"b", b"t0".to_vec())?;
        t0.commit()?;

        let t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        assert_eq!(Some(b"t0".to_vec()), t1.get(b"a")?);
        t2.set(b"a", b"t2".to_vec())?;
        t2.set(b"b", b"t2".to_vec())?;
        t2.commit()?;
        assert_eq!(Some(b"t0".to_vec()), t1.get(b"b")?);

        Ok(())
    }

    #[test]
    // A phantom read is when t1 reads entries matching some predicate, but a modification by
    // t2 changes the entries that match the predicate such that a later read by t1 returns them.
    fn test_txn_anomaly_phantom_read() -> Result<()> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"a", b"true".to_vec())?;
        t0.set(b"b", b"false".to_vec())?;
        t0.commit()?;

        let t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        assert_eq!(Some(b"true".to_vec()), t1.get(b"a")?);
        assert_eq!(Some(b"false".to_vec()), t1.get(b"b")?);

        t2.set(b"b", b"true".to_vec())?;
        t2.commit()?;

        assert_eq!(Some(b"true".to_vec()), t1.get(b"a")?);
        assert_eq!(Some(b"false".to_vec()), t1.get(b"b")?);

        Ok(())
    }

    /* FIXME To avoid write skew we need to implement serializable snapshot isolation.
    #[test]
    // Write skew is when t1 reads b and writes it to a while t2 reads a and writes it to b.¨
    fn test_txn_anomaly_write_skew() -> Result<()> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"a", b"1".to_vec())?;
        t0.set(b"b", b"2".to_vec())?;
        t0.commit()?;

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        assert_eq!(Some(b"1".to_vec()), t1.get(b"a")?);
        assert_eq!(Some(b"2".to_vec()), t2.get(b"b")?);

        // Some of the following operations should error
        t1.set(b"a", b"2".to_vec())?;
        t2.set(b"b", b"1".to_vec())?;

        t1.commit()?;
        t2.commit()?;

        Ok(())
    }*/

    #[test]
    /// Tests unversioned key/value pairs, via set/get_unversioned().
    fn test_unversioned() -> Result<()> {
        let m = setup();

        // Unversioned keys should not interact with versioned keys.
        let mut txn = m.begin()?;
        txn.set(b"foo", b"bar".to_vec())?;
        txn.commit()?;

        // The unversioned key should return None.
        assert_eq!(m.get_unversioned(b"foo")?, None);

        // Setting and then fetching the unversioned key should return its value.
        m.set_unversioned(b"foo", b"bar".to_vec())?;
        assert_eq!(m.get_unversioned(b"foo")?, Some(b"bar".to_vec()));

        // Replacing it should return the new value.
        m.set_unversioned(b"foo", b"baz".to_vec())?;
        assert_eq!(m.get_unversioned(b"foo")?, Some(b"baz".to_vec()));

        // The versioned key should remain unaffected.
        let txn = m.begin_read_only()?;
        assert_eq!(txn.get(b"foo")?, Some(b"bar".to_vec()));

        Ok(())
    }
}