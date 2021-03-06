
//TODO use faster HashMap, HashSet
use std::{self, iter, mem};
use std::collections::VecDeque;
use std::collections::hash_map;
use std::io;
use std::sync::mpsc;
use std::rc::Rc;
use std::u64;

use packets::*;
use store::AsyncStoreClient;
use self::FromStore::*;
use self::FromClient::*;

use hash::{HashMap, HashSet, UuidHashMap};

use self::per_color::{PerColor, IsRead, ReadHandle, NextToFetch};

use store;

pub mod log_handle;
mod per_color;
mod range_tree;

// const MAX_PREFETCH: u32 = 40;
const MAX_PREFETCH: u32 = 40;

type ChainEntry = Rc<Vec<u8>>;

pub struct ThreadLog<FinshedReadQueue, FinshedWriteQueue> {
    to_store: store::ToSelf, //TODO send WriteState or other enum?
    from_outside: mpsc::Receiver<Message>, //TODO should this be per-chain?
    blockers: HashMap<OrderIndex, Vec<ChainEntry>>,
    blocked_multiappends: UuidHashMap<MultiSearchState>,
    per_chains: HashMap<order, PerColor>,
    //TODO replace with queue from deque to allow multiple consumers?
    ready_reads: FinshedReadQueue,
    //TODO blocked_chains: BitSet ?
    //TODO how to multiplex writers finished_writes: Vec<mpsc::Sender<()>>,
    finished_writes: FinshedWriteQueue,
    //FIXME is currently unused
    #[allow(dead_code)]
    to_return: VecDeque<Vec<u8>>,
    //TODO
    no_longer_blocked: Vec<OrderIndex>,
    cache: BufferCache,
    chains_currently_being_read: IsRead,
    num_snapshots: usize,

    num_errors: u64,

    fetch_boring_multis: bool,
    ack_writes: bool,
    return_snapshots: bool,
    no_remote_style: NoRemoteStyle,

    finished: bool,

    print_data: PrintData,
    prefetch : u32,

    last_seen_entries: HashMap<order, entry>,
    my_colors_chains: HashSet<order>,
}

pub struct ThreadLogBuilder<FinshedReadQueue, FinshedWriteQueue=()> {
    to_store: store::ToSelf,
    from_outside: mpsc::Receiver<Message>,
    ready_reads: FinshedReadQueue,

    ack_writes: bool,
    finished_writes: FinshedWriteQueue,
    return_snapshots: bool,
    per_chains: HashMap<order, PerColor>,
    fetch_boring_multis: bool,
    no_remote_style: NoRemoteStyle,

    my_colors_chains: Option<HashSet<order>>,
}

#[derive(Debug, PartialEq, Eq)]
enum NoRemoteStyle {
    NoConnection,
    EnsureRead,
    Atomic,
}

impl<FinshedReadQueue> ThreadLogBuilder<FinshedReadQueue, ()> {
    pub fn new(
        to_store: store::ToSelf,
        from_outside: mpsc::Receiver<Message>,
        ready_reads: FinshedReadQueue,
    ) -> Self {
        ThreadLogBuilder {
            to_store,
            from_outside,
            ready_reads,

            ack_writes: false,
            finished_writes: (),
            return_snapshots: false,
            per_chains: HashMap::default(),
            fetch_boring_multis: false,

            //TODO what's the default?
            no_remote_style: NoRemoteStyle::NoConnection,

            my_colors_chains: None,
        }
    }
}

impl<FinshedReadQueue, FinshedWriteQueue> ThreadLogBuilder<FinshedReadQueue, FinshedWriteQueue> {
    pub fn chains<I>(self, chains: I) -> Self
    where I: IntoIterator<Item=order> {
        let per_chains = chains.into_iter().map(|c| (c, PerColor::interesting(c))).collect();
        ThreadLogBuilder{ per_chains: per_chains, .. self}
    }

    pub fn ack_writes<FWQ>(self, to: FWQ) -> ThreadLogBuilder<FinshedReadQueue, FWQ> {
        let ThreadLogBuilder{
            to_store,
            from_outside,
            ready_reads,
            ack_writes: _,
            finished_writes: _,
            return_snapshots,
            per_chains,
            fetch_boring_multis,
            no_remote_style,
            my_colors_chains,
        } = self;
        ThreadLogBuilder{
            to_store,
            from_outside,
            ready_reads,
            return_snapshots,
            per_chains,
            fetch_boring_multis,
            no_remote_style,
            ack_writes: true,
            finished_writes: to,
            my_colors_chains,
        }
    }

    fn no_ack_writes(&mut self) {
        self.ack_writes = false;
    }

    pub fn return_snapshots(self) -> Self {
        ThreadLogBuilder{ return_snapshots: true, .. self}
    }

    pub fn fetch_boring_multis(self) -> Self {
        ThreadLogBuilder{ fetch_boring_multis: true, .. self}
    }

    pub fn set_fetch_boring_multis(self, fetch_boring_multis: bool) -> Self {
        ThreadLogBuilder{ fetch_boring_multis, .. self}
    }

    #[allow(non_snake_case)]
    pub fn ensure__no_remote__read(self) -> Self {
        ThreadLogBuilder{ no_remote_style: NoRemoteStyle::EnsureRead, .. self}
    }

    #[allow(non_snake_case)]
    pub fn atomic__no_remotes(self) -> Self {
        ThreadLogBuilder{ no_remote_style: NoRemoteStyle::Atomic, .. self}
    }

    pub fn my_colors_chains(self, chains: impl IntoIterator<Item=order>) -> Self {
        let chains: HashSet<_> = chains.into_iter().collect();
        let builder = self.chains(chains.iter().cloned());
        ThreadLogBuilder{ my_colors_chains: Some(chains), .. builder }
    }

    pub fn build(self) -> ThreadLog<FinshedReadQueue, FinshedWriteQueue>
    where
        FinshedReadQueue: OnRead,
        FinshedWriteQueue: OnWrote, {
        let ThreadLogBuilder {
            to_store,
            from_outside,
            ready_reads,
            ack_writes,
            finished_writes,
            return_snapshots,
            per_chains,
            fetch_boring_multis,
            no_remote_style,
            my_colors_chains,
        } = self;
        ThreadLog {
            to_store,
            from_outside,
            blockers: HashMap::default(),
            blocked_multiappends: Default::default(),
            ready_reads,
            finished_writes,
            per_chains,
            to_return: Default::default(),
            no_longer_blocked: Default::default(),
            cache: BufferCache::new(),
            chains_currently_being_read: Rc::new(ReadHandle),
            num_snapshots: 0,
            num_errors: 0,
            print_data: Default::default(),
            finished: false,
            fetch_boring_multis,
            ack_writes,
            return_snapshots,
            no_remote_style,
            prefetch: 1,
            last_seen_entries: Default::default(),
            my_colors_chains: my_colors_chains.unwrap_or_default(),
        }
    }
}

pub type FinshedReadQueue = mpsc::Sender<Result<Vec<u8>, Error>>;
pub type FinshedReadRecv = mpsc::Receiver<Result<Vec<u8>, Error>>;

pub type FinshedWriteQueue = mpsc::Sender<Result<(Uuid, Vec<OrderIndex>), Error>>;
pub type FinshedWriteRecv = mpsc::Receiver<Result<(Uuid, Vec<OrderIndex>), Error>>;

#[derive(Debug, Clone)]
pub struct Error {
    error_num: u64,
    server: usize,
    error: io::ErrorKind,
}

counters!{
    struct PrintData {
        snap: u64,
        append: u64,
        write_done: u64,
        read_done: u64,
        ret: u64,
        shut: u64,
    }
}

struct MultiSearchState {
    val: Vec<u8>,
    //pieces_remaining: usize,
}

pub enum Message {
    FromStore(FromStore),
    FromClient(FromClient),
}

//TODO hide in struct
pub enum FromStore {
    WriteComplete(Uuid, Vec<OrderIndex>), //TODO
    ReadComplete(OrderIndex, Vec<u8>),
    IoError(io::ErrorKind, usize),
}

pub enum FromClient {
    //TODO
    SnapshotAndPrefetch(order),
    MultiSnapshotAndPrefetch(Vec<order>),
    StrongSnapshotAndPrefetch(Vec<OrderIndex>),
    PerformAppend(Vec<u8>),
    ReturnBuffer(Vec<u8>),
    ReadUntil(OrderIndex),
    Fastforward(OrderIndex),
    Rewind(OrderIndex),
    StopAckingWrites,
    Shutdown,
}

enum MultiSearch {
    Finished(Vec<u8>),
    InProgress,
    EarlySentinel,
    BeyondHorizon(Vec<u8>),
    #[allow(dead_code)]
    Repeat,
    WaitForDeps(Vec<u8>),
    //MultiSearch::FirstPart(),
}

impl<FinshedReadQueue> ThreadLog<FinshedReadQueue, ()> {
    pub fn builder(
        to_store: store::ToSelf,
        from_outside: mpsc::Receiver<Message>,
        ready_reads: FinshedReadQueue,
    ) -> ThreadLogBuilder<FinshedReadQueue, ()> {
        ThreadLogBuilder::new(to_store, from_outside, ready_reads)
    }
}

impl<FinshedReadQueue, FinshedWriteQueue> ThreadLog<FinshedReadQueue, FinshedWriteQueue>
where
    FinshedReadQueue: OnRead,
    FinshedWriteQueue: OnWrote, {

    //TODO
    pub fn new<I>(
        to_store: store::ToSelf,
        from_outside: mpsc::Receiver<Message>,
        ready_reads: FinshedReadQueue,
        finished_writes: FinshedWriteQueue,
        fetch_boring_multis: bool,
        ack_writes: bool,
        interesting_chains: I
    ) -> Self
    where I: IntoIterator<Item=order>{
        let builder = ThreadLog::builder(to_store, from_outside, ready_reads)
            .chains(interesting_chains)
            .ack_writes(finished_writes);
        let mut builder = if fetch_boring_multis {
            builder.fetch_boring_multis()
        } else {
            builder
        };
        if !ack_writes {
            builder.no_ack_writes()
        }
        builder.build()
    }

    pub fn run(mut self) {
        // use std::thread;
        use std::time::Duration;
        //FIXME remove
        //let mut num_msgs = 0;
        'recv: while !self.finished {
            //let msg = self.from_outside.recv().expect("outside is gone");
            if let Ok(msg) = self.from_outside.recv_timeout(Duration::from_secs(3)) {
            //if let Ok(msg) = self.from_outside.recv() {
                if !self.handle_message(msg) { break 'recv }
                // num_msgs += 1;
            }
            else {
                #[cfg(feature = "print_stats")]
                {
                    println!("no log activity for 10s, {:?}",
                        self.print_data);
                }
            }
        }
    }

    fn handle_message(&mut self, msg: Message) -> bool {
        match msg {
            Message::FromClient(msg) => self.handle_from_client(msg),
            Message::FromStore(msg) => self.handle_from_store(msg),
        }
    }

    fn handle_from_client(&mut self, msg: FromClient) -> bool {
        match msg {
            SnapshotAndPrefetch(chain) => {
                self.print_data.snap(1);
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                trace!("FUZZY snapshot {:?}: {:?}", chain, self.num_snapshots);
                //FIXME
                if chain != 0u64.into() {
                    self.fetch_snapshot(chain);
                    self.prefetch(chain);
                }
                else {
                    let chains: Vec<_> = self.per_chains.iter()
                        .filter(|pc| pc.1.is_interesting)
                        .map(|pc| pc.0.clone()).collect();
                    for chain in chains {
                        self.fetch_snapshot(chain);
                        self.prefetch(chain);
                    }
                }
                true
            }
            MultiSnapshotAndPrefetch(chains) => {
                self.print_data.snap(1);
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                trace!("FUZZY snapshot {:?}: {:?}", chains, self.num_snapshots);
                for chain in chains {
                    self.fetch_snapshot(chain);
                    self.prefetch(chain);
                }
                true
            },
            StrongSnapshotAndPrefetch(chains) => {
                self.print_data.snap(1);
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                trace!("FUZZY strong snapshot {:?}: {:?}", chains, self.num_snapshots);
                self.fetch_strong_snapshot(&chains[..]);
                for chain in chains {
                    self.prefetch(chain.0);
                }
                true
            },
            PerformAppend(mut msg) => {
                self.print_data.append(1);

                if !self.my_colors_chains.is_empty() {
                    msg = {

                        let contents = bytes_as_entry(&msg);
                        let locs = contents.locs();

                        let mut happens_after_entries: Vec<_> = self.last_seen_entries
                            .drain()
                            //We don't want a dep if we're in the chain, it's redundant
                            .filter(|oi| locs.binary_search(&(oi.0, 0.into()).into()).is_err())
                            .map(OrderIndex::from)
                            .collect();

                        happens_after_entries.sort_unstable_by_key(|oi| u64::from(oi.0));

                        contents.with_deps(&happens_after_entries).to_vec()
                    };
                } else {
                    let layout = bytes_as_entry(&msg).layout();
                    assert!(layout == EntryLayout::Data || layout == EntryLayout::Multiput);
                }
                self.to_store.send(msg).expect("store hung up");
                true
            }
            ReturnBuffer(buffer) => {
                self.print_data.ret(1);
                self.cache.cache_buffer(buffer);
                true
            }
            ReadUntil(OrderIndex(chain, index)) => {
                self.num_snapshots = self.num_snapshots.saturating_add(1);
                let unblocked = {
                    let mut pc = self.per_chains.entry(chain)
                        .or_insert_with(|| PerColor::new(chain));
                    pc.increment_outstanding_snapshots(&self.chains_currently_being_read);
                    pc.give_new_snapshot(index)
                };
                if let Some(val) = unblocked {
                    let locs = self.return_entry(val);
                    if let Some(locs) = locs { self.stop_blocking_on(locs) }
                }
                self.continue_fetch(chain);
                true
            }
            Fastforward(loc) => {
                let pc = self.per_chains.entry(loc.0)
                    .or_insert_with(|| PerColor::new(loc.0));
                //FIXME drain irrelevant entries
                pc.give_new_snapshot(loc.1);
                true
            }
            Rewind(loc) => {
                let pc = self.per_chains.entry(loc.0)
                    .or_insert_with(|| PerColor::new(loc.0));
                //FIXME drain irrelevant entries
                pc.rewind_to(loc.1);
                true
            }
            StopAckingWrites => {
                self.ack_writes = false;
                true
            }
            Shutdown => {
                self.print_data.shut(1);
                self.finished = true;
                //TODO send shutdown
                false
            }
        }
    }

    fn handle_from_store(&mut self, msg: FromStore) -> bool {
        match msg {
            WriteComplete(id, locs) => {
                self.print_data.write_done(1);
                let check_color = !self.my_colors_chains.is_empty();
                for &OrderIndex(o, i) in locs.iter() {
                    if check_color && self.my_colors_chains.contains(&o) {
                        let e = self.last_seen_entries.entry(o).or_insert(0.into());
                        if *e < i { *e = i }
                    }
                }
                if self.ack_writes && self.finished_writes.send(Ok((id, locs))).is_err() {
                    self.finished = true;
                }
            },
            ReadComplete(loc, msg) => {
                self.print_data.read_done(1);
                self.handle_completed_read(loc, msg)
            },
            IoError(kind, server) => {
                let err = self.make_error(kind, server);
                let e1 = if self.ack_writes {
                    self.finished_writes.send(Err(err.clone()))
                } else {
                    Ok(())
                };
                let e2 = self.ready_reads.send(Err(err));
                if e1.is_err() || e2.is_err() {
                    self.finished = true;
                }
            }
        }
        true
    }

    fn make_error(&mut self, error: io::ErrorKind, server: usize) -> Error {
        let error_num = self.num_errors;
        self.num_errors += 1;
        Error {error_num, error, server,}
    }

    fn fetch_snapshot(&mut self, chain: order) {
        //XXX outstanding_snapshots is incremented in prefetch
        let packet = self.make_read_packet(chain, u64::MAX.into());
        self.to_store.send(packet).expect("store hung up")
    }

    fn fetch_strong_snapshot(&mut self, chains: &[OrderIndex]) {
        //XXX outstanding_snapshots is incremented in prefetch
        let packet = {
            let mut buffer = self.cache.alloc();
            EntryContents::Snapshot {
                id: &Uuid::new_v4(),
                flags: &EntryFlag::Nothing,
                data_bytes: &0,
                num_deps: &0,
                lock: &0,
                locs: chains,
            }.fill_vec(&mut buffer);
            buffer
        };
        self.to_store.send(packet).expect("store hung up")
    }

    fn prefetch(&mut self, chain: order) {
        //TODO allow new chains?
        //TODO how much to fetch
        let to_fetch = {
            let pc = &mut self.per_chains.get_mut(&chain).expect("boring server read");
            pc.increment_outstanding_snapshots(&self.chains_currently_being_read);
            let next = pc.next_range_to_fetch();
            let max_prefetch = self.prefetch;
            // let max_prefetch = (MAX_PREFETCH).saturating_sub(self.blockers.len() as u64) + 1;
            // let max_prefetch = MAX_PREFETCH;
            match next {
                NextToFetch::None => None,
                NextToFetch::AboveHorizon(low, high) => {
                    let num_to_fetch = (high - low) + 1;

                    let num_to_fetch = std::cmp::min(num_to_fetch, max_prefetch.into());
                    let currently_buffering = pc.currently_buffering();
                    if num_to_fetch == 0 {
                        None
                    } else if currently_buffering < num_to_fetch {
                        let num_to_fetch = num_to_fetch - currently_buffering;
                        let high = std::cmp::min(high, low + num_to_fetch - 1);
                        Some((low, high))
                    } else { None }
                },
                NextToFetch::BelowHorizon(low, high) => {
                    let num_to_fetch = (high - low) + 1;
                    let num_to_fetch = std::cmp::max(num_to_fetch, max_prefetch.into());
                    let currently_buffering = pc.currently_buffering();
                    if num_to_fetch == 0 {
                        None
                    } else if currently_buffering < num_to_fetch {
                        let num_to_fetch = num_to_fetch - currently_buffering;
                        let high = std::cmp::min(high, high + num_to_fetch - 1);
                        Some((low, high))
                    } else { None }
                },
            }
        };
        if let Some((low, high)) = to_fetch {
            self.fetch_next(chain, low, high)
        }
    }

    fn handle_completed_read(&mut self, read_loc: OrderIndex, msg: Vec<u8>) {
        //TODO right now this assumes order...
        let (kind, flag) = {
            let e = bytes_as_entry(&msg);
            (e.kind(), *e.flag())
        };
        trace!("FUZZY handle read @ {:?}", read_loc);

        match kind.layout() {
            EntryLayout::Snapshot => {
                debug_assert!(flag.contains(EntryFlag::ReadSuccess));
                {
                    let locs = bytes_as_entry(&msg).locs();
                    trace!("FUZZY got strong snapshot {:?}", locs);
                    // for loc in locs {
                    let loc = read_loc;
                    debug_assert!(bytes_as_entry(&msg).flag().contains(EntryFlag::ReadSuccess));
                    let unblocked = self.per_chains.get_mut(&loc.0).and_then(|s| {
                        trace!("FUZZY try update horizon to {:?}", loc);
                        s.give_new_snapshot(loc.1)
                    });
                    if let Some(val) = unblocked {
                        let locs = self.return_entry(val);
                        if let Some(locs) = locs { self.stop_blocking_on(locs) }
                    }
                }
                if self.return_snapshots {
                    self.ready_reads.send(Ok(msg)).expect("client gone");
                }
            }
            EntryLayout::Read => {
                trace!("FUZZY read has no data");
                debug_assert!(!flag.contains(EntryFlag::ReadSuccess));
                debug_assert!(bytes_as_entry(&msg).locs()[0] == read_loc);
                if read_loc.1 < u64::MAX.into() {
                    trace!("FUZZY overread at {:?}", read_loc);
                    //TODO would be nice to handle ooo reads better...
                    //     we can probably do it by checking (chain, read_loc - 1)
                    //     to see if the read we're about to attempt is there, but
                    //     it might be better to switch to a buffer per-chain model
                    self.per_chains.get_mut(&read_loc.0).map(|s| {
                        s.overread_at(read_loc.1);
                        //s.decrement_outstanding_reads();
                    });
                }
                else {
                    //due to non-atomic snapshot
                    let unblocked = {
                        let prefetch = &mut self.prefetch;

                        self.per_chains.get_mut(&read_loc.0).and_then(|s| {
                            let e = bytes_as_entry(&msg);
                            assert_eq!(e.locs()[0].1, u64::MAX.into());
                            debug_assert!(!e.flag().contains(EntryFlag::ReadSuccess));
                            let new_horizon = e.horizon().1;
                            let old_horizon = s.current_snap();
                            let needs_fetch = (u64::from(new_horizon)).saturating_sub(u64::from(old_horizon));
                            *prefetch = (needs_fetch / 3 + (2 * *prefetch as u64) / 3) as u32;
                            trace!("FUZZY try update horizon to {:?}", (read_loc.0, new_horizon));
                            s.give_new_snapshot(new_horizon)
                        })
                    };
                    if let Some(val) = unblocked {
                        let locs = self.return_entry(val);
                        if let Some(locs) = locs { self.stop_blocking_on(locs) }
                    }
                    if self.return_snapshots {
                        self.ready_reads.send(Ok(msg)).expect("client gone");
                    }
                }
            }
            EntryLayout::Data => {
                trace!("FUZZY read is single");
                debug_assert!(flag.contains(EntryFlag::ReadSuccess));
                //assert!(read_loc.1 >= pc.first_buffered);
                //TODO check that read is needed?
                //TODO no-alloc?
                let needed = self.per_chains.get_mut(&read_loc.0).map(|s|
                    s.got_read(read_loc.1)).unwrap_or(false);
                    //s.decrement_outstanding_reads());
                if needed {
                    let packet = Rc::new(msg);
                    let try_ret = self.add_blockers_at(read_loc, &packet);
                    if try_ret {
                        self.try_returning_at(read_loc, packet);
                    }
                }
            }
            EntryLayout::Multiput if self.no_remote_style != NoRemoteStyle::Atomic &&
                flag.contains(EntryFlag::NoRemote) => {

                trace!("FUZZY read is atom");
                // println!("FUZZY read is atom {:?} {:?}", read_loc, bytes_as_entry(&*msg).id());
                //FIXME handle all locs
                let needed = self.per_chains.get_mut(&read_loc.0).map(|s|
                    if read_loc.1 == entry::from(0u64) {
                        // panic!("{:?}", bytes_as_entry(&*msg))
                        false //TODO
                    } else {
                        s.got_read(read_loc.1)
                    }).unwrap_or(false);
                    //s.decrement_outstanding_reads());
                if needed {
                    //FIXME ensure other pieces get fetched if reading those chains
                    let packet = Rc::new(msg);
                    //TODO checks to ensure multiple parts of the append are returned atomically
                    if let NoRemoteStyle::EnsureRead = self.no_remote_style {
                        self.ensure_read(read_loc, &packet)
                    };
                    self.add_blockers_at(read_loc, &packet);
                    self.try_returning_at(read_loc, packet);
                }
            }

            layout @ EntryLayout::Multiput | layout @ EntryLayout::Sentinel => {
                trace!("FUZZY read is multi");
                debug_assert!(flag.contains(EntryFlag::ReadSuccess));
                let needed = self.per_chains.get_mut(&read_loc.0).map(|s|
                    s.got_read(read_loc.1)).unwrap_or(false); //FIXME only call got_read on success otherwise call overread
                    //s.decrement_outstanding_reads());
                if needed {
                    let is_sentinel = layout == EntryLayout::Sentinel;
                    let search_status =
                        self.update_multi_part_read(read_loc, msg, is_sentinel);
                    match search_status {
                        MultiSearch::InProgress | MultiSearch::EarlySentinel => {}
                        MultiSearch::BeyondHorizon(..) => {
                            //TODO better ooo reads
                            self.per_chains.entry(read_loc.0)
                                .or_insert_with(|| PerColor::new(read_loc.0))
                                .overread_at(read_loc.1);
                        }
                        MultiSearch::WaitForDeps(msg) => {
                            //TODO no-alloc?
                            let packet = Rc::new(msg);
                            //TODO it would be nice to fetch the blockers in parallel...
                            //     we can add a fetch blockers call in update_multi_part_read
                            //     which updates the horizon but doesn't actually add the block
                            let mut try_ret = self.wait_for_deps(&packet);
                            try_ret &= self.add_blockers(&packet);
                            if try_ret {
                                self.try_returning(packet);
                            }
                        },
                        MultiSearch::Finished(msg) => {
                            //TODO no-alloc?
                            let packet = Rc::new(msg);
                            //TODO it would be nice to fetch the blockers in parallel...
                            //     we can add a fetch blockers call in update_multi_part_read
                            //     which updates the horizon but doesn't actually add the block
                            let try_ret = self.add_blockers(&packet);
                            if try_ret {
                                self.try_returning(packet);
                            }
                        }
                        MultiSearch::Repeat => {}
                    }
                }
            }

            EntryLayout::Lock | EntryLayout::GC => unreachable!(),
        }

        self.continue_fetch(read_loc.0)
    }

    fn continue_fetch(&mut self, chain: order) {
        let finished_server = self.continue_fetch_if_needed(chain);
        if finished_server {
            trace!("FUZZY finished reading {:?}", chain);

            self.per_chains.get_mut(&chain).map(|pc| {
                debug_assert!(pc.is_finished());
                trace!("FUZZY chain {:?} is finished", pc.chain);
                pc.set_finished_reading();
            });
            if self.finshed_reading() {
                trace!("FUZZY finished reading all chains after {:?}", chain);
                //TODO do we need a better system?
                let num_completeds = mem::replace(&mut self.num_snapshots, 0);
                //assert!(num_completeds > 0);
                //FIXME add is_snapshoting to PerColor so this doesn't race?
                trace!("FUZZY finished reading {:?} snaps", num_completeds);
                for _ in 0..num_completeds {
                    if self.ready_reads.send(Ok(vec![])).is_err() {
                        self.finished = true;
                    }
                }
                self.cache.flush();
            } else {
                trace!("FUZZY chains other than {:?} not finished", chain);
            }
        }
        else {
            #[cfg(debug_assertions)]
            self.per_chains.get(&chain).map(|pc| {
                pc.trace_unfinished()
            });

        }
    }

    /// Blocks a packet on entries a it depends on. Will increment the refcount for each
    /// blockage.
    fn add_blockers(&mut self, packet: &ChainEntry) -> bool {
        //FIXME dependencies currently assumes you gave it the correct type
        //      this is unnecessary and should be changed
        let entr = bytes_as_entry(packet);
        let deps = entr.dependencies();
        let locs = entr.locs();
        let mut needed = false;
        let mut try_ret = false;
        for &loc in locs {
            if loc.0 == order::from(0u64) { continue }
            let (is_next_in_chain, needs_to_be_returned);
            {
                let (is_next, ntbr) = self.per_chains.get(&loc.0).map(|pc| {
                    (pc.next_return_is(loc.1), !pc.has_returned(loc.1))
                }).unwrap_or((true, false));
                is_next_in_chain = is_next;
                needs_to_be_returned = ntbr;
            }
            needed |= needs_to_be_returned;
            if !needs_to_be_returned { continue }

            try_ret |= is_next_in_chain;
            if !is_next_in_chain {
                self.enqueue_packet(loc, packet.clone());
            }
        }
        if !needed {
            return false
        }
        trace!("FUZZY checking {:?} for blockers in {:?}", locs, deps);
        for &OrderIndex(chain, index) in deps {
            let blocker_already_returned = self.per_chains.get_mut(&chain)
                .map(|pc| pc.has_returned(index))
                .unwrap_or(true);
            if !blocker_already_returned {
                trace!("FUZZY read @ {:?} blocked on {:?}", locs, (chain, index));
                //TODO no-alloc?
                self.blockers.entry(OrderIndex(chain, index))
                    .or_insert_with(Vec::new)
                    .push(packet.clone());
                self.fetch_blocker_at(chain, index);
            } else {
                trace!("FUZZY read @ {:?} need not wait for {:?}", locs, (chain, index));
            }
        }
        try_ret
    }

    /// Blocks a packet on entries a it depends on. Will increment the refcount for each
    /// blockage.
    fn add_blockers_at(&mut self, loc: OrderIndex, packet: &ChainEntry) -> bool {
        //FIXME dependencies currently assumes you gave it the correct type
        //      this is unnecessary and should be changed
        let entr = bytes_as_entry(packet);
        let deps = entr.dependencies();
        let (needed, try_ret);
        {
            let pc = self.per_chains.get(&loc.0).expect("blocking uninteresting chain");
            needed = !pc.has_returned(loc.1);
            try_ret = pc.next_return_is(loc.1);
        }
        if !needed { return false }
        if !try_ret {
            self.enqueue_packet(loc, Rc::clone(packet));
        }
        trace!("FUZZY checking {:?} for blockers in {:?}", loc, deps);
        for &OrderIndex(chain, index) in deps {
            let blocker_already_returned = self.per_chains.get_mut(&chain).map(|pc|{
                pc.has_returned(index)
            }).unwrap_or(true);
            if !blocker_already_returned {
                trace!("FUZZY read @ {:?} blocked on {:?}", loc, (chain, index));
                //TODO no-alloc?
                self.blockers.entry(OrderIndex(chain, index))
                    .or_insert_with(Vec::new)
                    .push(Rc::clone(packet));
                self.fetch_blocker_at(chain, index);
            } else {
                trace!("FUZZY read @ {:?} need not wait for {:?}", loc, (chain, index));
            }
        }
        try_ret
    }

    fn wait_for_deps(&mut self, packet: &ChainEntry) -> bool {
        //FIXME dependencies currently assumes you gave it the correct type
        //      this is unnecessary and should be changed
        let entr = bytes_as_entry(packet);
        let locs = entr.locs();
        let mut is_blocked = false;
        for &loc in locs.iter().skip_while(|&&OrderIndex(o, _)| o != order::from(0u64)).skip(1) {
            let block = self.per_chains.get(&loc.0).map(|pc| !pc.has_returned(loc.1))
                .unwrap_or(/*TODO*/ false);
            is_blocked |= block;
            if block {
                let blocked = self.blockers.entry(loc).or_insert_with(Vec::new);
                blocked.push(packet.clone());
            }
        }
        !is_blocked
    }

    // FIXME This is unneeded
    fn fetch_blockers_if_needed(&mut self, packet: &ChainEntry) {
        //TODO num_to_fetch
        //FIXME only do if below last_snapshot?
        let deps = bytes_as_entry(packet).dependencies();
        for &OrderIndex(chain, index) in deps {
            self.fetch_blocker_at(chain, index)
        }
    }

    fn fetch_blocker_at(&mut self, chain: order, index: entry) {
        let unblocked;
        let to_fetch: NextToFetch = {
            let (ub, to_fetch) = self.per_chains.get_mut(&chain).map(|pc|{
                let ub = pc.update_horizon(index);
                (ub, pc.next_range_to_fetch())
            }).unwrap_or_else(|| (None, NextToFetch::None));
            unblocked = ub;
            to_fetch
        };
        trace!("FUZZY blocker {:?} needs additional reads {:?}", chain, to_fetch);
        if let NextToFetch::BelowHorizon(low, high) = to_fetch {
            self.fetch_next(chain, low, high)
        }
        if let Some(val) = unblocked {
            let locs = self.return_entry(val);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }
    }

    fn try_returning_at(&mut self, loc: OrderIndex, packet: ChainEntry) {
        match Rc::try_unwrap(packet) {
            Ok(e) => {
                trace!("FUZZY read {:?} is next", loc);
                if self.return_entry_at(loc, e) {
                    self.stop_blocking_on(iter::once(loc));
                }
            }
            //TODO should this be in add_blockers?
            Err(e) => self.fetch_blockers_if_needed(&e),
        }
    }

    fn try_returning(&mut self, packet: ChainEntry) {
        match Rc::try_unwrap(packet) {
            Ok(e) => {
                trace!("FUZZY returning next read?");
                if let Some(locs) = self.return_entry(e) {
                    trace!("FUZZY {:?} unblocked", locs);
                    self.stop_blocking_on(locs);
                }
            }
            //TODO should this be in add_blockers?
            Err(e) => self.fetch_blockers_if_needed(&e),
        }
    }

    fn stop_blocking_on<I>(&mut self, locs: I)
    where I: IntoIterator<Item=OrderIndex> {
        for loc in locs {
            if loc.0 == order::from(0u64) { continue }
            trace!("FUZZY unblocking reads after {:?}", loc);
            self.try_return_blocked_by(loc);
        }
        while let Some(loc) = self.no_longer_blocked.pop() {
            trace!("FUZZY continue unblocking reads after {:?}", loc);
            self.try_return_blocked_by(loc);
        }
    }

    fn try_return_blocked_by(&mut self, loc: OrderIndex) {
        //FIXME switch to using try_returning so needed fetches are done
        //      move up the stop_block loop into try_returning?
        let blocked = self.blockers.remove(&loc);
        if let Some(blocked) = blocked {
            for blocked in blocked.into_iter() {
                match Rc::try_unwrap(blocked) {
                    Ok(val) => {
                        {
                            let locs = bytes_as_entry(&val).locs();
                            trace!("FUZZY {:?} unblocked by {:?}", locs, loc);
                            self.no_longer_blocked.extend_from_slice(locs);
                        }
                        self.return_entry(val);
                    }
                    Err(still_blocked) => {
                        trace!("FUZZY {:?} no longer by {:?} but still blocked by {:?}",
                            bytes_as_entry(&still_blocked).locs(), loc,
                                Rc::strong_count(&still_blocked))
                    }
                }
            }
        }
    }

    fn ensure_read(&mut self, read_loc: OrderIndex, packet: &ChainEntry) {
        let e = bytes_as_entry(packet);
        let id = e.id();
        let first = self.per_chains.get_mut(&read_loc.0).map(|p| p.got_no_remote(id));
        if let Some(true) = first {
            for &OrderIndex(ref o, _) in e.locs() {
                if o == &read_loc.0 { continue }
                if self.fetch_boring_multis { unimplemented!("which has priority?") }
                self.per_chains.get_mut(o).map(|p| p.add_no_remote(id));
            }
        }
    }

    fn update_multi_part_read(&mut self,
        read_loc: OrderIndex,
        mut msg: Vec<u8>,
        is_sentinel: bool)
    -> MultiSearch {
        let (id, is_multi_server) = {
            let entr = bytes_as_entry(&msg);
            let id = entr.id().clone();
            trace!("FUZZY multi part read {:?} @ {:?}", id, entr.locs());
            (id, entr.flag().contains(EntryFlag::TakeLock))
        };

        //TODO this should never really occur...
        // if num_pieces == 1 {
        //     return MultiSearch::Finished(msg)
        // }

        let is_later_piece = self.blocked_multiappends.contains_key(&id);
        if !is_later_piece && !is_sentinel {
            {
                let pc = &self.per_chains[&read_loc.0];
                //FIXME I'm not sure if this is right
                if !pc.is_within_snapshot(read_loc.1) {
                    //FIXME this occasionally breaks things
                    if bytes_as_entry(&mut msg).locs().iter()
                        .all(|&OrderIndex(o, i)| o == order::from(0u64) || i != entry::from(0u64)) {
                        return MultiSearch::Finished(msg)
                    }
                    trace!("FUZZY read multi too early @ {:?}", read_loc);
                    return MultiSearch::BeyondHorizon(msg)
                }

                if pc.has_returned(read_loc.1) {
                    trace!("FUZZY duplicate multi @ {:?}", read_loc);
                    return MultiSearch::BeyondHorizon(msg)
                }
            }

            //let mut pieces_remaining = num_pieces;
            trace!("FUZZY first part of multi part read");
            let mut finished = true;
            let mut is_sentinel = false;
            let mut wait_for_deps = false;
            {
                let mut entr = bytes_as_entry_mut(&mut msg);
                let fastpath = !entr.flag_mut().contains(EntryFlag::TakeLock);
                for &mut OrderIndex(o, ref mut i) in entr.locs_mut() {
                    if o == order::from(0u64) {
                        is_sentinel = true;
                        continue
                    }

                    trace!("FUZZY fetching multi part @ {:?}?", (o, *i));
                    if self.per_chains.get(&o).is_none() && !self.fetch_boring_multis {
                        continue
                    } else {
                        let early_sentinel = self.fetch_multi_parts(&id, o, *i, is_multi_server);
                        if let Some(loc) = early_sentinel {
                            trace!("FUZZY no fetch @ {:?} sentinel already found", (o, *i));
                            assert!(loc != entry::from(0u64));
                            *i = loc;
                        } else if *i != entry::from(0u64) {
                            trace!("FUZZY multi shortcircuit @ {:?}", (o, *i));
                            if is_sentinel {
                                if fastpath {
                                    wait_for_deps |= self.per_chains.get(&o).map(|pc|
                                        pc.can_return(*i)).unwrap_or(false);
                                } else {
                                    self.per_chains.get_mut(&o)
                                        .map(|pc| pc.mark_as_skippable(*i));
                                }
                            }
                        } else {
                            finished = false
                        }
                    }
                }
            }

            if finished {
                if wait_for_deps {
                    return MultiSearch::WaitForDeps(msg)
                }
                trace!("FUZZY all sentinels had already been found for {:?}", read_loc);
                return MultiSearch::Finished(msg)
            }

            //trace!("FUZZY {:?} waiting", read_loc, pieces_remaining);
            self.blocked_multiappends.insert(id, MultiSearchState {
                val: msg,
                //pieces_remaining: pieces_remaining
            });

            return MultiSearch::InProgress
        }
        else if !is_later_piece && is_sentinel {
            trace!("FUZZY early sentinel");
            self.per_chains.get_mut(&read_loc.0)
                .expect("boring sentinel")
                .add_early_sentinel(id, read_loc.1);
            return MultiSearch::EarlySentinel
        }

        trace!("FUZZY later part of multi part read");

        debug_assert!(self.per_chains[&read_loc.0].is_within_snapshot(read_loc.1));


        let mut finished = true;
        let mut found = match self.blocked_multiappends.entry(id) {
            hash_map::Entry::Occupied(o) => o,
            _ => unreachable!(),
        };
        {
            let multi = found.get_mut();
            if !is_sentinel {
                unsafe {
                    debug_assert_eq!(data_bytes(&multi.val), data_bytes(&msg))
                }
            }
            {
                let mut e = bytes_as_entry_mut(&mut multi.val);
                let my_locs = e.locs_mut();
                {
                    let locs = my_locs.iter_mut().zip(bytes_as_entry(&msg).locs().iter());
                    let mut is_sentinel = false;
                    for (my_loc, new_loc) in locs {
                        assert_eq!(my_loc.0, new_loc.0);
                        if my_loc.0 == order::from(0u64) {
                            is_sentinel = true;
                            continue
                        }

                        if my_loc.1 == entry::from(0u64) && new_loc.1 != entry::from(0u64) {
                            trace!("FUZZY finished blind seach for {:?}", new_loc);
                            *my_loc = *new_loc;
                            let pc = self.per_chains.entry(new_loc.0)
                                .or_insert_with(|| PerColor::new(new_loc.0));
                            pc.decrement_multi_search();
                            if !is_sentinel {
                                pc.mark_as_already_fetched(new_loc.1);
                            } else {
                                pc.mark_as_skippable(new_loc.1);
                            }
                        } else if my_loc.1 != entry::from(0u64) && new_loc.1!= entry::from(0u64) {
                            debug_assert_eq!(*my_loc, *new_loc);
                            if is_sentinel {
                                self.per_chains.get_mut(&new_loc.0)
                                    .map(|pc| pc.mark_as_skippable(new_loc.1));
                            }
                        }

                        finished &= my_loc.1 != entry::from(0u64);
                    }
                }
                trace!("FUZZY multi pieces remaining {:?}", my_locs);
            }
        }
        let finished = match finished {
            true => Some(found.remove().val),
            false => None,
        };

        // if was_blind_search {
        //     trace!("FUZZY finished blind seach for {:?}", read_loc);
        //     let pc = self.per_chains.entry(read_loc.0)
        //         .or_insert_with(|| PerColor::new(read_loc.0));
        //     pc.decrement_multi_search();
        // }

        match finished {
            Some(val) => {
                trace!("FUZZY finished multi part read");
                MultiSearch::Finished(val)
            }
            None => {
                trace!("FUZZY multi part read still waiting");
                MultiSearch::InProgress
            }
        }
    }

    fn fetch_multi_parts(&mut self, id: &Uuid, chain: order, index: entry, multi_server: bool)
    -> Option<entry> {
        //TODO argh, no-alloc
        let (unblocked, early_sentinel) = {
            let fech_boring = &self.fetch_boring_multis;
            let pc = self.per_chains.entry(chain)
                .or_insert_with(|| {assert!(fech_boring); PerColor::new(chain)});

            let early_sentinel = pc.take_early_sentinel(&id);
            let potential_new_horizon = match early_sentinel {
                Some(loc) => loc,
                None => index,
            };

            //perform a non blind search if possible
            //TODO less than ideal with new lock scheme
            //     lock index is always below color index, starting with a non-blind read
            //     based on the lock number should be balid, if a bit conservative
            //     this would require some way to fall back to a blind read,
            //     if the horizon was reached before the multi found
            if index != entry::from(0u64) /* && !pc.is_within_snapshot(index) */ {
                trace!("RRRRR non-blind search {:?} {:?}", chain, index);
                let unblocked = pc.update_horizon(potential_new_horizon);
                //TODO with opt, only for non-sentinels
                if multi_server {
                    pc.mark_as_already_fetched(index);
                }
                (unblocked, early_sentinel)
            } else if early_sentinel.is_some() {
                trace!("RRRRR already found {:?} {:?}", chain, early_sentinel);
                //FIXME How does this interact with cached reads?
                (None, early_sentinel)
            } else {
                trace!("RRRRR blind search {:?}", chain);
                pc.increment_multi_search(&self.chains_currently_being_read);
                (None, None)
            }
        };
        self.continue_fetch_if_needed(chain);

        if let Some(unblocked) = unblocked {
            //TODO no-alloc
            let locs = self.return_entry(unblocked);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }
        early_sentinel
    }

    fn continue_fetch_if_needed(&mut self, chain: order) -> bool {
        //TODO num_to_fetch
        let (num_to_fetch, unblocked) = {
            let pc = match self.per_chains.entry(chain) {
                hash_map::Entry::Occupied(o) => o.into_mut(),
                hash_map::Entry::Vacant(v) => if false {
                    //FIXME flag to fetch uninteresting chains
                    v.insert(PerColor::new(chain))
                } else {
                    return true
                },
            };
            let to_fetch = pc.next_range_to_fetch();
            //TODO should fetch == number of multis searching for
            match to_fetch {
                NextToFetch::BelowHorizon(low, high) => {
                    trace!("FUZZY {:?} needs additional reads {:?}", chain, (low, high));
                    (Some((low, high)), None)
                },
                NextToFetch::AboveHorizon(low, _)
                    if pc.has_more_multi_search_than_outstanding_reads() => {
                    trace!("FUZZY {:?} updating horizon due to multi search", chain);
                    (Some((low, low)), pc.increment_horizon())
                },
                _ => {
                    trace!("FUZZY {:?} needs no more reads", chain);
                    (None, None)
                },
            }
        };

        if let Some((low, high)) = num_to_fetch {
            //FIXME check if we have a cached version before issuing fetch
            //      laking this can cause unsound behzvior on multipart reads
            self.fetch_next(chain, low, high)
        }

        if let Some(unblocked) = unblocked {
            //TODO no-alloc
            let locs = self.return_entry(unblocked);
            if let Some(locs) = locs { self.stop_blocking_on(locs) }
        }

        self.server_is_finished(chain)
    }

    fn enqueue_packet(&mut self, loc: OrderIndex, packet: ChainEntry) {
        assert!(loc.1 > 1.into());
        debug_assert!(
            !self.per_chains[&loc.0].next_return_is(loc.1)
            && !self.per_chains[&loc.0].has_returned(loc.1),
            //self.per_chains.get(&loc.0).unwrap().last_returned_to_client
            //< loc.1 - 1,
            "tried to enqueue non enqueable entry {:?};",// last returned {:?}",
            loc.1 - 1,
            //self.per_chains.get(&loc.0).unwrap().last_returned_to_client,
        );
        let blocked_on = OrderIndex(loc.0, loc.1 - 1);
        trace!("FUZZY read @ {:?} blocked on prior {:?}", loc, blocked_on);
        //TODO no-alloc?
        let blocked = self.blockers.entry(blocked_on).or_insert_with(Vec::new);
        blocked.push(packet.clone());
    }

    fn return_entry_at(&mut self, loc: OrderIndex, val: Vec<u8>) -> bool {
        //debug_assert!(bytes_as_entry(&val).locs()[0] == loc);
        //debug_assert!(bytes_as_entry(&val).locs().len() == 1);
        trace!("FUZZY trying to return read @ {:?}", loc);
        let OrderIndex(o, i) = loc;

        let is_interesting = {
            let pc = match self.per_chains.get_mut(&o) {
                Some(pc) => pc,
                //TODO or true?
                None => return false,
            };

            if pc.has_returned(i) {
                return false
            }

            if !pc.is_within_snapshot(i) {
                trace!("FUZZY blocking read @ {:?}, waiting for snapshot", loc);
                pc.block_on_snapshot(val);
                return false
            }

            trace!("QQQQQ setting returned {:?}", (o, i));
            assert!(i > entry::from(0u64));
            pc.set_returned(i);
            pc.is_interesting
        };
        if is_interesting && !self.my_colors_chains.is_empty() {
            let OrderIndex(o, i) = loc;
            if self.my_colors_chains.contains(&o) {
                let e = self.last_seen_entries.entry(o).or_insert(0.into());
                if *e < i { *e = i }
            }
        }
        trace!("FUZZY returning read @ {:?}", loc);
        if is_interesting {
            //FIXME first_buffered?
            if self.ready_reads.send(Ok(val)).is_err() {
                self.finished = true;
            }
        }
        true
    }

    ///returns None if return stalled Some(Locations which are now unblocked>) if return
    ///        succeeded
    //TODO it may make sense to change these funtions to add the returned messages to an
    //     internal ring which can be used to discover the unblocked entries before the
    //     messages are flushed to the client, as this would remove the intermidate allocation
    //     and it may be a bit nicer
    fn return_entry(&mut self, val: Vec<u8>) -> Option<Vec<OrderIndex>> {
        let (locs, is_interesting) = {
            let mut should_block_on = None;
            {
                let e = bytes_as_entry(&val);
                let no_remote = e.flag().contains(EntryFlag::NoRemote);
                let locs = e.locs();
                trace!("FUZZY trying to return read from {:?}", locs);
                let mut checking_sentinels = false;
                for &OrderIndex(o, i) in locs.into_iter() {
                    if o == order::from(0u64) {
                        checking_sentinels = true;
                        continue
                    }
                    match (self.per_chains.get_mut(&o), no_remote) {
                        (None, true) => {}
                        (None, false) => {},
                        (Some(pc), _) => {
                            if pc.has_returned(i) {
                                if !checking_sentinels {
                                    //TODO is this an error?
                                    trace!("FUZZY double return {:?} in {:?}", (o, i), locs);
                                    return None
                                }
                            } else if checking_sentinels {
                                //FIXME this should be unreachable
                                panic!("FUZZY must block read on Sentinel {:?}: {:?}",
                                    (o, i), pc);
                            }
                            if !pc.is_within_snapshot(i) {
                                trace!("FUZZY must block read @ {:?}, waiting for snapshot", (o, i));
                                should_block_on = Some((o, i));
                            }
                        },
                    }
                }
            }
            if let Some((o, i)) = should_block_on {
                let is_next;
                {
                    let pc = self.per_chains.get_mut(&o)
                        .expect("blocking on uninteresting chain");
                    is_next = pc.next_return_is(i);
                    if is_next {
                        pc.block_on_snapshot(val);
                        return None
                    }
                }
                self.enqueue_packet(OrderIndex(o, i), Rc::new(val));
                return None
            }
            let mut is_interesting = false;
            let e = bytes_as_entry(&val);
            let no_remote = e.flag().contains(EntryFlag::NoRemote);
            let locs = e.locs();
            for &OrderIndex(o, i) in locs.into_iter() {
                if o == order::from(0u64) { break }
                match (self.per_chains.get_mut(&o), no_remote) {
                    (None, _) => {}
                    //(None, false) => panic!("trying to return boring chain {:?}", o),
                    (Some(pc), _) => {
                        trace!("QQQQ setting returned {:?}", (o, i));
                        debug_assert!(pc.is_within_snapshot(i));
                        pc.set_returned(i);
                        is_interesting |= pc.is_interesting;
                    },
                }
            }
            let check_color = !self.my_colors_chains.is_empty();
            for &OrderIndex(o, i) in locs.into_iter() {
                if is_interesting && check_color && self.my_colors_chains.contains(&o) {
                    let e = self.last_seen_entries.entry(o).or_insert(0.into());
                    if *e < i { *e = i }
                }
            }
            //TODO no-alloc
            //     a better solution might be to have this function push onto a temporary
            //     VecDeque who's head is used to unblock further entries, and is then sent
            //     to the client
            (locs.to_vec(), is_interesting)
        };
        trace!("FUZZY returning read @ {:?}", locs);
        if is_interesting {
            //FIXME first_buffered?
            if self.ready_reads.send(Ok(val)).is_err() {
                self.finished = true;
            }
        }
        Some(locs)
    }

    fn fetch_next(&mut self, chain: order, low: u64, high: u64) {
        {
            let per_chain = &mut self.per_chains.get_mut(&chain)
                .expect("fetching uninteresting chain");
            //assert!(per_chain.last_read_sent_to_server < per_chain.last_snapshot,
            //    "last_read_sent_to_server {:?} >= {:?} last_snapshot @ fetch_next",
            //    per_chain.last_read_sent_to_server, per_chain.last_snapshot,
            //);
            per_chain.fetching_range((low.into(), high.into()),
                &self.chains_currently_being_read)
        };
        for next in low..high+1 {
            let packet = self.make_read_packet(chain, next.into());
            if self.to_store.send(packet).is_err() {
                self.finished = true;
            }
        }
    }

    fn make_read_packet(&mut self, chain: order, index: entry) -> Vec<u8> {
        let mut buffer = self.cache.alloc();
        EntryContents::Read{
            id: &Uuid::nil(),
            flags: &EntryFlag::Nothing,
            data_bytes: &0,
            dependency_bytes: &0,
            loc: &OrderIndex(chain, index),
            horizon: &OrderIndex(0.into(), 0.into()),
            min: &OrderIndex(0.into(), 0.into()),
        }.fill_vec(&mut buffer);
        buffer
    }

    fn finshed_reading(&mut self) -> bool {
        let finished = Rc::get_mut(&mut self.chains_currently_being_read).is_some();
        /*debug_assert_eq!({
            let mut currently_being_read = 0;
            for (_, pc) in self.per_chains.iter() {
                assert_eq!(pc.is_finished(), !pc.has_read_state());
                if !pc.is_finished() {
                    currently_being_read += 1
                }
                //still_reading |= pc.has_outstanding_reads()
            }
            // !still_reading == (self.servers_currently_being_read == 0)
            if finished != (currently_being_read == 0) {
                panic!("currently_being_read == {:?} @ finish {:?}",
                currently_being_read, finished);
            }
            currently_being_read == 0
        }, finished);*/
        debug_assert!(
            if finished {
                let _ = self.per_chains.iter().map(|(_, pc)| {
                    if !pc.has_outstanding() {
                        assert!(pc.finished_until_snapshot());
                    }
                    assert!(pc.is_finished());
                });
                true
            } else {
                true
            }
        );

        finished
    }

    fn server_is_finished(&self, chain: order) -> bool {
        let pc = &self.per_chains[&chain];
        assert!(!(!pc.has_outstanding_reads() && pc.has_pending_reads_reqs()));
        assert!(!(pc.is_searching_for_multi() && !pc.has_outstanding_reads()));
        pc.is_finished()
    }
}

//TODO no-alloc
struct BufferCache {
    vec_cache: VecDeque<Vec<u8>>,
    //     rc_cache: VecDeque<Rc<Vec<u8>>>,
    //     alloced: usize,
    //     avg_alloced: usize,
    //num_allocs: u64,
    //num_misses: u64,
}

impl BufferCache {
    fn new() -> Self {
        BufferCache{
            vec_cache: VecDeque::new(),
            //num_allocs: 0,
            //num_misses: 0,
        }
    }

    fn alloc(&mut self) -> Vec<u8> {
        //self.num_allocs += 1;
        self.vec_cache.pop_front().unwrap_or_else(||{
            //self.num_misses += 1;
            Vec::new()
        })
    }

    fn cache_buffer(&mut self, mut buffer: Vec<u8>) {
        //TODO
        //if self.vec_cache.len() < 100 {
        buffer.clear();
        self.vec_cache.push_front(buffer)
        //}
    }

    fn flush(&mut self) {
        //TODO replace with truncate
        /*println!("veccache len {:?}", self.vec_cache.len());
        let hits = self.num_allocs - self.num_misses;
        // hits / allocs = x / 100
        let hit_p = (100.0 * hits as f64) / self.num_allocs as f64;
        println!("num alloc {}, hit {}% ,\nhits {}, misses {}",
            self.vec_cache.len(), hit_p,
            hits, self.num_misses,
        );*/
        for _ in 100..self.vec_cache.len() {
            self.vec_cache.pop_back();
        }
        //self.num_allocs = 0;
        //self.num_misses = 0;
    }
}

impl AsyncStoreClient for mpsc::Sender<Message> {
    fn on_finished_read(&mut self, read_loc: OrderIndex, read_packet: Vec<u8>)
    -> Result<(), ()> {
        if bytes_as_entry(&*read_packet).locs().len() > 1 {
            // FIXME assert!(read_loc.1 != entry::from(0u64), "read {:?}", bytes_as_entry(&*read_packet));
        }
        self.send(Message::FromStore(ReadComplete(read_loc, read_packet)))
            .map(|_| ()).map_err(|_| ())
    }

    //TODO what info is needed?
    fn on_finished_write(&mut self, write_id: Uuid, write_locs: Vec<OrderIndex>)
    -> Result<(), ()> {
        self.send(Message::FromStore(WriteComplete(write_id, write_locs)))
            .map(|_| ()).map_err(|_| ())
    }

    fn on_io_error(&mut self, err: io::Error, server: usize)
    -> Result<(), ()> {
        self.send(Message::FromStore(IoError(err.kind(), server)))
            .map(|_| ()).map_err(|_| ())
    }
}

pub trait OnRead {
    type Error: ::std::fmt::Debug;

    fn send(&mut self, res: Result<Vec<u8>, Error>) -> Result<(), Self::Error>;
}

pub trait OnWrote {
    type Error: ::std::fmt::Debug;

    fn send(&mut self, res: Result<(Uuid, Vec<OrderIndex>), Error>) -> Result<(), Self::Error>;
}

impl OnRead for mpsc::Sender<Result<Vec<u8>, Error>> {
    type Error = mpsc::SendError<Result<Vec<u8>, Error>>;

    fn send(&mut self, res: Result<Vec<u8>, Error>) -> Result<(), Self::Error> {
        mpsc::Sender::send(self, res)
    }
}

impl OnWrote for mpsc::Sender<Result<(Uuid, Vec<OrderIndex>), Error>> {
    type Error = mpsc::SendError<Result<(Uuid, Vec<OrderIndex>), Error>>;

    fn send(&mut self, res: Result<(Uuid, Vec<OrderIndex>), Error>) -> Result<(), Self::Error> {
        mpsc::Sender::send(self, res)
    }
}

impl OnWrote for () {
    type Error = (); //TODO should be !

    fn send(&mut self, _res: Result<(Uuid, Vec<OrderIndex>), Error>) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum Response {
    Read(Vec<u8>),
    Wrote(Uuid, Vec<OrderIndex>),
    Err(Error),
}

impl OnRead for mpsc::Sender<Response> {
    type Error = mpsc::SendError<Response>;

    fn send(&mut self, res: Result<Vec<u8>, Error>) -> Result<(), Self::Error> {
        let resp = match res {
            Ok(read) => Response::Read(read),
            Err(err) => Response::Err(err),
        };
        mpsc::Sender::send(self, resp)
    }
}

impl OnWrote for mpsc::Sender<Response> {
    type Error = mpsc::SendError<Response>;

    fn send(&mut self, res: Result<(Uuid, Vec<OrderIndex>), Error>) -> Result<(), Self::Error> {
        let resp = match res {
            Ok((id, locs)) => Response::Wrote(id, locs),
            Err(err) => Response::Err(err),
        };
        mpsc::Sender::send(self, resp)
    }
}
