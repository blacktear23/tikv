// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

//! Scheduler which schedules the execution of `storage::Command`s.
//!
//! There is one scheduler for each store. It receives commands from clients, executes them against
//! the MVCC layer storage engine.
//!
//! Logically, the data organization hierarchy from bottom to top is row -> region -> store ->
//! database. But each region is replicated onto N stores for reliability, the replicas form a Raft
//! group, one of which acts as the leader. When the client read or write a row, the command is
//! sent to the scheduler which is on the region leader's store.
//!
//! Scheduler runs in a single-thread event loop, but command executions are delegated to a pool of
//! worker thread.
//!
//! Scheduler keeps track of all the running commands and uses latches to ensure serialized access
//! to the overlapping rows involved in concurrent commands. But note that scheduler only ensures
//! serialized access to the overlapping rows at command level, but a transaction may consist of
//! multiple commands, therefore conflicts may happen at transaction level. Transaction semantics
//! is ensured by the transaction protocol implemented in the client library, which is transparent
//! to the scheduler.

use std::fmt::{self, Debug, Formatter};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use std::thread;
use std::hash::{Hash, Hasher};
use std::u64;

use prometheus::HistogramTimer;
use kvproto::kvrpcpb::{CommandPri, Context, LockInfo};

use storage::{Command, Engine, Error as StorageError, Result as StorageResult, ScanMode, Snapshot,
              Statistics, StatisticsSummary, StorageCb};
use storage::mvcc::{Error as MvccError, Lock as MvccLock, MvccReader, MvccTxn, Write, WriteType,
                    MAX_TXN_WRITE_SIZE};
use storage::{Key, KvPair, MvccInfo, Value, CMD_TAG_GC};
use storage::engine::{self, Callback as EngineCallback, CbContext, Error as EngineError, Modify,
                      Result as EngineResult};
use raftstore::store::engine::IterOption;
use util::transport::{Error as TransportError, SyncSendCh};
use util::threadpool::{Context as ThreadContext, ThreadPool, ThreadPoolBuilder};
use util::time::SlowTimer;
use util::collections::HashMap;

use super::Result;
use super::Error;
use super::store::SnapshotStore;
use super::latch::{Latches, Lock};
use super::super::metrics::*;

// TODO: make it configurable.
pub const GC_BATCH_SIZE: usize = 512;

pub const RESOLVE_LOCK_BATCH_SIZE: usize = 512;

/// Process result of a command.
pub enum ProcessResult {
    Res,
    MultiRes { results: Vec<StorageResult<()>> },
    MultiKvpairs { pairs: Vec<StorageResult<KvPair>> },
    MvccKey { mvcc: MvccInfo },
    MvccStartTs { mvcc: Option<(Key, MvccInfo)> },
    Value { value: Option<Value> },
    Locks { locks: Vec<LockInfo> },
    NextCommand { cmd: Command },
    Failed { err: StorageError },
}

type SnapshotResult = (Vec<u64>, CbContext, EngineResult<Box<Snapshot>>);

/// Message types for the scheduler event loop.
pub enum Msg {
    Quit,
    RawCmd { cmd: Command, cb: StorageCb },
    RetryGetSnapshots(Vec<(Context, Vec<u64>)>),
    SnapshotFinished {
        cids: Vec<u64>,
        cb_ctx: CbContext,
        snapshot: EngineResult<Box<Snapshot>>,
    },
    BatchSnapshotFinished { batch: Vec<SnapshotResult> },
    ReadFinished { cid: u64, pr: ProcessResult },
    WritePrepareFinished {
        cid: u64,
        cmd: Command,
        pr: ProcessResult,
        to_be_write: Vec<Modify>,
    },
    WritePrepareFailed { cid: u64, err: Error },
    WriteFinished {
        cid: u64,
        pr: ProcessResult,
        cb_ctx: CbContext,
        result: EngineResult<()>,
    },
}

/// Debug for messages.
impl Debug for Msg {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Msg::Quit => write!(f, "Quit"),
            Msg::RawCmd { ref cmd, .. } => write!(f, "RawCmd {:?}", cmd),
            Msg::RetryGetSnapshots(ref tasks) => write!(f, "RetryGetSnapshots {:?}", tasks),
            Msg::SnapshotFinished { ref cids, .. } => {
                write!(f, "SnapshotFinished [cids={:?}]", cids)
            }
            Msg::BatchSnapshotFinished { ref batch } => {
                let ids: Vec<&Vec<_>> = batch.iter().map(|&(ref ids, _, _)| ids).collect();
                write!(f, "BatchSnapshotFinished cids: {:?}", ids)
            }
            Msg::ReadFinished { cid, .. } => write!(f, "ReadFinished [cid={}]", cid),
            Msg::WritePrepareFinished { cid, ref cmd, .. } => {
                write!(f, "WritePrepareFinished [cid={}, cmd={:?}]", cid, cmd)
            }
            Msg::WritePrepareFailed { cid, ref err } => {
                write!(f, "WritePrepareFailed [cid={}, err={:?}]", cid, err)
            }
            Msg::WriteFinished { cid, .. } => write!(f, "WriteFinished [cid={}]", cid),
        }
    }
}

/// Delivers the process result of a command to the storage callback.
fn execute_callback(callback: StorageCb, pr: ProcessResult) {
    match callback {
        StorageCb::Boolean(cb) => match pr {
            ProcessResult::Res => cb(Ok(())),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
        StorageCb::Booleans(cb) => match pr {
            ProcessResult::MultiRes { results } => cb(Ok(results)),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
        StorageCb::SingleValue(cb) => match pr {
            ProcessResult::Value { value } => cb(Ok(value)),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
        StorageCb::KvPairs(cb) => match pr {
            ProcessResult::MultiKvpairs { pairs } => cb(Ok(pairs)),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
        StorageCb::MvccInfoByKey(cb) => match pr {
            ProcessResult::MvccKey { mvcc } => cb(Ok(mvcc)),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
        StorageCb::MvccInfoByStartTs(cb) => match pr {
            ProcessResult::MvccStartTs { mvcc } => cb(Ok(mvcc)),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
        StorageCb::Locks(cb) => match pr {
            ProcessResult::Locks { locks } => cb(Ok(locks)),
            ProcessResult::Failed { err } => cb(Err(err)),
            _ => panic!("process result mismatch"),
        },
    }
}

/// Context for a running command.
pub struct RunningCtx {
    cid: u64,
    cmd: Option<Command>,
    lock: Lock,
    callback: Option<StorageCb>,
    tag: &'static str,
    ts: u64,
    region_id: u64,
    latch_timer: Option<HistogramTimer>,
    _timer: HistogramTimer,
    slow_timer: SlowTimer,
}

impl RunningCtx {
    /// Creates a context for a running command.
    pub fn new(cid: u64, cmd: Command, lock: Lock, cb: StorageCb) -> RunningCtx {
        let tag = cmd.tag();
        let ts = cmd.ts();
        let region_id = cmd.get_context().get_region_id();
        RunningCtx {
            cid: cid,
            cmd: Some(cmd),
            lock: lock,
            callback: Some(cb),
            tag: tag,
            ts: ts,
            region_id: region_id,
            latch_timer: Some(
                SCHED_LATCH_HISTOGRAM_VEC
                    .with_label_values(&[tag])
                    .start_coarse_timer(),
            ),
            _timer: SCHED_HISTOGRAM_VEC
                .with_label_values(&[tag])
                .start_coarse_timer(),
            slow_timer: SlowTimer::new(),
        }
    }
}

impl Drop for RunningCtx {
    fn drop(&mut self) {
        slow_log!(
            self.slow_timer,
            "[region {}] scheduler handle command: {}, ts: {}",
            self.region_id,
            self.tag,
            self.ts
        );
    }
}

/// Creates a callback to receive async results of write prepare from the storage engine.
fn make_engine_cb(cid: u64, pr: ProcessResult, ch: SyncSendCh<Msg>) -> EngineCallback<()> {
    Box::new(move |(cb_ctx, result)| {
        match ch.send(Msg::WriteFinished {
            cid: cid,
            pr: pr,
            cb_ctx: cb_ctx,
            result: result,
        }) {
            Ok(_) => {}
            e @ Err(TransportError::Closed) => info!("channel closed, err {:?}", e),
            Err(e) => {
                panic!(
                    "send write finished to scheduler failed cid={}, err:{:?}",
                    cid,
                    e
                );
            }
        }
    })
}

#[derive(Clone)]
struct HashableContext(Context);

impl PartialEq for HashableContext {
    fn eq(&self, other: &HashableContext) -> bool {
        // k1 == k2 ⇒ hash(k1) == hash(k2)
        self.0.get_region_id() == other.0.get_region_id() &&
            self.0.get_region_epoch().get_version() == other.0.get_region_epoch().get_version() &&
            self.0.get_peer().get_id() == other.0.get_peer().get_id()
    }
}

impl Hash for HashableContext {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let key = {
            let ctx = &self.0;
            (
                ctx.get_region_id(),
                ctx.get_region_epoch().get_version(),
                ctx.get_peer().get_id(),
            )
        };
        Hash::hash(&key, state);
    }
}

impl Eq for HashableContext {}

/// Scheduler which schedules the execution of `storage::Command`s.
pub struct Scheduler {
    engine: Box<Engine>,

    // cid -> RunningCtx
    cmd_ctxs: HashMap<u64, RunningCtx>,
    // Context -> cids
    grouped_cmds: Option<HashMap<HashableContext, Vec<u64>>>,

    schedch: SyncSendCh<Msg>,

    // cmd id generator
    id_alloc: u64,

    // write concurrency control
    latches: Latches,

    sched_too_busy_threshold: usize,

    // worker pool
    worker_pool: ThreadPool<ScheContext>,

    // high priority commands will be delivered to this pool
    high_priority_pool: ThreadPool<ScheContext>,

    has_gc_command: bool,

    // used to control write flow
    running_write_count: usize,
}

// Make clippy happy.
type MultipleReturnValue = (Option<MvccLock>, Vec<(u64, Write)>, Vec<(u64, bool, Value)>);

fn find_mvcc_infos_by_key(
    reader: &mut MvccReader,
    key: &Key,
    mut ts: u64,
) -> Result<MultipleReturnValue> {
    let mut writes = vec![];
    let mut values = vec![];
    let lock = try!(reader.load_lock(key));
    loop {
        let opt = try!(reader.seek_write(key, ts));
        let short_value: Option<Value>;
        match opt {
            Some((commit_ts, mut write)) => {
                ts = commit_ts - 1;
                let write_type = write.write_type;
                short_value = write.short_value.take();
                writes.push((commit_ts, write));
                if write_type != WriteType::Put {
                    continue;
                }
            }
            None => break,
        };
        let write = &writes[writes.len() - 1].1;
        if let Some(v) = short_value {
            values.push((write.start_ts, true, v));
        }
    }
    for (ts, v) in try!(reader.scan_values_in_default(key)) {
        values.push((ts, false, v));
    }
    Ok((lock, writes, values))
}

impl Scheduler {
    /// Creates a scheduler.
    pub fn new(
        engine: Box<Engine>,
        schedch: SyncSendCh<Msg>,
        concurrency: usize,
        worker_pool_size: usize,
        sched_too_busy_threshold: usize,
    ) -> Scheduler {
        Scheduler {
            engine: engine,
            cmd_ctxs: Default::default(),
            grouped_cmds: Some(HashMap::with_capacity_and_hasher(
                CMD_BATCH_SIZE,
                Default::default(),
            )),
            schedch: schedch,
            id_alloc: 0,
            latches: Latches::new(concurrency),
            sched_too_busy_threshold: sched_too_busy_threshold,
            worker_pool: ThreadPoolBuilder::with_default_factory(thd_name!("sched-worker-pool"))
                .thread_count(worker_pool_size)
                .build(),
            high_priority_pool: ThreadPoolBuilder::with_default_factory(
                thd_name!("sched-high-pri-pool"),
            ).build(),
            has_gc_command: false,
            running_write_count: 0,
        }
    }
}

/// Processes a read command within a worker thread, then posts `ReadFinished` message back to the
/// event loop.
fn process_read(
    cid: u64,
    mut cmd: Command,
    ch: SyncSendCh<Msg>,
    snapshot: Box<Snapshot>,
) -> Statistics {
    debug!("process read cmd(cid={}) in worker pool.", cid);
    SCHED_WORKER_COUNTER_VEC
        .with_label_values(&[cmd.tag(), "read"])
        .inc();
    let tag = cmd.tag();

    let mut statistics = Statistics::default();

    let pr = match cmd {
        // Gets from the snapshot.
        Command::Get {
            ref ctx,
            ref key,
            start_ts,
            ..
        } => {
            KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                .with_label_values(&[tag])
                .observe(1f64);
            let snap_store = SnapshotStore::new(
                snapshot.as_ref(),
                start_ts,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            let res = snap_store.get(key, &mut statistics);
            match res {
                Ok(val) => ProcessResult::Value { value: val },
                Err(e) => ProcessResult::Failed {
                    err: StorageError::from(e),
                },
            }
        }
        // Batch gets from the snapshot.
        Command::BatchGet {
            ref ctx,
            ref keys,
            start_ts,
            ..
        } => {
            KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                .with_label_values(&[tag])
                .observe(keys.len() as f64);
            let snap_store = SnapshotStore::new(
                snapshot.as_ref(),
                start_ts,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            match snap_store.batch_get(keys, &mut statistics) {
                Ok(results) => {
                    let mut res = vec![];
                    for (k, v) in keys.into_iter().zip(results) {
                        match v {
                            Ok(Some(x)) => res.push(Ok((k.raw().unwrap(), x))),
                            Ok(None) => {}
                            Err(e) => res.push(Err(StorageError::from(e))),
                        }
                    }
                    ProcessResult::MultiKvpairs { pairs: res }
                }
                Err(e) => ProcessResult::Failed {
                    err: StorageError::from(e),
                },
            }
        }
        // Scans a range starting with `start_key` up to `limit` rows from the snapshot.
        Command::Scan {
            ref ctx,
            ref start_key,
            limit,
            start_ts,
            ref options,
            ..
        } => {
            let snap_store = SnapshotStore::new(
                snapshot.as_ref(),
                start_ts,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            let res = snap_store
                .scanner(ScanMode::Forward, options.key_only, None, &mut statistics)
                .and_then(|mut scanner| scanner.scan(start_key.clone(), limit))
                .and_then(|mut results| {
                    KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                        .with_label_values(&[tag])
                        .observe(results.len() as f64);
                    Ok(
                        results
                            .drain(..)
                            .map(|x| x.map_err(StorageError::from))
                            .collect(),
                    )
                });

            match res {
                Ok(pairs) => ProcessResult::MultiKvpairs { pairs: pairs },
                Err(e) => ProcessResult::Failed { err: e.into() },
            }
        }
        Command::MvccByKey { ref ctx, ref key } => {
            let mut reader = MvccReader::new(
                snapshot.as_ref(),
                &mut statistics,
                Some(ScanMode::Forward),
                !ctx.get_not_fill_cache(),
                None,
                ctx.get_isolation_level(),
            );
            match find_mvcc_infos_by_key(&mut reader, key, u64::MAX) {
                Ok((lock, writes, values)) => ProcessResult::MvccKey {
                    mvcc: MvccInfo {
                        lock: lock,
                        writes: writes,
                        values: values,
                    },
                },
                Err(e) => ProcessResult::Failed { err: e.into() },
            }
        }
        Command::MvccByStartTs { ref ctx, start_ts } => {
            let mut reader = MvccReader::new(
                snapshot.as_ref(),
                &mut statistics,
                Some(ScanMode::Forward),
                !ctx.get_not_fill_cache(),
                None,
                ctx.get_isolation_level(),
            );
            match reader.seek_ts(start_ts).map_err(StorageError::from) {
                Err(e) => ProcessResult::Failed { err: e.into() },
                Ok(opt) => match opt {
                    Some(key) => match find_mvcc_infos_by_key(&mut reader, &key, u64::MAX) {
                        Ok((lock, writes, values)) => ProcessResult::MvccStartTs {
                            mvcc: Some((
                                key,
                                MvccInfo {
                                    lock: lock,
                                    writes: writes,
                                    values: values,
                                },
                            )),
                        },
                        Err(e) => ProcessResult::Failed { err: e.into() },
                    },
                    None => ProcessResult::MvccStartTs { mvcc: None },
                },
            }
        }
        // Scans locks with timestamp <= `max_ts`
        Command::ScanLock {
            ref ctx, max_ts, ..
        } => {
            let mut reader = MvccReader::new(
                snapshot.as_ref(),
                &mut statistics,
                Some(ScanMode::Forward),
                !ctx.get_not_fill_cache(),
                None,
                ctx.get_isolation_level(),
            );
            let res = reader
                .scan_lock(None, |lock| lock.ts <= max_ts, None)
                .map_err(Error::from)
                .and_then(|(v, _)| {
                    let mut locks = vec![];
                    for (key, lock) in v {
                        let mut lock_info = LockInfo::new();
                        lock_info.set_primary_lock(lock.primary);
                        lock_info.set_lock_version(lock.ts);
                        lock_info.set_key(try!(key.raw()));
                        locks.push(lock_info);
                    }
                    KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                        .with_label_values(&[tag])
                        .observe(locks.len() as f64);
                    Ok(locks)
                });
            match res {
                Ok(locks) => ProcessResult::Locks { locks: locks },
                Err(e) => ProcessResult::Failed { err: e.into() },
            }
        }
        // Scan the locks with timestamp `start_ts`, then either commit them if the command has
        // commit timestamp populated or rollback otherwise.
        Command::ResolveLock {
            ref ctx,
            start_ts,
            commit_ts,
            ref mut scan_key,
            ..
        } => {
            let mut reader = MvccReader::new(
                snapshot.as_ref(),
                &mut statistics,
                Some(ScanMode::Forward),
                !ctx.get_not_fill_cache(),
                None,
                ctx.get_isolation_level(),
            );
            let res = reader
                .scan_lock(
                    scan_key.take(),
                    |lock| lock.ts == start_ts,
                    Some(RESOLVE_LOCK_BATCH_SIZE),
                )
                .map_err(Error::from)
                .and_then(|(v, next_scan_key)| {
                    let keys: Vec<Key> = v.into_iter().map(|x| x.0).collect();
                    KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                        .with_label_values(&[tag])
                        .observe(keys.len() as f64);
                    if keys.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(Command::ResolveLock {
                            ctx: ctx.clone(),
                            start_ts: start_ts,
                            commit_ts: commit_ts,
                            scan_key: next_scan_key,
                            keys: keys,
                        }))
                    }
                });
            match res {
                Ok(Some(cmd)) => ProcessResult::NextCommand { cmd: cmd },
                Ok(None) => ProcessResult::Res,
                Err(e) => ProcessResult::Failed { err: e.into() },
            }
        }
        // Collects garbage.
        Command::Gc {
            ref ctx,
            safe_point,
            ratio_threshold,
            ref mut scan_key,
            ..
        } => {
            let mut reader = MvccReader::new(
                snapshot.as_ref(),
                &mut statistics,
                Some(ScanMode::Forward),
                !ctx.get_not_fill_cache(),
                None,
                ctx.get_isolation_level(),
            );
            // scan_key is used as start_key here,and Range start gc with scan_key=none.
            let is_range_start_gc = scan_key.is_none();
            // This is an optimization to skip gc before scanning all data.
            let need_gc = if is_range_start_gc {
                reader.need_gc(safe_point, ratio_threshold)
            } else {
                true
            };
            let res = if !need_gc {
                KV_COMMAND_GC_SKIPPED_COUNTER.inc();
                Ok(None)
            } else {
                reader
                    .scan_keys(scan_key.take(), GC_BATCH_SIZE)
                    .map_err(Error::from)
                    .and_then(|(keys, next_start)| {
                        KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                            .with_label_values(&[tag])
                            .observe(keys.len() as f64);
                        if keys.is_empty() {
                            // empty range
                            if is_range_start_gc {
                                KV_COMMAND_GC_EMPTY_RANGE_COUNTER.inc();
                            }
                            Ok(None)
                        } else {
                            Ok(Some(Command::Gc {
                                ctx: ctx.clone(),
                                safe_point: safe_point,
                                ratio_threshold: ratio_threshold,
                                scan_key: next_start,
                                keys: keys,
                            }))
                        }
                    })
            };
            match res {
                Ok(Some(cmd)) => ProcessResult::NextCommand { cmd: cmd },
                Ok(None) => ProcessResult::Res,
                Err(e) => ProcessResult::Failed { err: e.into() },
            }
        }
        Command::RawGet { ref key, .. } => {
            KV_COMMAND_KEYREAD_HISTOGRAM_VEC
                .with_label_values(&[tag])
                .observe(1f64);
            match snapshot.get(key) {
                Ok(val) => ProcessResult::Value { value: val },
                Err(e) => ProcessResult::Failed {
                    err: StorageError::from(e),
                },
            }
        }
        Command::RawScan {
            ref start_key,
            limit,
            ..
        } => match process_rawscan(snapshot, start_key, limit, &mut statistics) {
            Ok(val) => ProcessResult::MultiKvpairs { pairs: val },
            Err(e) => ProcessResult::Failed {
                err: StorageError::from(e),
            },
        },
        Command::Pause { duration, .. } => {
            thread::sleep(Duration::from_millis(duration));
            ProcessResult::Res
        }
        _ => panic!("unsupported read command"),
    };

    if let Err(e) = ch.send(Msg::ReadFinished { cid: cid, pr: pr }) {
        // Todo: if this happens we need to clean up command's context
        panic!("send read finished failed, cid={}, err={:?}", cid, e);
    }
    statistics
}

fn process_rawscan(
    snapshot: Box<Snapshot>,
    start_key: &Key,
    limit: usize,
    stats: &mut Statistics,
) -> Result<Vec<StorageResult<KvPair>>> {
    let mut cursor = try!(snapshot.iter(IterOption::default(), ScanMode::Forward));
    if !try!(cursor.seek(start_key, &mut stats.data)) {
        return Ok(vec![]);
    }
    let mut pairs = vec![];
    while cursor.valid() && pairs.len() < limit {
        pairs.push(Ok((cursor.key().to_owned(), cursor.value().to_owned())));
        cursor.next(&mut stats.data);
    }
    Ok(pairs)
}

/// Processes a write command within a worker thread, then posts either a `WritePrepareFinished`
/// message if successful or a `WritePrepareFailed` message back to the event loop.
fn process_write(
    cid: u64,
    cmd: Command,
    ch: SyncSendCh<Msg>,
    snapshot: Box<Snapshot>,
) -> Statistics {
    let mut statistics = Statistics::default();
    SCHED_WORKER_COUNTER_VEC
        .with_label_values(&[cmd.tag(), "write"])
        .inc();
    if let Err(e) = process_write_impl(cid, cmd, ch.clone(), snapshot.as_ref(), &mut statistics) {
        if let Err(err) = ch.send(Msg::WritePrepareFailed { cid: cid, err: e }) {
            // Todo: if this happens, lock will hold for ever
            panic!(
                "send WritePrepareFailed message to channel failed. cid={}, err={:?}",
                cid,
                err
            );
        }
    }
    statistics
}

fn process_write_impl(
    cid: u64,
    mut cmd: Command,
    ch: SyncSendCh<Msg>,
    snapshot: &Snapshot,
    statistics: &mut Statistics,
) -> Result<()> {
    let (pr, modifies) = match cmd {
        Command::Prewrite {
            ref ctx,
            ref mutations,
            ref primary,
            start_ts,
            ref options,
            ..
        } => {
            let mut txn = MvccTxn::new(
                snapshot,
                statistics,
                start_ts,
                None,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            let mut locks = vec![];
            for m in mutations {
                match txn.prewrite(m.clone(), primary, options) {
                    Ok(_) => {}
                    e @ Err(MvccError::KeyIsLocked { .. }) => {
                        locks.push(e.map_err(Error::from).map_err(StorageError::from));
                    }
                    Err(e) => return Err(Error::from(e)),
                }
            }
            if locks.is_empty() {
                let pr = ProcessResult::MultiRes { results: vec![] };
                (pr, txn.modifies())
            } else {
                // Skip write stage if some keys are locked.
                let pr = ProcessResult::MultiRes { results: locks };
                (pr, vec![])
            }
        }
        Command::Commit {
            ref ctx,
            ref keys,
            lock_ts,
            commit_ts,
            ..
        } => {
            if commit_ts <= lock_ts {
                return Err(Error::InvalidTxnTso {
                    start_ts: lock_ts,
                    commit_ts: commit_ts,
                });
            }
            let mut txn = MvccTxn::new(
                snapshot,
                statistics,
                lock_ts,
                None,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            for k in keys {
                try!(txn.commit(k, commit_ts));
            }

            let pr = ProcessResult::Res;
            (pr, txn.modifies())
        }
        Command::Cleanup {
            ref ctx,
            ref key,
            start_ts,
            ..
        } => {
            let mut txn = MvccTxn::new(
                snapshot,
                statistics,
                start_ts,
                None,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            try!(txn.rollback(key));

            let pr = ProcessResult::Res;
            (pr, txn.modifies())
        }
        Command::Rollback {
            ref ctx,
            ref keys,
            start_ts,
            ..
        } => {
            let mut txn = MvccTxn::new(
                snapshot,
                statistics,
                start_ts,
                None,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            for k in keys {
                try!(txn.rollback(k));
            }

            let pr = ProcessResult::Res;
            (pr, txn.modifies())
        }
        Command::ResolveLock {
            ref ctx,
            start_ts,
            commit_ts,
            ref mut scan_key,
            ref keys,
        } => {
            if let Some(cts) = commit_ts {
                if cts <= start_ts {
                    return Err(Error::InvalidTxnTso {
                        start_ts: start_ts,
                        commit_ts: cts,
                    });
                }
            }
            let mut scan_key = scan_key.take();
            let mut txn = MvccTxn::new(
                snapshot,
                statistics,
                start_ts,
                None,
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            for k in keys {
                match commit_ts {
                    Some(ts) => try!(txn.commit(k, ts)),
                    None => try!(txn.rollback(k)),
                }
                if txn.write_size() >= MAX_TXN_WRITE_SIZE {
                    scan_key = Some(k.to_owned());
                    break;
                }
            }
            if scan_key.is_none() {
                (ProcessResult::Res, txn.modifies())
            } else {
                let pr = ProcessResult::NextCommand {
                    cmd: Command::ResolveLock {
                        ctx: ctx.clone(),
                        start_ts: start_ts,
                        commit_ts: commit_ts,
                        scan_key: scan_key.take(),
                        keys: vec![],
                    },
                };
                (pr, txn.modifies())
            }
        }
        Command::Gc {
            ref ctx,
            safe_point,
            ratio_threshold,
            ref mut scan_key,
            ref keys,
        } => {
            let mut scan_key = scan_key.take();
            let mut txn = MvccTxn::new(
                snapshot,
                statistics,
                0,
                Some(ScanMode::Forward),
                ctx.get_isolation_level(),
                !ctx.get_not_fill_cache(),
            );
            for k in keys {
                try!(txn.gc(k, safe_point));
                if txn.write_size() >= MAX_TXN_WRITE_SIZE {
                    scan_key = Some(k.to_owned());
                    break;
                }
            }
            if scan_key.is_none() {
                (ProcessResult::Res, txn.modifies())
            } else {
                let pr = ProcessResult::NextCommand {
                    cmd: Command::Gc {
                        ctx: ctx.clone(),
                        safe_point: safe_point,
                        ratio_threshold: ratio_threshold,
                        scan_key: scan_key.take(),
                        keys: vec![],
                    },
                };
                (pr, txn.modifies())
            }
        }
        _ => panic!("unsupported write command"),
    };

    box_try!(ch.send(Msg::WritePrepareFinished {
        cid: cid,
        cmd: cmd,
        pr: pr,
        to_be_write: modifies,
    }));

    Ok(())
}

#[derive(Default)]
struct ScheContext {
    stats: HashMap<&'static str, StatisticsSummary>,
}

impl ScheContext {
    fn add_statistics(&mut self, cmd_tag: &'static str, stat: &Statistics) {
        let entry = self.stats.entry(cmd_tag).or_insert_with(Default::default);
        entry.add_statistics(stat);
    }
}

impl ThreadContext for ScheContext {
    fn on_tick(&mut self) {
        for (cmd, stat) in self.stats.drain() {
            for (cf, details) in stat.stat.details() {
                for (tag, count) in details {
                    KV_COMMAND_SCAN_DETAILS
                        .with_label_values(&[cmd, cf, tag])
                        .observe(count as f64 / stat.count as f64);
                }
            }
        }
    }
}

impl Scheduler {
    /// Generates the next command ID.
    fn gen_id(&mut self) -> u64 {
        self.id_alloc += 1;
        self.id_alloc
    }

    fn insert_ctx(&mut self, ctx: RunningCtx) {
        if ctx.lock.is_write_lock() {
            self.running_write_count += 1;
        }
        if ctx.tag == CMD_TAG_GC {
            self.has_gc_command = true;
        }
        let cid = ctx.cid;
        if self.cmd_ctxs.insert(cid, ctx).is_some() {
            panic!("command cid={} shouldn't exist", cid);
        }
        SCHED_CONTEX_GAUGE.set(self.cmd_ctxs.len() as f64);
    }

    fn remove_ctx(&mut self, cid: u64) -> RunningCtx {
        let ctx = self.cmd_ctxs.remove(&cid).unwrap();
        assert_eq!(ctx.cid, cid);
        if ctx.lock.is_write_lock() {
            self.running_write_count -= 1;
        }
        if ctx.tag == CMD_TAG_GC {
            self.has_gc_command = false;
        }
        SCHED_CONTEX_GAUGE.set(self.cmd_ctxs.len() as f64);
        ctx
    }

    fn get_ctx_tag(&self, cid: u64) -> &'static str {
        let ctx = &self.cmd_ctxs[&cid];
        ctx.tag
    }

    fn fetch_worker_pool(&self, priority: CommandPri) -> &ThreadPool<ScheContext> {
        match priority {
            CommandPri::Low | CommandPri::Normal => &self.worker_pool,
            CommandPri::High => &self.high_priority_pool,
        }
    }

    /// Delivers a command to a worker thread for processing.
    fn process_by_worker(&mut self, cid: u64, cb_ctx: CbContext, snapshot: Box<Snapshot>) {
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[self.get_ctx_tag(cid), "process"])
            .inc();
        debug!(
            "process cmd with snapshot, cid={}, cb_ctx={:?}",
            cid,
            cb_ctx
        );
        let mut cmd = {
            let ctx = &mut self.cmd_ctxs.get_mut(&cid).unwrap();
            assert_eq!(ctx.cid, cid);
            ctx.cmd.take().unwrap()
        };
        if let Some(term) = cb_ctx.term {
            cmd.mut_context().set_term(term);
        }
        let ch = self.schedch.clone();
        let readcmd = cmd.readonly();
        let worker_pool = self.fetch_worker_pool(cmd.priority());
        let tag = cmd.tag();
        if readcmd {
            worker_pool.execute(move |ctx: &mut ScheContext| {
                let s = process_read(cid, cmd, ch, snapshot);
                ctx.add_statistics(tag, &s);
            });
        } else {
            worker_pool.execute(move |ctx: &mut ScheContext| {
                let s = process_write(cid, cmd, ch, snapshot);
                ctx.add_statistics(tag, &s);
            });
        }
    }

    /// Calls the callback with an error.
    fn finish_with_err(&mut self, cid: u64, err: Error) {
        debug!("command cid={}, finished with error", cid);
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[self.get_ctx_tag(cid), "error"])
            .inc();

        let mut ctx = self.remove_ctx(cid);
        let cb = ctx.callback.take().unwrap();
        let pr = ProcessResult::Failed {
            err: StorageError::from(err),
        };
        execute_callback(cb, pr);

        self.release_lock(&ctx.lock, cid);
    }

    /// Extracts the context of a command.
    fn extract_context(&self, cid: u64) -> &Context {
        let ctx = &self.cmd_ctxs[&cid];
        assert_eq!(ctx.cid, cid);
        ctx.cmd.as_ref().unwrap().get_context()
    }

    /// Event handler for new command.
    ///
    /// This method will try to acquire all the necessary latches. If all the necessary latches are
    /// acquired,  the method initiates a get snapshot operation for furthur processing; otherwise,
    /// the method adds the command to the waiting queue(s).   The command will be handled later in
    /// `lock_and_register_get_snapshot` when its turn comes.
    ///
    /// Note that once a command is ready to execute, the snapshot is always up-to-date during the
    /// execution because 1) all the conflicting commands (if any) must be in the waiting queues;
    /// 2) there may be non-conflicitng commands running concurrently, but it doesn't matter.
    fn schedule_command(&mut self, cmd: Command, callback: StorageCb) {
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[cmd.tag(), "new"])
            .inc();
        SCHED_COMMANDS_PRI_COUNTER_VEC
            .with_label_values(&[cmd.priority_tag()])
            .inc();
        let cid = self.gen_id();
        debug!("received new command, cid={}, cmd={}", cid, cmd);
        let lock = gen_command_lock(&self.latches, &cmd);
        let ctx = RunningCtx::new(cid, cmd, lock, callback);
        self.insert_ctx(ctx);
        self.lock_and_register_get_snapshot(cid);
    }

    fn too_busy(&self) -> bool {
        self.running_write_count >= self.sched_too_busy_threshold
    }

    fn on_receive_new_cmd(&mut self, cmd: Command, callback: StorageCb) {
        // write flow control
        if cmd.need_flow_control() && self.too_busy() {
            SCHED_TOO_BUSY_COUNTER_VEC
                .with_label_values(&[cmd.tag()])
                .inc();
            execute_callback(
                callback,
                ProcessResult::Failed {
                    err: StorageError::SchedTooBusy,
                },
            );
            return;
        }
        // Allow 1 GC command at the same time.
        if cmd.tag() == CMD_TAG_GC && self.has_gc_command {
            SCHED_TOO_BUSY_COUNTER_VEC
                .with_label_values(&[cmd.tag()])
                .inc();
            execute_callback(
                callback,
                ProcessResult::Failed {
                    err: StorageError::SchedTooBusy,
                },
            );
            return;

        }
        self.schedule_command(cmd, callback);
    }

    /// Tries to acquire all the required latches for a command.
    ///
    /// Returns true if successful; returns false otherwise.
    fn acquire_lock(&mut self, cid: u64) -> bool {
        let ctx = &mut self.cmd_ctxs.get_mut(&cid).unwrap();
        assert_eq!(ctx.cid, cid);
        let ok = self.latches.acquire(&mut ctx.lock, cid);
        if ok {
            ctx.latch_timer.take();
        }
        ok
    }

    /// Initiates an async operation to get a snapshot from the storage engine, then posts a
    /// `SnapshotFinished` message back to the event loop when it finishes.
    fn get_snapshot(&mut self, ctx: &Context, cids: Vec<u64>) {
        for cid in &cids {
            SCHED_STAGE_COUNTER_VEC
                .with_label_values(&[self.get_ctx_tag(*cid), "snapshot"])
                .inc();
        }
        let cids1 = cids.clone();
        let ch = self.schedch.clone();
        let cb = box move |(cb_ctx, snapshot)| match ch.send(Msg::SnapshotFinished {
            cids: cids1,
            cb_ctx: cb_ctx,
            snapshot: snapshot,
        }) {
            Ok(_) => {}
            e @ Err(TransportError::Closed) => info!("channel closed, err {:?}", e),
            Err(e) => panic!("send SnapshotFinish failed, err {:?}", e),
        };

        if let Err(e) = self.engine.async_snapshot(ctx, cb) {
            for cid in cids {
                SCHED_STAGE_COUNTER_VEC
                    .with_label_values(&[self.get_ctx_tag(cid), "async_snap_err"])
                    .inc();

                let e = e.maybe_clone().unwrap_or_else(|| {
                    error!("async snapshot failed for cid={}, error {:?}", cid, e);
                    EngineError::Other(box_err!("{:?}", e))
                });
                self.finish_with_err(cid, Error::from(e));
            }
        }
    }

    /// Initiates an async operation to batch get snapshot from the storage engine, then posts a
    /// `BatchSnapshotFinished` message back to the event loop when it finishes, also it posts a
    /// `RetryGetSnapshots` message if there are any `None` responses.
    fn batch_get_snapshot(&mut self, batch: Vec<(Context, Vec<u64>)>) {
        let mut all_cids = Vec::with_capacity(batch.iter().map(|&(_, ref cids)| cids.len()).sum());
        for &(_, ref cids) in &batch {
            all_cids.extend(cids);
        }

        let batch1 = batch.iter().map(|&(ref ctx, _)| ctx.clone()).collect();
        let ch = self.schedch.clone();
        let on_finished: engine::BatchCallback<Box<Snapshot>> = box move |results: Vec<_>| {
            let mut ready = Vec::with_capacity(results.len());
            let mut retry = Vec::new();
            for ((ctx, cids), snapshot) in batch.into_iter().zip(results) {
                match snapshot {
                    Some((cb_ctx, snapshot)) => {
                        ready.push((cids, cb_ctx, snapshot));
                    }
                    None => {
                        retry.push((ctx, cids));
                    }
                }
            }
            if !ready.is_empty() {
                match ch.send(Msg::BatchSnapshotFinished { batch: ready }) {
                    Ok(_) => {}
                    e @ Err(TransportError::Closed) => info!("channel closed, err {:?}", e),
                    Err(e) => {
                        panic!("send BatchSnapshotFinish failed err {:?}", e);
                    }
                }
            }
            if !retry.is_empty() {
                BATCH_COMMANDS
                    .with_label_values(&["retry"])
                    .observe(retry.len() as f64);
                match ch.send(Msg::RetryGetSnapshots(retry)) {
                    Ok(_) => {}
                    e @ Err(TransportError::Closed) => info!("channel closed, err {:?}", e),
                    Err(e) => {
                        panic!("send RetryGetSnapshots failed err {:?}", e);
                    }
                }
            }
        };

        if let Err(e) = self.engine.async_batch_snapshot(batch1, on_finished) {
            for cid in all_cids {
                SCHED_STAGE_COUNTER_VEC
                    .with_label_values(&[self.get_ctx_tag(cid), "async_snap_err"])
                    .inc();
                let e = e.maybe_clone().unwrap_or_else(|| {
                    error!("async snapshot failed for cid={}, error {:?}", cid, e);
                    EngineError::Other(box_err!("{:?}", e))
                });
                self.finish_with_err(cid, Error::from(e));
            }
        }
    }

    /// Event handler for the completion of get snapshot.
    ///
    /// Delivers the command along with the snapshot to a worker thread to execute.
    fn on_snapshot_finished(
        &mut self,
        cids: Vec<u64>,
        cb_ctx: CbContext,
        snapshot: EngineResult<Box<Snapshot>>,
    ) {
        debug!(
            "receive snapshot finish msg for cids={:?}, cb_ctx={:?}",
            cids,
            cb_ctx
        );
        match snapshot {
            Ok(ref snapshot) => for cid in cids {
                SCHED_STAGE_COUNTER_VEC
                    .with_label_values(&[self.get_ctx_tag(cid), "snapshot_ok"])
                    .inc();
                let s = Snapshot::clone(snapshot.as_ref());
                self.process_by_worker(cid, cb_ctx.clone(), s);
            },
            Err(ref e) => {
                error!("get snapshot failed for cids={:?}, error {:?}", cids, e);
                for cid in cids {
                    SCHED_STAGE_COUNTER_VEC
                        .with_label_values(&[self.get_ctx_tag(cid), "snapshot_err"])
                        .inc();
                    let e = e.maybe_clone()
                        .unwrap_or_else(|| EngineError::Other(box_err!("{:?}", e)));
                    self.finish_with_err(cid, Error::from(e));
                }
            }
        }
    }

    /// Event handler for the success of read.
    ///
    /// If a next command is present, continues to execute; otherwise, delivers the result to the
    /// callback.
    fn on_read_finished(&mut self, cid: u64, pr: ProcessResult) {
        debug!("read command(cid={}) finished", cid);
        let mut ctx = self.remove_ctx(cid);
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[ctx.tag, "read_finish"])
            .inc();
        let cb = ctx.callback.take().unwrap();
        if let ProcessResult::NextCommand { cmd } = pr {
            SCHED_STAGE_COUNTER_VEC
                .with_label_values(&[ctx.tag, "next_cmd"])
                .inc();
            self.schedule_command(cmd, cb);
        } else {
            execute_callback(cb, pr);
        }

        self.release_lock(&ctx.lock, cid);
    }

    /// Event handler for the failure of write prepare.
    ///
    /// Write prepare failure typically means conflicting transactions are detected. Delivers the
    /// error to the callback, and releases the latches.
    fn on_write_prepare_failed(&mut self, cid: u64, e: Error) {
        debug!("write command(cid={}) failed at prewrite.", cid);
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[self.get_ctx_tag(cid), "prepare_write_err"])
            .inc();
        self.finish_with_err(cid, e);
    }

    /// Event handler for the success of write prepare.
    ///
    /// Initiates an async write operation on the storage engine, there'll be a `WriteFinished`
    /// message when it finishes.
    fn on_write_prepare_finished(
        &mut self,
        cid: u64,
        cmd: Command,
        pr: ProcessResult,
        to_be_write: Vec<Modify>,
    ) {
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[self.get_ctx_tag(cid), "write"])
            .inc();
        if to_be_write.is_empty() {
            return self.on_write_finished(cid, pr, Ok(()));
        }
        let engine_cb = make_engine_cb(cid, pr, self.schedch.clone());
        if let Err(e) = self.engine
            .async_write(cmd.get_context(), to_be_write, engine_cb)
        {
            SCHED_STAGE_COUNTER_VEC
                .with_label_values(&[self.get_ctx_tag(cid), "async_write_err"])
                .inc();
            self.finish_with_err(cid, Error::from(e));
        }
    }

    /// Event handler for the success of write.
    fn on_write_finished(&mut self, cid: u64, pr: ProcessResult, result: EngineResult<()>) {
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[self.get_ctx_tag(cid), "write_finish"])
            .inc();
        debug!("write finished for command, cid={}", cid);
        let mut ctx = self.remove_ctx(cid);
        let cb = ctx.callback.take().unwrap();
        let pr = match result {
            Ok(()) => pr,
            Err(e) => ProcessResult::Failed {
                err: ::storage::Error::from(e),
            },
        };
        if let ProcessResult::NextCommand { cmd } = pr {
            SCHED_STAGE_COUNTER_VEC
                .with_label_values(&[ctx.tag, "next_cmd"])
                .inc();
            self.schedule_command(cmd, cb);
        } else {
            execute_callback(cb, pr);
        }

        self.release_lock(&ctx.lock, cid);
    }

    /// Releases all the latches held by a command.
    fn release_lock(&mut self, lock: &Lock, cid: u64) {
        let wakeup_list = self.latches.release(lock, cid);
        for wcid in wakeup_list {
            self.lock_and_register_get_snapshot(wcid);
        }
    }

    /// Tries to acquire all the necessary latches. If all the necessary latches are acquired,
    /// the method initiates a get snapshot operation for furthur processing.
    fn lock_and_register_get_snapshot(&mut self, cid: u64) {
        if self.acquire_lock(cid) {
            let ctx = self.extract_context(cid).clone();
            let group = self.grouped_cmds
                .as_mut()
                .unwrap()
                .entry(HashableContext(ctx))
                .or_insert_with(Vec::new);
            group.push(cid);
        }
    }

    pub fn run(&mut self, receiver: Receiver<Msg>) -> Result<()> {
        let mut msgs = Vec::with_capacity(CMD_BATCH_SIZE);
        loop {
            let msg = box_try!(receiver.recv());
            msgs.push(msg);
            while let Ok(msg) = receiver.try_recv() {
                msgs.push(msg);
                if msgs.len() >= CMD_BATCH_SIZE {
                    break;
                }
            }

            for msg in msgs.drain(..) {
                match msg {
                    Msg::Quit => return self.shutdown(),
                    Msg::RawCmd { cmd, cb } => self.on_receive_new_cmd(cmd, cb),
                    Msg::RetryGetSnapshots(tasks) => for (ctx, cids) in tasks {
                        self.get_snapshot(&ctx, cids);
                    },
                    Msg::SnapshotFinished {
                        cids,
                        cb_ctx,
                        snapshot,
                    } => self.on_snapshot_finished(cids, cb_ctx, snapshot),
                    Msg::BatchSnapshotFinished { batch } => for (cids, cb_ctx, snapshot) in batch {
                        self.on_snapshot_finished(cids, cb_ctx, snapshot)
                    },
                    Msg::ReadFinished { cid, pr } => self.on_read_finished(cid, pr),
                    Msg::WritePrepareFinished {
                        cid,
                        cmd,
                        pr,
                        to_be_write,
                    } => self.on_write_prepare_finished(cid, cmd, pr, to_be_write),
                    Msg::WritePrepareFailed { cid, err } => self.on_write_prepare_failed(cid, err),
                    Msg::WriteFinished {
                        cid, pr, result, ..
                    } => self.on_write_finished(cid, pr, result),
                }
            }

            if self.grouped_cmds.as_ref().unwrap().is_empty() {
                continue;
            }

            if let Some(cmds) = self.grouped_cmds.take() {
                self.grouped_cmds = Some(HashMap::with_capacity_and_hasher(
                    CMD_BATCH_SIZE,
                    Default::default(),
                ));
                let batch = cmds.into_iter().map(|(hash_ctx, cids)| {
                    BATCH_COMMANDS
                        .with_label_values(&["all"])
                        .observe(cids.len() as f64);
                    (hash_ctx.0, cids)
                });
                self.batch_get_snapshot(batch.collect());
            }
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        if let Err(e) = self.worker_pool.stop() {
            return Err(Error::Other(box_err!("{:?}", e)));
        }
        if let Err(e) = self.high_priority_pool.stop() {
            return Err(Error::Other(box_err!("{:?}", e)));
        }
        Ok(())
    }
}

const CMD_BATCH_SIZE: usize = 256;

/// Generates the lock for a command.
///
/// Basically, read-only commands require no latches, write commands require latches hashed
/// by the referenced keys.
pub fn gen_command_lock(latches: &Latches, cmd: &Command) -> Lock {
    match *cmd {
        Command::Prewrite { ref mutations, .. } => {
            let keys: Vec<&Key> = mutations.iter().map(|x| x.key()).collect();
            latches.gen_lock(&keys)
        }
        Command::Commit { ref keys, .. } |
        Command::Rollback { ref keys, .. } |
        Command::ResolveLock { ref keys, .. } => latches.gen_lock(keys),
        Command::Cleanup { ref key, .. } => latches.gen_lock(&[key]),
        _ => Lock::new(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvproto::kvrpcpb::Context;
    use storage::txn::latch::*;
    use storage::{make_key, Command, Mutation, Options};

    #[test]
    fn test_command_latches() {
        let readonly_cmds = vec![
            Command::Get {
                ctx: Context::new(),
                key: make_key(b"k"),
                start_ts: 25,
            },
            Command::BatchGet {
                ctx: Context::new(),
                keys: vec![make_key(b"k")],
                start_ts: 25,
            },
            Command::Scan {
                ctx: Context::new(),
                start_key: make_key(b"k"),
                limit: 100,
                start_ts: 25,
                options: Options::default(),
            },
            Command::ScanLock {
                ctx: Context::new(),
                max_ts: 5,
            },
            Command::ResolveLock {
                ctx: Context::new(),
                start_ts: 10,
                commit_ts: Some(20),
                scan_key: None,
                keys: vec![],
            },
            Command::Gc {
                ctx: Context::new(),
                safe_point: 5,
                ratio_threshold: 0.0,
                scan_key: None,
                keys: vec![make_key(b"k")],
            },
            Command::MvccByKey {
                ctx: Context::new(),
                key: make_key(b"k"),
            },
            Command::MvccByStartTs {
                ctx: Context::new(),
                start_ts: 25,
            },
        ];
        let write_cmds = vec![
            Command::Prewrite {
                ctx: Context::new(),
                mutations: vec![Mutation::Put((make_key(b"k"), b"v".to_vec()))],
                primary: b"k".to_vec(),
                start_ts: 10,
                options: Options::default(),
            },
            Command::Commit {
                ctx: Context::new(),
                keys: vec![make_key(b"k")],
                lock_ts: 10,
                commit_ts: 20,
            },
            Command::Cleanup {
                ctx: Context::new(),
                key: make_key(b"k"),
                start_ts: 10,
            },
            Command::Rollback {
                ctx: Context::new(),
                keys: vec![make_key(b"k")],
                start_ts: 10,
            },
            Command::ResolveLock {
                ctx: Context::new(),
                start_ts: 10,
                commit_ts: Some(20),
                scan_key: None,
                keys: vec![make_key(b"k")],
            },
        ];

        let mut latches = Latches::new(1024);

        let write_locks: Vec<Lock> = write_cmds
            .into_iter()
            .enumerate()
            .map(|(id, cmd)| {
                let mut lock = gen_command_lock(&latches, &cmd);
                assert_eq!(latches.acquire(&mut lock, id as u64), id == 0);
                lock
            })
            .collect();

        for (id, cmd) in readonly_cmds.iter().enumerate() {
            let mut lock = gen_command_lock(&latches, cmd);
            assert!(latches.acquire(&mut lock, id as u64));
        }

        // acquire/release locks one by one.
        let max_id = write_locks.len() as u64 - 1;
        for (id, mut lock) in write_locks.into_iter().enumerate() {
            let id = id as u64;
            if id != 0 {
                assert!(latches.acquire(&mut lock, id));
            }
            let unlocked = latches.release(&lock, id);
            if id as u64 == max_id {
                assert!(unlocked.is_empty());
            } else {
                assert_eq!(unlocked, vec![id + 1]);
            }
        }
    }
}
