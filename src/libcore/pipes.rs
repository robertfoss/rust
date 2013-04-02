// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*! Runtime support for message passing with protocol enforcement.


Pipes consist of two endpoints. One endpoint can send messages and
the other can receive messages. The set of legal messages and which
directions they can flow at any given point are determined by a
protocol. Below is an example protocol.

~~~
proto! pingpong (
    ping: send {
        ping -> pong
    }
    pong: recv {
        pong -> ping
    }
)
~~~

The `proto!` syntax extension will convert this into a module called
`pingpong`, which includes a set of types and functions that can be
used to write programs that follow the pingpong protocol.

*/

/* IMPLEMENTATION NOTES

The initial design for this feature is available at:

https://github.com/eholk/rust/wiki/Proposal-for-channel-contracts

Much of the design in that document is still accurate. There are
several components for the pipe implementation. First of all is the
syntax extension. To see how that works, it is best see comments in
libsyntax/ext/pipes.rs.

This module includes two related pieces of the runtime
implementation: support for unbounded and bounded
protocols. The main difference between the two is the type of the
buffer that is carried along in the endpoint data structures.


The heart of the implementation is the packet type. It contains a
header and a payload field. Much of the code in this module deals with
the header field. This is where the synchronization information is
stored. In the case of a bounded protocol, the header also includes a
pointer to the buffer the packet is contained in.

Packets represent a single message in a protocol. The payload field
gets instatiated at the type of the message, which is usually an enum
generated by the pipe compiler. Packets are conceptually single use,
although in bounded protocols they are reused each time around the
loop.


Packets are usually handled through a send_packet_buffered or
recv_packet_buffered object. Each packet is referenced by one
send_packet and one recv_packet, and these wrappers enforce that only
one end can send and only one end can receive. The structs also
include a destructor that marks packets are terminated if the sender
or receiver destroys the object before sending or receiving a value.

The *_packet_buffered structs take two type parameters. The first is
the message type for the current packet (or state). The second
represents the type of the whole buffer. For bounded protocols, the
protocol compiler generates a struct with a field for each protocol
state. This generated struct is used as the buffer type parameter. For
unbounded protocols, the buffer is simply one packet, so there is a
shorthand struct called send_packet and recv_packet, where the buffer
type is just `packet<T>`. Using the same underlying structure for both
bounded and unbounded protocols allows for less code duplication.

*/

use cast::{forget, reinterpret_cast, transmute};
use either::{Either, Left, Right};
use kinds::Owned;
use libc;
use option::{None, Option, Some};
use unstable::intrinsics;
use ptr;
use task;
use vec;

#[doc(hidden)]
static SPIN_COUNT: uint = 0;

macro_rules! move_it (
    { $x:expr } => ( unsafe { let y = *ptr::addr_of(&($x)); y } )
)

#[doc(hidden)]
#[deriving(Eq)]
enum State {
    Empty,
    Full,
    Blocked,
    Terminated
}

pub struct BufferHeader {
    // Tracks whether this buffer needs to be freed. We can probably
    // get away with restricting it to 0 or 1, if we're careful.
    mut ref_count: int,

    // We may want a drop, and to be careful about stringing this
    // thing along.
}

pub fn BufferHeader() -> BufferHeader {
    BufferHeader {
        ref_count: 0
    }
}

// This is for protocols to associate extra data to thread around.
#[doc(hidden)]
pub struct Buffer<T> {
    header: BufferHeader,
    data: T,
}

pub struct PacketHeader {
    mut state: State,
    mut blocked_task: *rust_task,

    // This is a reinterpret_cast of a ~buffer, that can also be cast
    // to a buffer_header if need be.
    mut buffer: *libc::c_void,
}

pub fn PacketHeader() -> PacketHeader {
    PacketHeader {
        state: Empty,
        blocked_task: ptr::null(),
        buffer: ptr::null()
    }
}

pub impl PacketHeader {
    // Returns the old state.
    unsafe fn mark_blocked(&self, this: *rust_task) -> State {
        rustrt::rust_task_ref(this);
        let old_task = swap_task(&mut self.blocked_task, this);
        assert!(old_task.is_null());
        swap_state_acq(&mut self.state, Blocked)
    }

    unsafe fn unblock(&self) {
        let old_task = swap_task(&mut self.blocked_task, ptr::null());
        if !old_task.is_null() {
            unsafe {
                rustrt::rust_task_deref(old_task)
            }
        }
        match swap_state_acq(&mut self.state, Empty) {
          Empty | Blocked => (),
          Terminated => self.state = Terminated,
          Full => self.state = Full
        }
    }

    // unsafe because this can do weird things to the space/time
    // continuum. It ends making multiple unique pointers to the same
    // thing. You'll proobably want to forget them when you're done.
    unsafe fn buf_header(&self) -> ~BufferHeader {
        assert!(self.buffer.is_not_null());
        reinterpret_cast(&self.buffer)
    }

    fn set_buffer<T:Owned>(&self, b: ~Buffer<T>) {
        unsafe {
            self.buffer = reinterpret_cast(&b);
        }
    }
}

#[doc(hidden)]
pub struct Packet<T> {
    header: PacketHeader,
    mut payload: Option<T>,
}

#[doc(hidden)]
pub trait HasBuffer {
    fn set_buffer(&self, b: *libc::c_void);
}

impl<T:Owned> HasBuffer for Packet<T> {
    fn set_buffer(&self, b: *libc::c_void) {
        self.header.buffer = b;
    }
}

#[doc(hidden)]
pub fn mk_packet<T:Owned>() -> Packet<T> {
    Packet {
        header: PacketHeader(),
        payload: None,
    }
}
#[doc(hidden)]
fn unibuffer<T>() -> ~Buffer<Packet<T>> {
    let b = ~Buffer {
        header: BufferHeader(),
        data: Packet {
            header: PacketHeader(),
            payload: None,
        }
    };

    unsafe {
        b.data.header.buffer = reinterpret_cast(&b);
    }
    b
}

#[doc(hidden)]
pub fn packet<T>() -> *Packet<T> {
    let b = unibuffer();
    let p = ptr::addr_of(&(b.data));
    // We'll take over memory management from here.
    unsafe { forget(b) }
    p
}

#[doc(hidden)]
pub fn entangle_buffer<T:Owned,Tstart:Owned>(
    buffer: ~Buffer<T>,
    init: &fn(*libc::c_void, x: &T) -> *Packet<Tstart>)
    -> (SendPacketBuffered<Tstart, T>, RecvPacketBuffered<Tstart, T>)
{
    let p = init(unsafe { reinterpret_cast(&buffer) }, &buffer.data);
    unsafe { forget(buffer) }
    (SendPacketBuffered(p), RecvPacketBuffered(p))
}

// If I call the rusti versions directly from a polymorphic function,
// I get link errors. This is a bug that needs investigated more.
#[doc(hidden)]
pub fn atomic_xchng_rel(dst: &mut int, src: int) -> int {
    unsafe {
        intrinsics::atomic_xchg_rel(dst, src)
    }
}

#[doc(hidden)]
pub fn atomic_add_acq(dst: &mut int, src: int) -> int {
    unsafe {
        intrinsics::atomic_xadd_acq(dst, src)
    }
}

#[doc(hidden)]
pub fn atomic_sub_rel(dst: &mut int, src: int) -> int {
    unsafe {
        intrinsics::atomic_xsub_rel(dst, src)
    }
}

#[doc(hidden)]
pub fn swap_task(dst: &mut *rust_task, src: *rust_task) -> *rust_task {
    // It might be worth making both acquire and release versions of
    // this.
    unsafe {
        transmute(intrinsics::atomic_xchg(transmute(dst), src as int))
    }
}

#[doc(hidden)]
#[allow(non_camel_case_types)]
pub type rust_task = libc::c_void;

#[doc(hidden)]
pub mod rustrt {
    use libc;
    use super::rust_task;

    pub extern {
        #[rust_stack]
        unsafe fn rust_get_task() -> *rust_task;
        #[rust_stack]
        unsafe fn rust_task_ref(task: *rust_task);
        unsafe fn rust_task_deref(task: *rust_task);

        #[rust_stack]
        unsafe fn task_clear_event_reject(task: *rust_task);

        unsafe fn task_wait_event(this: *rust_task,
                                  killed: &mut *libc::c_void)
                               -> bool;
        unsafe fn task_signal_event(target: *rust_task, event: *libc::c_void);
    }
}

#[doc(hidden)]
fn wait_event(this: *rust_task) -> *libc::c_void {
    unsafe {
        let mut event = ptr::null();

        let killed = rustrt::task_wait_event(this, &mut event);
        if killed && !task::failing() {
            fail!(~"killed")
        }
        event
    }
}

#[doc(hidden)]
fn swap_state_acq(dst: &mut State, src: State) -> State {
    unsafe {
        transmute(intrinsics::atomic_xchg_acq(transmute(dst), src as int))
    }
}

#[doc(hidden)]
fn swap_state_rel(dst: &mut State, src: State) -> State {
    unsafe {
        transmute(intrinsics::atomic_xchg_rel(transmute(dst), src as int))
    }
}

#[doc(hidden)]
pub unsafe fn get_buffer<T>(p: *PacketHeader) -> ~Buffer<T> {
    transmute((*p).buf_header())
}

// This could probably be done with SharedMutableState to avoid move_it!().
struct BufferResource<T> {
    buffer: ~Buffer<T>,

}

#[unsafe_destructor]
impl<T> ::ops::Drop for BufferResource<T> {
    fn finalize(&self) {
        unsafe {
            let b = move_it!(self.buffer);
            //let p = ptr::addr_of(*b);
            //error!("drop %?", p);
            let old_count = atomic_sub_rel(&mut b.header.ref_count, 1);
            //let old_count = atomic_xchng_rel(b.header.ref_count, 0);
            if old_count == 1 {
                // The new count is 0.

                // go go gadget drop glue
            }
            else {
                forget(b)
            }
        }
    }
}

fn BufferResource<T>(b: ~Buffer<T>) -> BufferResource<T> {
    //let p = ptr::addr_of(*b);
    //error!("take %?", p);
    atomic_add_acq(&mut b.header.ref_count, 1);

    BufferResource {
        // tjc: ????
        buffer: b
    }
}

#[doc(hidden)]
pub fn send<T,Tbuffer>(p: SendPacketBuffered<T,Tbuffer>, payload: T) -> bool {
    let header = p.header();
    let p_ = p.unwrap();
    let p = unsafe { &*p_ };
    assert!(ptr::addr_of(&(p.header)) == header);
    assert!(p.payload.is_none());
    p.payload = Some(payload);
    let old_state = swap_state_rel(&mut p.header.state, Full);
    match old_state {
        Empty => {
            // Yay, fastpath.

            // The receiver will eventually clean this up.
            //unsafe { forget(p); }
            return true;
        }
        Full => fail!(~"duplicate send"),
        Blocked => {
            debug!("waking up task for %?", p_);
            let old_task = swap_task(&mut p.header.blocked_task, ptr::null());
            if !old_task.is_null() {
                unsafe {
                    rustrt::task_signal_event(
                        old_task,
                        ptr::addr_of(&(p.header)) as *libc::c_void);
                    rustrt::rust_task_deref(old_task);
                }
            }

            // The receiver will eventually clean this up.
            //unsafe { forget(p); }
            return true;
        }
        Terminated => {
            // The receiver will never receive this. Rely on drop_glue
            // to clean everything up.
            return false;
        }
    }
}

/** Receives a message from a pipe.

Fails if the sender closes the connection.

*/
pub fn recv<T:Owned,Tbuffer:Owned>(
    p: RecvPacketBuffered<T, Tbuffer>) -> T {
    try_recv(p).expect("connection closed")
}

/** Attempts to receive a message from a pipe.

Returns `None` if the sender has closed the connection without sending
a message, or `Some(T)` if a message was received.

*/
pub fn try_recv<T:Owned,Tbuffer:Owned>(p: RecvPacketBuffered<T, Tbuffer>)
    -> Option<T>
{
    let p_ = p.unwrap();
    let p = unsafe { &*p_ };

    #[unsafe_destructor]
    struct DropState<'self> {
        p: &'self PacketHeader,

        drop {
            unsafe {
                if task::failing() {
                    self.p.state = Terminated;
                    let old_task = swap_task(&mut self.p.blocked_task,
                                             ptr::null());
                    if !old_task.is_null() {
                        rustrt::rust_task_deref(old_task);
                    }
                }
            }
        }
    };

    let _drop_state = DropState { p: &p.header };

    // optimistic path
    match p.header.state {
      Full => {
        let mut payload = None;
        payload <-> p.payload;
        p.header.state = Empty;
        return Some(payload.unwrap())
      },
      Terminated => return None,
      _ => {}
    }

    // regular path
    let this = unsafe { rustrt::rust_get_task() };
    unsafe {
        rustrt::task_clear_event_reject(this);
        rustrt::rust_task_ref(this);
    };
    debug!("blocked = %x this = %x", p.header.blocked_task as uint,
           this as uint);
    let old_task = swap_task(&mut p.header.blocked_task, this);
    debug!("blocked = %x this = %x old_task = %x",
           p.header.blocked_task as uint,
           this as uint, old_task as uint);
    assert!(old_task.is_null());
    let mut first = true;
    let mut count = SPIN_COUNT;
    loop {
        unsafe {
            rustrt::task_clear_event_reject(this);
        }

        let old_state = swap_state_acq(&mut p.header.state,
                                       Blocked);
        match old_state {
          Empty => {
            debug!("no data available on %?, going to sleep.", p_);
            if count == 0 {
                wait_event(this);
            }
            else {
                count -= 1;
                // FIXME (#524): Putting the yield here destroys a lot
                // of the benefit of spinning, since we still go into
                // the scheduler at every iteration. However, without
                // this everything spins too much because we end up
                // sometimes blocking the thing we are waiting on.
                task::yield();
            }
            debug!("woke up, p.state = %?", copy p.header.state);
          }
          Blocked => if first {
            fail!(~"blocking on already blocked packet")
          },
          Full => {
            let mut payload = None;
            payload <-> p.payload;
            let old_task = swap_task(&mut p.header.blocked_task, ptr::null());
            if !old_task.is_null() {
                unsafe {
                    rustrt::rust_task_deref(old_task);
                }
            }
            p.header.state = Empty;
            return Some(payload.unwrap())
          }
          Terminated => {
            // This assert detects when we've accidentally unsafely
            // casted too big of a number to a state.
            assert!(old_state == Terminated);

            let old_task = swap_task(&mut p.header.blocked_task, ptr::null());
            if !old_task.is_null() {
                unsafe {
                    rustrt::rust_task_deref(old_task);
                }
            }
            return None;
          }
        }
        first = false;
    }
}

/// Returns true if messages are available.
pub fn peek<T:Owned,Tb:Owned>(p: &RecvPacketBuffered<T, Tb>) -> bool {
    match unsafe {(*p.header()).state} {
      Empty | Terminated => false,
      Blocked => fail!(~"peeking on blocked packet"),
      Full => true
    }
}

#[doc(hidden)]
fn sender_terminate<T:Owned>(p: *Packet<T>) {
    let p = unsafe { &*p };
    match swap_state_rel(&mut p.header.state, Terminated) {
      Empty => {
        // The receiver will eventually clean up.
      }
      Blocked => {
        // wake up the target
        let old_task = swap_task(&mut p.header.blocked_task, ptr::null());
        if !old_task.is_null() {
            unsafe {
                rustrt::task_signal_event(
                    old_task,
                    ptr::addr_of(&(p.header)) as *libc::c_void);
                rustrt::rust_task_deref(old_task);
            }
        }
        // The receiver will eventually clean up.
      }
      Full => {
        // This is impossible
        fail!(~"you dun goofed")
      }
      Terminated => {
        assert!(p.header.blocked_task.is_null());
        // I have to clean up, use drop_glue
      }
    }
}

#[doc(hidden)]
fn receiver_terminate<T:Owned>(p: *Packet<T>) {
    let p = unsafe { &*p };
    match swap_state_rel(&mut p.header.state, Terminated) {
      Empty => {
        assert!(p.header.blocked_task.is_null());
        // the sender will clean up
      }
      Blocked => {
        let old_task = swap_task(&mut p.header.blocked_task, ptr::null());
        if !old_task.is_null() {
            unsafe {
                rustrt::rust_task_deref(old_task);
                assert!(old_task == rustrt::rust_get_task());
            }
        }
      }
      Terminated | Full => {
        assert!(p.header.blocked_task.is_null());
        // I have to clean up, use drop_glue
      }
    }
}

/** Returns when one of the packet headers reports data is available.

This function is primarily intended for building higher level waiting
functions, such as `select`, `select2`, etc.

It takes a vector slice of packet_headers and returns an index into
that vector. The index points to an endpoint that has either been
closed by the sender or has a message waiting to be received.

*/
pub fn wait_many<T: Selectable>(pkts: &[T]) -> uint {
    let this = unsafe { rustrt::rust_get_task() };

    unsafe {
        rustrt::task_clear_event_reject(this);
    }

    let mut data_avail = false;
    let mut ready_packet = pkts.len();
    for pkts.eachi |i, p| {
        unsafe {
            let p = &*p.header();
            let old = p.mark_blocked(this);
            match old {
              Full | Terminated => {
                data_avail = true;
                ready_packet = i;
                (*p).state = old;
                break;
              }
              Blocked => fail!(~"blocking on blocked packet"),
              Empty => ()
            }
        }
    }

    while !data_avail {
        debug!("sleeping on %? packets", pkts.len());
        let event = wait_event(this) as *PacketHeader;
        let pos = vec::position(pkts, |p| p.header() == event);

        match pos {
          Some(i) => {
            ready_packet = i;
            data_avail = true;
          }
          None => debug!("ignoring spurious event, %?", event)
        }
    }

    debug!("%?", pkts[ready_packet]);

    for pkts.each |p| { unsafe{ (*p.header()).unblock()} }

    debug!("%?, %?", ready_packet, pkts[ready_packet]);

    unsafe {
        assert!((*pkts[ready_packet].header()).state == Full
                     || (*pkts[ready_packet].header()).state == Terminated);
    }

    ready_packet
}

/** Receives a message from one of two endpoints.

The return value is `left` if the first endpoint received something,
or `right` if the second endpoint receives something. In each case,
the result includes the other endpoint as well so it can be used
again. Below is an example of using `select2`.

~~~
match select2(a, b) {
  left((none, b)) {
    // endpoint a was closed.
  }
  right((a, none)) {
    // endpoint b was closed.
  }
  left((Some(_), b)) {
    // endpoint a received a message
  }
  right(a, Some(_)) {
    // endpoint b received a message.
  }
}
~~~

Sometimes messages will be available on both endpoints at once. In
this case, `select2` may return either `left` or `right`.

*/
pub fn select2<A:Owned,Ab:Owned,B:Owned,Bb:Owned>(
    a: RecvPacketBuffered<A, Ab>,
    b: RecvPacketBuffered<B, Bb>)
    -> Either<(Option<A>, RecvPacketBuffered<B, Bb>),
              (RecvPacketBuffered<A, Ab>, Option<B>)>
{
    let i = wait_many([a.header(), b.header()]);

    match i {
      0 => Left((try_recv(a), b)),
      1 => Right((a, try_recv(b))),
      _ => fail!(~"select2 return an invalid packet")
    }
}

#[doc(hidden)]
pub trait Selectable {
    fn header(&self) -> *PacketHeader;
}

impl Selectable for *PacketHeader {
    fn header(&self) -> *PacketHeader { *self }
}

/// Returns the index of an endpoint that is ready to receive.
pub fn selecti<T:Selectable>(endpoints: &[T]) -> uint {
    wait_many(endpoints)
}

/// Returns 0 or 1 depending on which endpoint is ready to receive
pub fn select2i<A:Selectable,B:Selectable>(a: &A, b: &B) ->
        Either<(), ()> {
    match wait_many([a.header(), b.header()]) {
      0 => Left(()),
      1 => Right(()),
      _ => fail!(~"wait returned unexpected index")
    }
}

/** Waits on a set of endpoints. Returns a message, its index, and a
 list of the remaining endpoints.

*/
pub fn select<T:Owned,Tb:Owned>(endpoints: ~[RecvPacketBuffered<T, Tb>])
    -> (uint, Option<T>, ~[RecvPacketBuffered<T, Tb>])
{
    let ready = wait_many(endpoints.map(|p| p.header()));
    let mut remaining = endpoints;
    let port = remaining.swap_remove(ready);
    let result = try_recv(port);
    (ready, result, remaining)
}

/** The sending end of a pipe. It can be used to send exactly one
message.

*/
pub type SendPacket<T> = SendPacketBuffered<T, Packet<T>>;

#[doc(hidden)]
pub fn SendPacket<T>(p: *Packet<T>) -> SendPacket<T> {
    SendPacketBuffered(p)
}

pub struct SendPacketBuffered<T, Tbuffer> {
    mut p: Option<*Packet<T>>,
    mut buffer: Option<BufferResource<Tbuffer>>,
}

#[unsafe_destructor]
impl<T:Owned,Tbuffer:Owned> ::ops::Drop for SendPacketBuffered<T,Tbuffer> {
    fn finalize(&self) {
        //if self.p != none {
        //    debug!("drop send %?", option::get(self.p));
        //}
        if self.p != None {
            let mut p = None;
            p <-> self.p;
            sender_terminate(p.unwrap())
        }
        //unsafe { error!("send_drop: %?",
        //                if self.buffer == none {
        //                    "none"
        //                } else { "some" }); }
    }
}

pub fn SendPacketBuffered<T,Tbuffer>(p: *Packet<T>)
    -> SendPacketBuffered<T, Tbuffer> {
        //debug!("take send %?", p);
    SendPacketBuffered {
        p: Some(p),
        buffer: unsafe {
            Some(BufferResource(
                get_buffer(ptr::addr_of(&((*p).header)))))
        }
    }
}

pub impl<T,Tbuffer> SendPacketBuffered<T,Tbuffer> {
    fn unwrap(&self) -> *Packet<T> {
        let mut p = None;
        p <-> self.p;
        p.unwrap()
    }

    fn header(&self) -> *PacketHeader {
        match self.p {
          Some(packet) => unsafe {
            let packet = &*packet;
            let header = ptr::addr_of(&(packet.header));
            //forget(packet);
            header
          },
          None => fail!(~"packet already consumed")
        }
    }

    fn reuse_buffer(&self) -> BufferResource<Tbuffer> {
        //error!("send reuse_buffer");
        let mut tmp = None;
        tmp <-> self.buffer;
        tmp.unwrap()
    }
}

/// Represents the receive end of a pipe. It can receive exactly one
/// message.
pub type RecvPacket<T> = RecvPacketBuffered<T, Packet<T>>;

#[doc(hidden)]
pub fn RecvPacket<T>(p: *Packet<T>) -> RecvPacket<T> {
    RecvPacketBuffered(p)
}
pub struct RecvPacketBuffered<T, Tbuffer> {
    mut p: Option<*Packet<T>>,
    mut buffer: Option<BufferResource<Tbuffer>>,
}

#[unsafe_destructor]
impl<T:Owned,Tbuffer:Owned> ::ops::Drop for RecvPacketBuffered<T,Tbuffer> {
    fn finalize(&self) {
        //if self.p != none {
        //    debug!("drop recv %?", option::get(self.p));
        //}
        if self.p != None {
            let mut p = None;
            p <-> self.p;
            receiver_terminate(p.unwrap())
        }
        //unsafe { error!("recv_drop: %?",
        //                if self.buffer == none {
        //                    "none"
        //                } else { "some" }); }
    }
}

pub impl<T:Owned,Tbuffer:Owned> RecvPacketBuffered<T, Tbuffer> {
    fn unwrap(&self) -> *Packet<T> {
        let mut p = None;
        p <-> self.p;
        p.unwrap()
    }

    fn reuse_buffer(&self) -> BufferResource<Tbuffer> {
        //error!("recv reuse_buffer");
        let mut tmp = None;
        tmp <-> self.buffer;
        tmp.unwrap()
    }
}

impl<T:Owned,Tbuffer:Owned> Selectable for RecvPacketBuffered<T, Tbuffer> {
    fn header(&self) -> *PacketHeader {
        match self.p {
          Some(packet) => unsafe {
            let packet = &*packet;
            let header = ptr::addr_of(&(packet.header));
            //forget(packet);
            header
          },
          None => fail!(~"packet already consumed")
        }
    }
}

pub fn RecvPacketBuffered<T,Tbuffer>(p: *Packet<T>)
    -> RecvPacketBuffered<T,Tbuffer> {
    //debug!("take recv %?", p);
    RecvPacketBuffered {
        p: Some(p),
        buffer: unsafe {
            Some(BufferResource(
                get_buffer(ptr::addr_of(&((*p).header)))))
        }
    }
}

#[doc(hidden)]
pub fn entangle<T>() -> (SendPacket<T>, RecvPacket<T>) {
    let p = packet();
    (SendPacket(p), RecvPacket(p))
}

pub mod rt {
    use option::{None, Option, Some};

    // These are used to hide the option constructors from the
    // compiler because their names are changing
    pub fn make_some<T>(val: T) -> Option<T> { Some(val) }
    pub fn make_none<T>() -> Option<T> { None }
}

#[cfg(test)]
pub mod test {
    use either::Right;
    use comm::{Chan, Port, oneshot, recv_one, stream, Select2,
               GenericChan, Peekable};

    #[test]
    pub fn test_select2() {
        let (p1, c1) = stream();
        let (p2, c2) = stream();

        c1.send(~"abc");

        match (p1, p2).select() {
          Right(_) => fail!(),
          _ => ()
        }

        c2.send(123);
    }

    #[test]
    pub fn test_oneshot() {
        let (c, p) = oneshot::init();

        oneshot::client::send(c, ());

        recv_one(p)
    }

    #[test]
    fn test_peek_terminated() {
        let (port, chan): (Port<int>, Chan<int>) = stream();

        {
            // Destroy the channel
            let _chan = chan;
        }

        assert!(!port.peek());
    }
}
