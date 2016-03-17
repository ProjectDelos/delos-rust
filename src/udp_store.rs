
use prelude::*;

use std::fmt::Debug;
//use std::marker::{Unsize, PhantomData};
use std::marker::{PhantomData};
use std::mem::{self, size_of};
use std::net::{SocketAddr, UdpSocket};
//use std::ops::CoerceUnsized;
use std::slice;
use std::thread;

//use mio::buf::{SliceBuf, MutSliceBuf};
//use mio::udp::UdpSocket;
//use mio::unix;

use time::precise_time_ns;

use uuid::Uuid;

//#[derive(Debug)]
pub struct UdpStore<V> {
    socket: UdpSocket,
    server_addr: SocketAddr,
    receive_buffer: Box<Entry<V>>,
    send_buffer: Box<Entry<V>>,
    rtt: i64,
    dev: i64,
    _pd: PhantomData<V>,
}

const SLEEP_NANOS: u32 = 8000; //TODO user settable
const RTT: i64 = 80000;

impl<V: Copy + Debug + Eq> Store<V> for UdpStore<V> {

    fn insert(&mut self, key: OrderIndex, val: Entry<V>) -> InsertResult {
        let request_id = Uuid::new_v4();
        *self.send_buffer = val;
        assert_eq!(self.send_buffer.kind & EntryKind::Layout, EntryKind::Data);
        {
            let entr = unsafe { self.send_buffer.as_data_entry_mut() };
            entr.flex.loc = key;
            entr.id = request_id.clone();
        }
        trace!("packet {:#?}", self.send_buffer);

        trace!("at {:?}", self.socket.local_addr());
        let start_time = precise_time_ns() as i64;
        'send: loop {
            {
                trace!("sending");
                self.socket.send_to(self.send_buffer.bytes(), &self.server_addr)
                    .expect("cannot send insert"); //TODO
            }

            'receive: loop {
                let (_, addr) = {
                    self.socket.recv_from(self.receive_buffer.bytes_mut())
                        .expect("unable to receive ack") //TODO
                        //precise_time_ns() as i64 - start_time < self.rtt + 4 * self.dev
                };
                trace!("got packet");
                if addr == self.server_addr {
                    //if self.receive_buffer.kind & EntryKind::ReadSuccess == EntryKind::ReadSuccess {
                    //    trace!("invalid response r ReadSuccess at insert");
                    //    continue 'receive
                    //}
                    match self.receive_buffer.kind & EntryKind::Layout {
                        EntryKind::Data => { //TODO types?
                            trace!("correct response");
                            let entr = unsafe { self.receive_buffer.as_data_entry() };
                            if entr.flex.loc == key {
                                //let rtt = precise_time_ns() as i64 - start_time;
                                //self.rtt = ((self.rtt * 4) / 5) + (rtt / 5);
                                let sample_rtt = precise_time_ns() as i64 - start_time;
                                let diff = sample_rtt - self.rtt;
                                self.dev = self.dev + (diff.abs() - self.dev) / 4;
                                self.rtt = self.rtt + (diff * 4 / 5);
                                if entr.id == request_id {
                                    trace!("write success");
                                    return Ok(())
                                }
                                trace!("already written");
                                return Err(InsertErr::AlreadyWritten)
                            }
                            else {
                                println!("packet {:?}", self.receive_buffer);
                                continue 'receive
                            }
                        }
                        EntryKind::Multiput => {
                            match self.receive_buffer.contents() {
                                EntryContents::Multiput{columns, ..} => {
                                    if columns.contains(&key) {
                                        return Err(InsertErr::AlreadyWritten)
                                    }
                                    continue 'receive
                                }
                                _ => unreachable!(),
                            };
                        }
                        v => {
                            trace!("invalid response {:?}", v);
                            continue 'receive
                        }
                    }
                }
                else {
                    trace!("unexpected addr {:?}, expected {:?}", addr, self.server_addr);
                    continue 'receive
                }
            }
        }
    }

    fn get(&mut self, key: OrderIndex) -> GetResult<Entry<V>> {
        assert!(size_of::<V>() <= MAX_DATA_LEN);

        //let request_id = Uuid::new_v4();
        self.send_buffer.kind = EntryKind::Read;
        unsafe {
            self.send_buffer.as_data_entry_mut().flex.loc = key;
            self.send_buffer.id = mem::zeroed();
        };
        //self.send_buffer.id = request_id.clone();

        trace!("at {:?}", self.socket.local_addr());
        'send: loop {
            {
                trace!("sending");
                self.socket.send_to(self.send_buffer.bytes(), &self.server_addr)
                    .expect("cannot send get"); //TODO
            }

            //thread::sleep(Duration::new(0, SLEEP_NANOS)); //TODO

            let (_, addr) = {
                self.socket.recv_from(self.receive_buffer.bytes_mut())
                    .expect("unable to receive ack") //TODO
            };
            if addr == self.server_addr {
                trace!("correct addr");
                match self.receive_buffer.kind {
                    EntryKind::ReadData => {
                        //TODO validate...
                        //TODO base on loc instead?
                        if unsafe { self.receive_buffer.as_data_entry_mut().flex.loc } == key {
                            trace!("correct response");
                            return Ok(*self.receive_buffer.clone())
                        }
                        trace!("wrong loc {:?}, expected {:?}",
                            self.receive_buffer, key);
                        continue 'send
                    }
                    EntryKind::ReadMulti => {
                        //TODO base on loc instead?
                        if unsafe { self.receive_buffer.as_multi_entry_mut().multi_contents_mut()
                            .columns.contains(&key) } {
                            trace!("correct response");
                            return Ok(*self.receive_buffer.clone())
                        }
                        trace!("wrong loc {:?}, expected {:?}",
                            self.receive_buffer, key);
                        continue 'send
                    }
                    EntryKind::NoValue => {
                        if unsafe { self.receive_buffer.as_data_entry_mut().flex.loc } == key {
                            trace!("correct response");
                            return Err(GetErr::NoValue)
                        }
                        trace!("wrong loc {:?}, expected {:?}",
                            self.receive_buffer, key);
                        continue 'send
                    }
                    k => {
                        trace!("invalid response, {:?}", k);
                        continue 'send
                    }
                }
            }
            else {
                trace!("unexpected addr {:?}, expected {:?}", addr, self.server_addr);
                continue 'send
            }
        }
    }

    fn multi_append(&mut self, chains: &[OrderIndex], data: V, deps: &[OrderIndex]) -> InsertResult {
        let request_id = Uuid::new_v4();

        let contents = EntryContents::Multiput {
            data: &data,
            uuid: &request_id,
            columns: chains,
            deps: deps,
        };

        *self.send_buffer = EntryContents::Multiput {
            data: &data,
            uuid: &request_id,
            columns: chains,
            deps: deps,
        }.clone_entry();
        //self.send_buffer.kind = EntryKind::Multiput;
        self.send_buffer.id = request_id.clone();
        trace!("Tpacket {:#?}", self.send_buffer);

        {
            //let fd = self.socket.as_raw_fd();

        }

        //TODO find server

        trace!("multi_append from {:?}", self.socket.local_addr());
        let start_time = precise_time_ns() as i64;
        'send: loop {
            {
                trace!("sending");
                self.socket.send_to(self.send_buffer.bytes(), &self.server_addr)
                    .expect("cannot send insert"); //TODO
            }

            'receive: loop {
                let (size, addr) = {
                    self.socket.recv_from(self.receive_buffer.bytes_mut())
                        .expect("unable to receive ack") //TODO
                        //precise_time_ns() as i64 - start_time < self.rtt + 4 * self.dev
                };
                trace!("got packet");
                if addr == self.server_addr {
                    match self.receive_buffer.kind & EntryKind::Layout {
                        EntryKind::Multiput => { //TODO types?
                            trace!("correct response");
                            trace!("id {:?}", self.receive_buffer.id);
                            if self.receive_buffer.id == request_id {
                                trace!("multiappend success");
                                let sample_rtt = precise_time_ns() as i64 - start_time;
                                let diff = sample_rtt - self.rtt;
                                self.dev = self.dev + (diff.abs() - self.dev) / 4;
                                self.rtt = self.rtt + (diff * 4 / 5);
                                return Ok(())
                            }
                            else {
                                trace!("?? packet {:?}", self.receive_buffer);
                                continue 'receive
                            }
                        }
                        v => {
                            trace!("invalid response {:?}", v);
                            continue 'receive
                        }
                    }
                }
                else {
                    trace!("unexpected addr {:?}, expected {:?}", addr, self.server_addr);
                    continue 'receive
                }
            }
        }
    }
}

impl<V: Clone> Clone for UdpStore<V> {
    fn clone(&self) -> Self {
        let &UdpStore {ref server_addr, ref receive_buffer, ref send_buffer, _pd, rtt, dev, ..} = self;
        UdpStore {
            socket: UdpSocket::bind("0.0.0.0:0").expect("cannot clone"), //TODO
            server_addr: server_addr.clone(),
            receive_buffer: receive_buffer.clone(),
            send_buffer: send_buffer.clone(),
            rtt: rtt,
            dev: dev,
            _pd: _pd,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use prelude::*;

    use std::collections::HashMap;
    use std::collections::hash_map::Entry::{Occupied, Vacant};
    use std::mem::{self, forget, transmute};
    use std::net::UdpSocket;
    use std::thread::spawn;

    use test::Bencher;

    use mio::buf::{MutSliceBuf, SliceBuf};
    //use mio::udp::UdpSocket;

    #[allow(non_upper_case_globals)]
    fn new_store<V: ::std::fmt::Debug>(_: Vec<OrderIndex>) -> UdpStore<V>
    where V: Clone {
        let handle = spawn(move || {
            use mio::udp::UdpSocket;
            const addr_str: &'static str = "0.0.0.0:13265";
            let addr = addr_str.parse().expect("invalid inet address");
            //return; TODO
            let receive = if let Ok(socket) =
                UdpSocket::bound(&addr) {
                socket
            } else {
                trace!("socket in use");
                return
            };
            let mut log: HashMap<_, Box<Entry<V>>> = HashMap::with_capacity(10);
            let mut horizon = HashMap::with_capacity(10);
            let mut buff = Box::new(unsafe {mem::zeroed::<Entry<V>>()});
            trace!("starting server thread");
            'server: loop {
                let res = receive.recv_from(&mut MutSliceBuf::wrap(buff.bytes_mut()));
                match res {
                    Err(e) => panic!("{}", e),
                    Ok(Some(sa)) => {
                        trace!("server recieved from {:?}", sa);

                        let kind = {
                            buff.kind
                        };

                        trace!("server recieved {:?}", buff);
                        if let EntryKind::Multiput = kind & EntryKind::Layout {
                            {
                                let cols = {
                                    let entr = unsafe { buff.as_multi_entry_mut() };
                                    let packet = entr.multi_contents_mut();
                                    //trace!("multiput {:?}", packet);
                                    Vec::from(&*packet.columns)
                                };
                                let cols = unsafe {
                                    let packet = buff.as_multi_entry_mut().multi_contents_mut();
                                    for i in 0..cols.len() {
                                        let hor: entry = horizon.get(&cols[i].0).cloned().unwrap_or(0.into()) + 1;
                                        packet.columns[i].1 = hor;

                                        horizon.insert(packet.columns[i].0, hor);
                                    }
                                    Vec::from(&*packet.columns)
                                };
                                trace!("appending at {:?}", cols);
                                for loc in cols {
                                    let b = buff.clone();
                                    log.insert(loc,  b);
                                    //trace!("appended at {:?}", loc);
                                }
                            }
                            let slice = &mut SliceBuf::wrap(buff.bytes());
                            let _ = receive.send_to(slice, &sa).expect("unable to ack");
                        }
                        else {
                            let loc = unsafe { buff.as_data_entry().flex.loc };

                            match log.entry(loc) {
                                Vacant(e) => {
                                    trace!("Vacant entry {:?}", loc);
                                    match kind & EntryKind::Layout {
                                        EntryKind::Data => {
                                            trace!("writing");
                                            let packet = mem::replace(&mut buff, Box::new(unsafe {mem::zeroed::<Entry<V>>()}));
                                            let packet: &mut Box<Entry<V>> =
                                                unsafe { transmute_ref_mut(e.insert(packet)) };
                                            horizon.insert(loc.0, loc.1);
                                            //packet.header.kind = Kind::Written;
                                            let slice = &mut SliceBuf::wrap(packet.bytes());
                                            let o = receive.send_to(slice, &sa).expect("unable to ack");
                                        }
                                        _ => {
                                            //buff.kind = unoccupied_response(kind);
                                            let slice = &mut SliceBuf::wrap(buff.bytes());
                                            receive.send_to(slice, &sa).expect("unable to ack");
                                        }
                                    }
                                }
                                Occupied(mut e) => {
                                    trace!("Occupied entry {:?}", loc);
                                    let packet = e.get_mut();
                                    packet.kind = packet.kind | EntryKind::ReadSuccess;
                                    trace!("returning {:?}", packet);
                                    let slice = &mut SliceBuf::wrap(packet.bytes());
                                    receive.send_to(slice, &sa).expect("unable to ack");
                                }
                            };
                        }
                        //receive.send_to(&mut ByteBuf::from_slice(&[1]), &sa).expect("unable to ack");
                        //send.send_to(&mut ByteBuf::from_slice(&[1]), &sa).expect("unable to ack");
                    }
                    _ => continue 'server,
                }
            }
        });
        forget(handle);

        //const addr_str: &'static str = "172.28.229.152:13265";
        const addr_str: &'static str = "10.21.7.4:13265";
        //const addr_str: &'static str = "127.0.0.1:13265";

        unsafe {
            UdpStore {
                socket: UdpSocket::bind("0.0.0.0:0").expect("unable to open store"),
                server_addr: addr_str.parse().expect("invalid inet address"),
                receive_buffer: Box::new(mem::zeroed()),
                send_buffer: Box::new(mem::zeroed()),
                _pd: Default::default(),
                rtt: super::RTT,
                dev: 0,
            }
        }
    }

    general_tests!(super::new_store);

    unsafe fn transmute_ref_mut<T, U>(t: &mut T) -> &mut U {
        assert_eq!(mem::size_of::<T>(), mem::size_of::<U>());
        mem::transmute(t)
    }

    #[bench]
    fn many_writes(b: &mut Bencher) {
        let mut store = new_store(vec![]);
        let mut i = 0;
        let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
        b.iter(|| {
            store.insert((17.into(), i.into()), entr.clone());
            i.wrapping_add(1);
        });
    }
/*
    #[test]
    fn test_external_write() {
        let mut store = new_store(vec![]);
        let mut send: Box<Packet<u64>> = Box::new(unsafe { mem::zeroed() });
        send.data = EntryContents::Data(&48u64, &*vec![]).clone_entry();
        let mut recv: Box<Packet<u64>> = Box::new(unsafe { mem::zeroed() });

        let res = store.insert_ref((1.into(), 1.into()), &mut *send, &mut *recv);
        println!("res {:?}", res);
    }

    #[bench]
    fn external_write(b: &mut Bencher) {
        let mut store = new_store(vec![]);
        let mut send: Box<Packet<u64>> = Box::new(unsafe { mem::zeroed() });
        send.data = EntryContents::Data(&48u64, &*vec![]).clone_entry();
        let mut recv: Box<Packet<u64>> = Box::new(unsafe { mem::zeroed() });
        b.iter(|| {
            store.insert_ref((1.into(), 1.into()), &mut *send, &mut *recv)
        });
    }

    #[bench]
    fn many_writes(b: &mut Bencher) {
        let mut store = new_store(vec![]);
        let mut i = 0;
        b.iter(|| {
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            store.insert((17.into(), i.into()), entr);
            i.wrapping_add(1);
        });
    }

    #[bench]
    fn bench_write(b: &mut Bencher) {
        let mut store = new_store(vec![]);
        b.iter(|| {
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            store.insert((1.into(), 0.into()), entr)
        });
    }

    #[bench]
    fn bench_sequential_writes(b: &mut Bencher) {
        let mut store = new_store(vec![]);
        b.iter(|| {
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            let a = store.insert((0.into(), 0.into()), entr);
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            let b = store.insert((0.into(), 1.into()), entr);
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            let c = store.insert((0.into(), 2.into()), entr);
            (a, b, c)
        });
    }

    #[bench]
    fn bench_multistore_writes(b: &mut Bencher) {
        let mut store_a = new_store(vec![]);
        let mut store_b = new_store(vec![]);
        let mut store_c = new_store(vec![]);
        b.iter(|| {
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            let a = store_a.insert((0.into(), 0.into()), entr);
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            let b = store_b.insert((0.into(), 1.into()), entr);
            let entr = EntryContents::Data(&48u64, &*vec![]).clone_entry();
            let c = store_c.insert((0.into(), 2.into()), entr);
            (a, b, c)
        });
    }

    //#[bench]
    fn bench_rtt(b: &mut Bencher) {
        use std::mem;
        use mio::udp::UdpSocket;
        const addr_str: &'static str = "10.21.7.4:13265";
        let client = UdpSocket::v4().expect("unable to open client");
        let addr = addr_str.parse().expect("invalid inet address");
        let buff = Box::new([0u8; 4]);
        let mut recv_buff = Box::new([0u8; 4096]);
        b.iter(|| {
            let a = {
                let buff: &[u8] = &buff[..];
                let buf = &mut SliceBuf::wrap(buff);
                client.send_to(buf, &addr)
            };
            let mut recv = client.recv_from(&mut MutSliceBuf::wrap(&mut recv_buff[..]));
            while let Ok(None) = recv {
                recv = client.recv_from(&mut MutSliceBuf::wrap(&mut recv_buff[..]))
            }
            //println!("rec");
            a
        });
    }

    #[bench]
    fn bench_mio_write(b: &mut Bencher) {
        use mio::udp::UdpSocket;
        /*let handle = spawn(move || {
            const addr_str: &'static str = "0.0.0.0:13269";
            let addr = addr_str.parse().expect("invalid inet address");
            let receive = if let Ok(socket) =
                UdpSocket::bound(&addr) {
                socket
            } else {
                return
            };
            let mut buff = Box::new([0;4]);
            'server: loop {
                let res = receive.recv_from(&mut MutSliceBuf::wrap(&mut buff[..]));
                match res {
                    Err(e) => panic!("{}", e),
                    Ok(None) => {
                        continue 'server
                    }
                    Ok(Some(sa)) => {
                        let slice = &mut SliceBuf::wrap(&buff[..]);
                        receive.send_to(slice, &sa).expect("unable to ack");
                    }
                }
            }
        });*/

        const addr_str: &'static str = "172.28.229.152:13266";
        let client = UdpSocket::v4().expect("unable to open client");
        let addr = addr_str.parse().expect("invalid inet address");
        let mut buff = Box::new([0;4096]);
        b.iter(|| {
            let a = client.send_to(&mut SliceBuf::wrap(&buff[..]), &addr);
         //   let mut recv = client.recv_from(&mut MutSliceBuf::wrap(&mut buff[..]));
         //   while let Ok(None) = recv {
         //       recv = client.recv_from(&mut MutSliceBuf::wrap(&mut buff[..]))
         //   }
            a
        });
    }*/
}