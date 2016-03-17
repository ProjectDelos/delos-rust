
#[macro_use] extern crate log;

extern crate env_logger;
extern crate fuzzy_log;

//use std::collections::HashMap;
//use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::{mem, ptr};

use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};


use std::collections::HashSet;
use std::sync::Mutex;

use fuzzy_log::prelude::*;
//use fuzzy_log::udp_store::*;

pub struct Log<F=[u8; MAX_DATA_LEN2]> {
    horizon: HashMap<order, entry>,
    log: HashMap<OrderIndex, Box<Entry<(), F>>>,
}

#[no_mangle]
pub extern "C" fn handle_packet(log: &mut Log<DataFlex>, packet: &mut Entry<(), DataFlex>)
{
    trace!("{:#?}", packet.kind);
    let (kind, loc) = {
        (packet.kind, packet.flex.loc)
    };

    if kind & EntryKind::Layout == EntryKind::Multiput {
        trace!("bad packet");
        return
    }

    match log.log.entry(loc) {
        Vacant(e) => {
            trace!("Vacant entry {:?}", loc);
            match kind & EntryKind::Layout {
                EntryKind::Data => {
                    trace!("writing");
                    //t.set_kind(Kind::Written);
                    let data: Box<Entry<_, DataFlex>> = unsafe {
                        let mut ptr = Box::new(mem::uninitialized());
                        ptr::copy_nonoverlapping(packet, &mut *ptr, 1);
                        ptr.kind = kind | EntryKind::ReadSuccess;
                        ptr
                    };
                    e.insert(data);
                }
                _ => {
                    trace!("not write");
                    //packet.set_kind(unoccupied_response(kind));
                }
            }
        }
        Occupied(e) => {
            trace!("Occupied entry {:?}", loc);
            unsafe {
                ptr::copy_nonoverlapping::<Entry<_, _>>(&**e.get(), packet, 1);
                packet.kind = packet.kind | EntryKind::ReadSuccess;
            }
            //*packet = *e.get().clone();
            //packet.set_kind(occupied_response(kind));;
        }
    }
    //trace!("=============>\n{:#?}", packet.contents());
}

#[no_mangle]
pub unsafe extern "C" fn handle_multiappend(core_id: u32, ring_mask: u32,
    log: &mut Log<MultiFlex>, packet: *mut Entry<(), MultiFlex>)
{
    assert_eq!((*packet).kind, EntryKind::Multiput);
    assert!(packet != ptr::null_mut());
    trace!("multiappend! {}", core_id);

    let num_cols = (*packet).flex.cols; //TODO len
    let mut cols = &mut (*packet).flex.data as *mut _ as *mut OrderIndex;

    for _ in 0..num_cols {
        trace!("append? {:?} & {:?} == {:?} ?= {:?}", (*cols).0, ring_mask, (*cols).0 & ring_mask, core_id);
        if (*cols).0 & ring_mask == core_id.into() {
            (*cols).1 = {
                let old_horizon = log.horizon.entry((*cols).0).or_insert(0.into());
                *old_horizon = *old_horizon + 1;
                *old_horizon
            };
            trace!("appending at {:?}", *cols);
            let data: Box<Entry<_, MultiFlex>> = {
                let mut ptr = Box::new(mem::uninitialized()); //TODO where to copy?
                ptr::copy_nonoverlapping(packet, &mut *ptr, 1);
                ptr.kind = ptr.kind | EntryKind::ReadSuccess; //TODO why does this not suffice?
                ptr
            };
            //trace!("multiappend at {:?}", *cols);
            log.log.insert(*cols, data);
        }
        cols = cols.offset(1)
    }
}

#[no_mangle]
pub extern "C" fn init_log() -> Box<Log> {
	assert_eq!(mem::size_of::<Box<Log>>(), mem::size_of::<*mut u8>());
	let log = Box::new(Log{
	        horizon: HashMap::new(),
	        log: HashMap::new(),
	});
	trace!("logging start.");
	trace!("log init as {:?}.", &*log as *const _);
	log
}

#[no_mangle]
pub extern "C" fn rss_log(core: u32, chain: u32, set: &Mutex<HashSet<(u32, u32)>>) -> u32 {
	let mut set = set.lock().unwrap();
	let other_core = if core == 0 { 1 } else { 0 };
	set.insert((core, chain));
	return if set.contains(&(other_core, chain)) { println!("err at {:?}", (core, chain)); 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn rss_log_init() -> Box<Mutex<HashSet<(u32, u16)>>> {
	Box::new(Mutex::new(HashSet::new()))
}
