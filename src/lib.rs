#![allow(unused_imports)]
#![allow(non_camel_case_types)]
#![allow(unused_variables)]
#![allow(unused_must_use)]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(unused_unsafe)]
#![allow(dead_code)]

#[macro_use] extern crate bitflags;
#[macro_use] extern crate custom_derive;
#[cfg(test)] #[macro_use] extern crate grabbag_macros;
#[macro_use] extern crate log;
#[macro_use] extern crate newtype_derive;

#[cfg(feature = "dynamodb_tests")]
extern crate hyper;
#[cfg(feature = "dynamodb_tests")]
extern crate rusoto;

//#[cfg(test)]
//extern crate test;

extern crate bit_set;
extern crate byteorder;
extern crate deque;
extern crate rustc_serialize;
extern crate mio;
extern crate nix;
extern crate net2;
extern crate time;
extern crate toml;
extern crate rand;
extern crate uuid;
extern crate libc;
extern crate lazycell;
extern crate env_logger;

//FIXME only needed until repeated multiput returns is fixed
extern crate linked_hash_map;

#[macro_use]
mod general_tests;

pub mod storeables;
pub mod packets;
pub mod prelude;
pub mod local_store;
pub mod udp_store;
pub mod tcp_store;
pub mod multitcp_store;
pub mod servers;
pub mod servers2;
pub mod color_api;
pub mod async;
mod hash;
mod socket_addr;
//TODO only for testing, should be private
pub mod buffer;

pub mod c_binidings {

    use prelude::*;
    use local_store::LocalHorizon;
    //use tcp_store::TcpStore;
    use multitcp_store::TcpStore;
    use async::fuzzy_log::LogHandle;

    use std::collections::HashMap;
    use std::{mem, ptr, slice};

    use std::ffi::CStr;
    use std::net::SocketAddr;
    use std::os::raw::c_char;

    use std::sync::atomic::{AtomicBool, Ordering};

    use servers::tcp::Server;

    use mio::deprecated::EventLoop;

    //pub type DAG = DAGHandle<[u8], TcpStore<[u8]>, LocalHorizon>;
    //pub type DAG = DAGHandle<[u8], Box<Store<[u8]>>, LocalHorizon>;
    pub type DAG = LogHandle<[u8]>;
    pub type ColorID = u32;

    #[repr(C)]
    pub struct colors {
        numcolors: usize,
        mycolors: *const ColorID,
    }

    #[no_mangle]
    pub extern "C" fn new_dag_handle(lock_server_ip: *const c_char,
        num_chain_ips: usize, chain_server_ips: *const *const c_char,
        color: *const colors) -> Box<DAG> {
        assert_eq!(mem::size_of::<Box<DAG>>(), mem::size_of::<*mut u8>());
        //assert_eq!(num_ips, 1, "Multiple servers are not yet supported via the C API");
        assert!(chain_server_ips != ptr::null());
        assert!(unsafe {*chain_server_ips != ptr::null()});
        assert!(num_chain_ips >= 1);
        assert!(color != ptr::null());
        assert!(colors_valid(color));
        let _ = ::env_logger::init();
        let (lock_server_addr, server_addrs) = unsafe {
            let addrs = slice::from_raw_parts(chain_server_ips, num_chain_ips)
                .into_iter().map(|&s|
                    CStr::from_ptr(s).to_str().expect("invalid IP string")
                        .parse().expect("invalid IP addr")
                    ).collect::<Vec<SocketAddr>>();
            if lock_server_ip != ptr::null() {
                let lock_server_addr = CStr::from_ptr(lock_server_ip).to_str()
                    .expect("invalid IP string")
                    .parse().expect("invalid IP addr");
                (lock_server_addr, addrs)
            }
            else {
                (addrs[0], addrs)
            }
        };
        let colors = unsafe {slice::from_raw_parts((*color).mycolors, (*color).numcolors)};
        Box::new(LogHandle::spawn_tcp_log2(lock_server_addr, server_addrs.into_iter(),
            colors.into_iter().cloned().map(order::from)))
    }

    #[no_mangle]
    pub extern "C" fn new_dag_handle_from_config(config_filename: *const c_char,
        color: *const colors) -> Box<DAG> {
        assert_eq!(mem::size_of::<Box<DAG>>(), mem::size_of::<*mut u8>());
        assert!(config_filename != ptr::null());
        assert!(color != ptr::null());
        assert!(colors_valid(color));
        let _ = ::env_logger::init();

        let (lock_server_addr, chain_server_addrs) = read_config_file(config_filename);
        let (lock_server_addr, server_addrs) = {
            let addrs = chain_server_addrs.into_iter()
                .map(|s| s.parse().expect("Invalid server addr"))
                .collect::<Vec<SocketAddr>>();
            let lock_server_addr = lock_server_addr.map(|a| a.parse().expect("Invalid lockserver addr"));
            if let Some(a) = lock_server_addr {
                (a, addrs)
            }
            else {
                (addrs[0], addrs)
            }
        };
        let colors = unsafe {slice::from_raw_parts((*color).mycolors, (*color).numcolors)};
        Box::new(LogHandle::spawn_tcp_log2(lock_server_addr, server_addrs.into_iter(),
            colors.into_iter().cloned().map(order::from)))
    }

    //NOTE currently can only use 31bits of return value
    #[no_mangle]
    pub extern "C" fn append(dag: *mut DAG, data: *const u8, data_size: usize,
        inhabits: *const colors, depends_on: *const colors) -> u32 {
        assert!(data_size == 0 || data != ptr::null());
        assert!(inhabits != ptr::null());
        assert!(colors_valid(inhabits));
        assert!(data_size <= 8000);

        let (dag, data, inhabits) = unsafe {
            let s = slice::from_raw_parts((*inhabits).mycolors, (*inhabits).numcolors);
            (dag.as_mut().expect("need to provide a valid DAGHandle"),
                slice::from_raw_parts(data, data_size),
                mem::transmute(s))
        };
        let depends_on: &[order] = unsafe {
            if depends_on != ptr::null() {
                assert!(colors_valid(depends_on));
                let s = slice::from_raw_parts((*depends_on).mycolors, (*depends_on).numcolors);
                mem::transmute(s)
            }
            else {
                &[]
            }
        };
        dag.color_append(data, inhabits, depends_on);
        0
    }

    fn colors_valid(c: *const colors) -> bool {
        unsafe { c != ptr::null() &&
            ((*c).numcolors == 0 || (*c).mycolors != ptr::null()) }
    }

    //NOTE we need either a way to specify data size, or to pass out a pointer
    // this version simple assumes that no data+metadat passed in or out will be
    // greater than DELOS_MAX_DATA_SIZE
    #[no_mangle]
    pub extern "C" fn get_next(dag: *mut DAG, data_out: *mut u8, data_read: *mut usize,
        inhabits_out: *mut colors) -> u32 {
        assert!(data_out != ptr::null_mut());
        assert!(data_read != ptr::null_mut());
        assert!(inhabits_out != ptr::null_mut());

        let dag = unsafe {dag.as_mut().expect("need to provide a valid DAGHandle")};
        let data_out = unsafe { slice::from_raw_parts_mut(data_out, 8000)};
        let data_read = unsafe {data_read.as_mut().expect("must provide valid data_out")};
        let val = dag.get_next();
        unsafe {
            let (mycolors, numcolors) = match val {
                Some((data, inhabited_colors)) => {
                    *data_read = <[u8] as Storeable>::copy_to_mut(data, data_out);
                    let numcolors = inhabited_colors.len();
                    let mycolors = ::libc::malloc(mem::size_of::<ColorID>() * numcolors) as *mut _;
                    let s = slice::from_raw_parts_mut(mycolors, numcolors);
                    for i in 0..numcolors {
                        let e: entry = inhabited_colors[i].1;
                        s[i] = e.into();
                    }
                    //ptr::copy_nonoverlapping(&inhabited_colors[0], mycolors, numcolors);
                    (mycolors, numcolors)
                }
                None => {
                    (ptr::null_mut(), 0)
                }
            };

            ptr::write(inhabits_out, colors{ numcolors: numcolors, mycolors: mycolors});
        }
        0
    }

    #[no_mangle]
    pub extern "C" fn snapshot(dag: *mut DAG) {
        let dag = unsafe {dag.as_mut().expect("need to provide a valid DAGHandle")};
        dag.take_snapshot();
    }

    #[no_mangle]
    pub unsafe extern "C" fn close_dag_handle(dag: *mut DAG) {
        assert!(dag != ptr::null_mut());
        Box::from_raw(dag);
    }

    ////////////////////////////////////
    //         Server bindings        //
    ////////////////////////////////////

    #[no_mangle]
    pub extern "C" fn start_fuzzy_log_server(server_ip: *const c_char) -> ! {
        start_fuzzy_log_server_for_group(server_ip, 0, 1)
    }

    #[no_mangle]
    pub extern "C" fn start_fuzzy_log_server_thread(server_ip: *const c_char) {
        assert!(server_ip != ptr::null());
        let server_ip = unsafe {
            CStr::from_ptr(server_ip).to_str().expect("invalid IP string")
        };
        start_fuzzy_log_server_thread_from_group(server_ip, 0, 1)
    }

    #[no_mangle]
    pub extern "C" fn start_fuzzy_log_server_for_group(server_ip: *const c_char,
        server_number: u32, total_servers_in_group: u32) -> ! {
        let mut event_loop = EventLoop::new()
            .expect("unable to start server loop");
        let server_ip = unsafe {
            CStr::from_ptr(server_ip).to_str().expect("invalid IP string")
        };
        let mut server = start_server(&mut event_loop, server_ip, server_number,
            total_servers_in_group);
        let res = event_loop.run(&mut server);
        panic!("server stopped with: {:?}", res)
    }

    #[no_mangle]
    pub extern "C" fn start_fuzzy_log_server_thread_from_group(server_ip: &str,
        server_number: u32, total_servers_in_group: u32) {
            let server_started = AtomicBool::new(false);
            let ip_addr = server_ip.parse().expect("invalid IP addr");
            let started = unsafe {
                //This should be safe since the while loop at the of the function
                //prevents it from exiting until the server is started and
                //server_started is no longer used
                extend_lifetime(&server_started)
            };
            let handle = ::std::thread::spawn(move || {
                let mut event_loop = EventLoop::new()
                    .expect("unable to start server loop");
                let mut server = Server::new(&ip_addr, server_number, total_servers_in_group, &mut event_loop)
                    .expect("unable to start server");
                started.store(true, Ordering::SeqCst);
                mem::drop(started);
                let res = event_loop.run(&mut server);
                panic!("server stopped with: {:?}", res)
            });
            while !server_started.load(Ordering::SeqCst) {}
            mem::forget(handle);
            mem::drop(server_ip);

            unsafe fn extend_lifetime<'a, 'b, T>(r: &'a T) -> &'b T {
                ::std::mem::transmute(r)
            }
    }

    fn start_server<'a, 'b>(event_loop: &'a mut EventLoop<Server>, server_ip: &'b str,
        server_num: u32, total_num_servers: u32) -> Server {
        let ip_addr = server_ip.parse().expect("invalid IP addr");
        Server::new(&ip_addr, server_num, total_num_servers, event_loop)
            .expect("unable to start server")
    }

    #[no_mangle]
    pub extern "C" fn start_servers_from_config(file_name: *const c_char) {
        assert!(file_name != ptr::null());
        let (lock_server_addr, chain_server_addrs) = read_config_file(file_name);
        if chain_server_addrs.len() > 1 && lock_server_addr.is_none() {
            panic!("Must provide a lock server if there are multiple chain servers")
        }
        if let Some(addr) = lock_server_addr {
            start_fuzzy_log_server_thread_from_group(&addr, 0, 1);
        }
        let total_chain_servers = chain_server_addrs.len() as u32;
        for (i, addr) in chain_server_addrs.into_iter().enumerate() {
            start_fuzzy_log_server_thread_from_group(&addr, i as u32, total_chain_servers);
        }
    }

    ////////////////////////////////////
    //           Config I/O           //
    ////////////////////////////////////

    fn read_config_file(file_name: *const c_char)
    -> (Option<String>, Vec<String>) {
        use std::fs::File;
        use std::io::Read;
        use toml::{self, Value};

        let file_name = unsafe { CStr::from_ptr(file_name) }
            .to_str().expect("Can only hanlde utf-8 filenames.");
        let mut config_string = String::new();
        {
            let mut config = File::open(file_name)
                .expect("Could not open config file.");
            config.read_to_string(&mut config_string)
                .expect("Invalid config file encoding.");
        }
        let mut vals = toml::Parser::new(&config_string)
            .parse().expect("Invalid config file format.");
        let lock_server_str =
            if let Some(Value::String(s)) = vals.remove("DELOS_LOCK_SERVER") {
                Some(s)
            } else { None };
        let css = vals.remove("DELOS_CHAIN_SERVERS")
            .expect("Must provide at least one chain server addr.");
        let chain_server_strings = if let Value::String(s) = css {
                s.split_whitespace().map(|s| s.to_string()).collect()
            } else { panic!("Must provide at least one chain server addr.") };
        (lock_server_str, chain_server_strings)
    }

    ////////////////////////////////////
    //    Old fuzzy log C bindings    //
    ////////////////////////////////////

    pub type Log = FuzzyLog<[u8], TcpStore<[u8]>, LocalHorizon>;

    #[no_mangle]
    pub extern "C" fn fuzzy_log_new(server_addr: *const c_char, relevent_chains: *const u32,
        num_relevent_chains: u16, callback: extern fn(*const u8, u16) -> u8) -> Box<Log> {
        let mut callbacks = HashMap::new();
        let relevent_chains = unsafe { slice::from_raw_parts(relevent_chains, num_relevent_chains as usize) };
        for &chain in relevent_chains {
            let callback: Box<Fn(&Uuid, &OrderIndex, &[u8]) -> bool> = Box::new(move |_, _, val| { callback(&val[0], val.len() as u16) != 0 });
            callbacks.insert(chain.into(), callback);
        }
        let server_addr_str = unsafe { CStr::from_ptr(server_addr).to_str().expect("invalid IP string") };
        //let ip_addr = server_addr_str.parse().expect("invalid IP addr");
        let log = FuzzyLog::new(TcpStore::new(server_addr_str, server_addr_str).unwrap(), LocalHorizon::new(), callbacks);
        Box::new(log)
    }

    #[no_mangle]
    pub extern "C" fn fuzzy_log_append(log: &mut Log,
        chain: u32, val: *const u8, len: u16, deps: *const OrderIndex, num_deps: u16) -> OrderIndex {
        unsafe {
            let val = slice::from_raw_parts(val, len as usize);
            let deps = slice::from_raw_parts(deps, num_deps as usize);
            log.append(chain.into(), val, deps)
        }
    }

    #[no_mangle]
    pub extern "C" fn fuzzy_log_multiappend(log: &mut Log,
        chains: *mut OrderIndex, num_chains: u16,
        val: *const u8, len: u16, deps: *const OrderIndex, num_deps: u16) {
        assert!(num_chains > 1);
        unsafe {
            let val = slice::from_raw_parts(val, len as usize);
            let deps = slice::from_raw_parts(deps, num_deps as usize);
            let chains = slice::from_raw_parts_mut(chains, num_chains as usize);
            log.multiappend2(chains, val, deps);
        }
    }

    #[no_mangle]
    pub extern "C" fn fuzzy_log_play_forward(log: &mut Log, chain: u32) -> OrderIndex {
        if let Some(oi) = log.play_foward(order::from(chain)) {
            oi
        }
        else {
            OrderIndex(0.into(), 0.into())
        }
    }

}
