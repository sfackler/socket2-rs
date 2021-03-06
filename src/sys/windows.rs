// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cmp;
use std::fmt;
use std::io::{Read, Write};
use std::io;
use std::mem;
use std::net::Shutdown;
use std::net::{self, Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, SocketAddr};
use std::os::windows::prelude::*;
use std::ptr;
use std::sync::{Once, ONCE_INIT};
use std::time::Duration;

use kernel32;
use winapi::*;
use ws2_32;

const HANDLE_FLAG_INHERIT: DWORD = 0x00000001;
const MSG_PEEK: c_int = 0x2;
const SD_BOTH: c_int = 2;
const SD_RECEIVE: c_int = 0;
const SD_SEND: c_int = 1;
const SIO_KEEPALIVE_VALS: DWORD = 0x98000004;
const WSA_FLAG_OVERLAPPED: DWORD = 0x01;

#[repr(C)]
struct tcp_keepalive {
    onoff: c_ulong,
    keepalivetime: c_ulong,
    keepaliveinterval: c_ulong,
}

fn init() {
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        // Initialize winsock through the standard library by just creating a
        // dummy socket. Whether this is successful or not we drop the result as
        // libstd will be sure to have initialized winsock.
        let _ = net::UdpSocket::bind("127.0.0.1:34254");
    });
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { ws2_32::WSAGetLastError() })
}

pub struct Socket {
    socket: SOCKET,
}

impl Socket {
    pub fn new(family: c_int, ty: c_int, protocol: c_int) -> io::Result<Socket> {
        init();
        unsafe {
            let socket = match ws2_32::WSASocketW(family,
                                                  ty,
                                                  protocol,
                                                  ptr::null_mut(),
                                                  0,
                                                  WSA_FLAG_OVERLAPPED) {
                INVALID_SOCKET => return Err(last_error()),
                socket => socket,
            };
            let socket = Socket::from_raw_socket(socket);
            socket.set_no_inherit()?;
            Ok(socket)
        }
    }

    pub fn bind(&self, addr: &SocketAddr) -> io::Result<()> {
        let (addr, len) = addr2raw(addr);
        unsafe {
            if ws2_32::bind(self.socket, addr, len) == 0 {
                Ok(())
            } else {
                Err(last_error())
            }
        }
    }

    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        unsafe {
            if ws2_32::listen(self.socket, backlog) == 0 {
                Ok(())
            } else {
                Err(last_error())
            }
        }
    }

    pub fn connect(&self, addr: &SocketAddr) -> io::Result<()> {
        let (addr, len) = addr2raw(addr);
        unsafe {
            if ws2_32::connect(self.socket, addr, len) == 0 {
                Ok(())
            } else {
                Err(last_error())
            }
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        unsafe {
            let mut storage: SOCKADDR_STORAGE = mem::zeroed();
            let mut len = mem::size_of_val(&storage) as c_int;
            if ws2_32::getsockname(self.socket,
                                   &mut storage as *mut _ as *mut _,
                                   &mut len) != 0 {
                return Err(last_error())
            }
            raw2addr(&storage, len)
        }
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        unsafe {
            let mut storage: SOCKADDR_STORAGE = mem::zeroed();
            let mut len = mem::size_of_val(&storage) as c_int;
            if ws2_32::getpeername(self.socket,
                                   &mut storage as *mut _ as *mut _,
                                   &mut len) != 0 {
                return Err(last_error())
            }
            raw2addr(&storage, len)
        }
    }

    pub fn try_clone(&self) -> io::Result<Socket> {
        unsafe {
            let mut info: WSAPROTOCOL_INFOW = mem::zeroed();
            let r = ws2_32::WSADuplicateSocketW(self.socket,
                                                kernel32::GetCurrentProcessId(),
                                                &mut info);
            if r != 0 {
                return Err(io::Error::last_os_error())
            }
            let socket = ws2_32::WSASocketW(info.iAddressFamily,
                                            info.iSocketType,
                                            info.iProtocol,
                                            &mut info,
                                            0,
                                            WSA_FLAG_OVERLAPPED);
            let socket = match socket {
                INVALID_SOCKET => return Err(last_error()),
                n => Socket::from_raw_socket(n),
            };
            socket.set_no_inherit()?;
            Ok(socket)
        }
    }

    pub fn accept(&self) -> io::Result<(Socket, SocketAddr)> {
        unsafe {
            let mut storage: SOCKADDR_STORAGE = mem::zeroed();
            let mut len = mem::size_of_val(&storage) as c_int;
            let socket = {
                ws2_32::accept(self.socket,
                               &mut storage as *mut _ as *mut _,
                               &mut len)
            };
            let socket = match socket {
                INVALID_SOCKET => return Err(last_error()),
                socket => Socket::from_raw_socket(socket),
            };
            socket.set_no_inherit()?;
            let addr = raw2addr(&storage, len)?;
            Ok((socket, addr))
        }
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_ERROR)?;
            if raw == 0 {
                Ok(None)
            } else {
                Ok(Some(io::Error::from_raw_os_error(raw as i32)))
            }
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        unsafe {
            let mut nonblocking = nonblocking as c_ulong;
            let r = ws2_32::ioctlsocket(self.socket,
                                        FIONBIO as c_int,
                                        &mut nonblocking);
            if r == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {

        let how = match how {
            Shutdown::Write => SD_SEND,
            Shutdown::Read => SD_RECEIVE,
            Shutdown::Both => SD_BOTH,
        };
        if unsafe { ws2_32::shutdown(self.socket, how) == 0 } {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let n = {
                ws2_32::recv(self.socket,
                             buf.as_mut_ptr() as *mut c_char,
                             clamp(buf.len()),
                             0)
            };
            match n {
                SOCKET_ERROR if ws2_32::WSAGetLastError() == WSAESHUTDOWN as i32 => Ok(0),
                SOCKET_ERROR => Err(last_error()),
                n => Ok(n as usize)
            }
        }
    }

    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let n = {
                ws2_32::recv(self.socket,
                             buf.as_mut_ptr() as *mut c_char,
                             clamp(buf.len()),
                             MSG_PEEK)
            };
            match n {
                SOCKET_ERROR if ws2_32::WSAGetLastError() == WSAESHUTDOWN as i32 => Ok(0),
                SOCKET_ERROR => Err(last_error()),
                n => Ok(n as usize)
            }
        }
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recvfrom(buf, 0)
    }

    pub fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.recvfrom(buf, MSG_PEEK)
    }

    fn recvfrom(&self, buf: &mut [u8], flags: c_int)
                -> io::Result<(usize, SocketAddr)> {
        unsafe {
            let mut storage: SOCKADDR_STORAGE = mem::zeroed();
            let mut addrlen = mem::size_of_val(&storage) as c_int;

            let n = {
                ws2_32::recvfrom(self.socket,
                                 buf.as_mut_ptr() as *mut c_char,
                                 clamp(buf.len()),
                                 flags,
                                 &mut storage as *mut _ as *mut _,
                                 &mut addrlen)
            };
            let n = match n {
                SOCKET_ERROR if ws2_32::WSAGetLastError() == WSAESHUTDOWN as i32 => 0,
                SOCKET_ERROR => return Err(last_error()),
                n => n as usize,
            };
            Ok((n, raw2addr(&storage, addrlen)?))
        }
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let n = {
                ws2_32::send(self.socket,
                             buf.as_ptr() as *const c_char,
                             clamp(buf.len()),
                             0)
            };
            if n == SOCKET_ERROR {
                Err(last_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        unsafe {
            let (addr, len) = addr2raw(addr);
            let n = {
                ws2_32::sendto(self.socket,
                               buf.as_ptr() as *const c_char,
                               clamp(buf.len()),
                               0,
                               addr,
                               len)
            };
            if n == SOCKET_ERROR {
                Err(last_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    // ================================================

    pub fn ttl(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IP, IP_TTL)?;
            Ok(raw as u32)
        }
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        unsafe {
            self.setsockopt(IPPROTO_IP, IP_TTL, ttl as c_int)
        }
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6.0 as c_int,
                                             IPV6_V6ONLY)?;
            Ok(raw != 0)
        }
    }

    pub fn set_only_v6(&self, only_v6: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(IPPROTO_IPV6.0 as c_int,
                            IPV6_V6ONLY,
                            only_v6 as c_int)
        }
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        unsafe {
            Ok(ms2dur(self.getsockopt(SOL_SOCKET, SO_RCVTIMEO)?))
        }
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe {
            self.setsockopt(SOL_SOCKET, SO_RCVTIMEO, dur2ms(dur)?)
        }
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        unsafe {
            Ok(ms2dur(self.getsockopt(SOL_SOCKET, SO_SNDTIMEO)?))
        }
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe {
            self.setsockopt(SOL_SOCKET, SO_SNDTIMEO, dur2ms(dur)?)
        }
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_TCP.0 as c_int,
                                             TCP_NODELAY)?;
            Ok(raw != 0)
        }
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(IPPROTO_TCP.0 as c_int,
                            TCP_NODELAY,
                            nodelay as c_int)
        }
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_BROADCAST)?;
            Ok(raw != 0)
        }
    }

    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(SOL_SOCKET, SO_BROADCAST, broadcast as c_int)
        }
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IP, IP_MULTICAST_LOOP)?;
            Ok(raw != 0)
        }
    }

    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(IPPROTO_IP,
                            IP_MULTICAST_LOOP,
                            multicast_loop_v4 as c_int)
        }
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IP, IP_MULTICAST_TTL)?;
            Ok(raw as u32)
        }
    }

    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> io::Result<()> {
        unsafe {
            self.setsockopt(IPPROTO_IP,
                            IP_MULTICAST_TTL,
                            multicast_ttl_v4 as c_int)
        }
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(IPPROTO_IPV6.0 as c_int,
                                             IPV6_MULTICAST_LOOP)?;
            Ok(raw != 0)
        }
    }

    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(IPPROTO_IPV6.0 as c_int,
                            IPV6_MULTICAST_LOOP,
                            multicast_loop_v6 as c_int)
        }
    }

    pub fn join_multicast_v4(&self,
                             multiaddr: &Ipv4Addr,
                             interface: &Ipv4Addr) -> io::Result<()> {
        let multiaddr = to_s_addr(multiaddr);
        let interface = to_s_addr(interface);
        let mreq = ip_mreq {
            imr_multiaddr: in_addr { S_un: multiaddr },
            imr_interface: in_addr { S_un: interface },
        };
        unsafe {
            self.setsockopt(IPPROTO_IP, IP_ADD_MEMBERSHIP, mreq)
        }
    }

    pub fn join_multicast_v6(&self,
                             multiaddr: &Ipv6Addr,
                             interface: u32) -> io::Result<()> {
        let multiaddr = to_in6_addr(multiaddr);
        let mreq = ipv6_mreq {
            ipv6mr_multiaddr: multiaddr,
            ipv6mr_interface: interface,
        };
        unsafe {
            self.setsockopt(IPPROTO_IP, IPV6_ADD_MEMBERSHIP, mreq)
        }
    }

    pub fn leave_multicast_v4(&self,
                              multiaddr: &Ipv4Addr,
                              interface: &Ipv4Addr) -> io::Result<()> {
        let multiaddr = to_s_addr(multiaddr);
        let interface = to_s_addr(interface);
        let mreq = ip_mreq {
            imr_multiaddr: in_addr { S_un: multiaddr },
            imr_interface: in_addr { S_un: interface },
        };
        unsafe {
            self.setsockopt(IPPROTO_IP, IP_DROP_MEMBERSHIP, mreq)
        }
    }

    pub fn leave_multicast_v6(&self,
                              multiaddr: &Ipv6Addr,
                              interface: u32) -> io::Result<()> {
        let multiaddr = to_in6_addr(multiaddr);
        let mreq = ipv6_mreq {
            ipv6mr_multiaddr: multiaddr,
            ipv6mr_interface: interface,
        };
        unsafe {
            self.setsockopt(IPPROTO_IP, IPV6_DROP_MEMBERSHIP, mreq)
        }
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsafe {
            Ok(linger2dur(self.getsockopt(SOL_SOCKET, SO_LINGER)?))
        }
    }

    pub fn set_linger(&self, dur: Option<Duration>) -> io::Result<()> {
        unsafe {
            self.setsockopt(SOL_SOCKET, SO_LINGER, dur2linger(dur))
        }
    }

    pub fn set_reuse_address(&self, reuse: bool) -> io::Result<()> {
        unsafe {
            self.setsockopt(SOL_SOCKET, SO_REUSEADDR, reuse as c_int)
        }
    }

    pub fn reuse_address(&self) -> io::Result<bool> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_REUSEADDR)?;
            Ok(raw != 0)
        }
    }

    pub fn recv_buffer_size(&self) -> io::Result<usize> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_RCVBUF)?;
            Ok(raw as usize)
        }
    }

    pub fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
        unsafe {
            // TODO: casting usize to a c_int should be a checked cast
            self.setsockopt(SOL_SOCKET, SO_RCVBUF, size as c_int)
        }
    }

    pub fn send_buffer_size(&self) -> io::Result<usize> {
        unsafe {
            let raw: c_int = self.getsockopt(SOL_SOCKET, SO_SNDBUF)?;
            Ok(raw as usize)
        }
    }

    pub fn set_send_buffer_size(&self, size: usize) -> io::Result<()> {
        unsafe {
            // TODO: casting usize to a c_int should be a checked cast
            self.setsockopt(SOL_SOCKET, SO_SNDBUF, size as c_int)
        }
    }

    pub fn keepalive(&self) -> io::Result<Option<Duration>> {
        let mut ka = tcp_keepalive {
            onoff: 0,
            keepalivetime: 0,
            keepaliveinterval: 0,
        };
        let n = unsafe {
            ws2_32::WSAIoctl(self.socket,
                             SIO_KEEPALIVE_VALS,
                             0 as *mut _,
                             0,
                             &mut ka as *mut _ as *mut _,
                             mem::size_of_val(&ka) as DWORD,
                             0 as *mut _,
                             0 as *mut _,
                             None)
        };
        if n == 0 {
            Ok(if ka.onoff == 0 {
                None
            } else if ka.keepaliveinterval == 0 {
                None
            } else {
                let seconds = ka.keepaliveinterval / 1000;
                let nanos = (ka.keepaliveinterval % 1000) * 1_000_000;
                Some(Duration::new(seconds as u64, nanos as u32))
            })
        } else {
            Err(last_error())
        }
    }

    pub fn set_keepalive(&self, keepalive: Option<Duration>) -> io::Result<()> {
        let ms = dur2ms(keepalive)?;
        // TODO: checked casts here
        let ka = tcp_keepalive {
            onoff: keepalive.is_some() as c_ulong,
            keepalivetime: ms as c_ulong,
            keepaliveinterval: ms as c_ulong,
        };
        let n = unsafe {
            ws2_32::WSAIoctl(self.socket,
                             SIO_KEEPALIVE_VALS,
                             &ka as *const _ as *mut _,
                             mem::size_of_val(&ka) as DWORD,
                             0 as *mut _,
                             0,
                             0 as *mut _,
                             0 as *mut _,
                             None)
        };
        if n == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    unsafe fn setsockopt<T>(&self,
                            opt: c_int,
                            val: c_int,
                            payload: T) -> io::Result<()>
        where T: Copy,
    {
        let payload = &payload as *const T as *const c_char;
        if ws2_32::setsockopt(self.socket,
                              opt,
                              val,
                              payload,
                              mem::size_of::<T>() as c_int) == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    unsafe fn getsockopt<T: Copy>(&self, opt: c_int, val: c_int) -> io::Result<T> {
        let mut slot: T = mem::zeroed();
        let mut len = mem::size_of::<T>() as c_int;
        if ws2_32::getsockopt(self.socket,
                              opt,
                              val,
                              &mut slot as *mut _ as *mut _,
                              &mut len) == 0 {
            assert_eq!(len as usize, mem::size_of::<T>());
            Ok(slot)
        } else {
            Err(last_error())
        }
    }

    fn set_no_inherit(&self) -> io::Result<()> {
        unsafe {
            let r = kernel32::SetHandleInformation(self.socket as HANDLE,
                                                   HANDLE_FLAG_INHERIT,
                                                   0);
            if r == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        <&Socket>::read(&mut &*self, buf)
    }
}

impl<'a> Read for &'a Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        <&Socket>::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        <&Socket>::flush(&mut &*self)
    }
}

impl<'a> Write for &'a Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut f = f.debug_struct("Socket");
        f.field("socket", &self.socket);
        if let Ok(addr) = self.local_addr() {
            f.field("local_addr", &addr);
        }
        if let Ok(addr) = self.peer_addr() {
            f.field("peer_addr", &addr);
        }
        f.finish()
    }
}

impl AsRawSocket for Socket {
    fn as_raw_socket(&self) -> SOCKET {
        self.socket
    }
}

impl IntoRawSocket for Socket {
    fn into_raw_socket(self) -> SOCKET {
        let socket = self.socket;
        mem::forget(self);
        return socket
    }
}

impl FromRawSocket for Socket {
    unsafe fn from_raw_socket(socket: SOCKET) -> Socket {
        Socket { socket: socket }
    }
}

impl AsRawSocket for ::Socket {
    fn as_raw_socket(&self) -> SOCKET {
        self.inner.as_raw_socket()
    }
}

impl IntoRawSocket for ::Socket {
    fn into_raw_socket(self) -> SOCKET {
        self.inner.into_raw_socket()
    }
}

impl FromRawSocket for ::Socket {
    unsafe fn from_raw_socket(socket: SOCKET) -> ::Socket {
        ::Socket { inner: Socket::from_raw_socket(socket) }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe {
            let _ = ws2_32::closesocket(self.socket);
        }
    }
}

impl From<Socket> for net::TcpStream {
    fn from(socket: Socket) -> net::TcpStream {
        unsafe { net::TcpStream::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<Socket> for net::TcpListener {
    fn from(socket: Socket) -> net::TcpListener {
        unsafe { net::TcpListener::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<Socket> for net::UdpSocket {
    fn from(socket: Socket) -> net::UdpSocket {
        unsafe { net::UdpSocket::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<net::TcpStream> for Socket {
    fn from(socket: net::TcpStream) -> Socket {
        unsafe { Socket::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<net::TcpListener> for Socket {
    fn from(socket: net::TcpListener) -> Socket {
        unsafe { Socket::from_raw_socket(socket.into_raw_socket()) }
    }
}

impl From<net::UdpSocket> for Socket {
    fn from(socket: net::UdpSocket) -> Socket {
        unsafe { Socket::from_raw_socket(socket.into_raw_socket()) }
    }
}

fn clamp(input: usize) -> c_int {
    cmp::min(input, <c_int>::max_value() as usize) as c_int
}

fn addr2raw(addr: &SocketAddr) -> (*const SOCKADDR, c_int) {
    match *addr {
        SocketAddr::V4(ref a) => {
            (a as *const _ as *const _, mem::size_of_val(a) as c_int)
        }
        SocketAddr::V6(ref a) => {
            (a as *const _ as *const _, mem::size_of_val(a) as c_int)
        }
    }
}

fn raw2addr(storage: &SOCKADDR_STORAGE, len: c_int) -> io::Result<SocketAddr> {
    match storage.ss_family as c_int {
        AF_INET => {
            unsafe {
                assert!(len as usize >= mem::size_of::<SOCKADDR_IN>());
                let sa = storage as *const _ as *const SOCKADDR_IN;
                let bits = ::ntoh((*sa).sin_addr.S_un);
                let ip = Ipv4Addr::new((bits >> 24) as u8,
                                       (bits >> 16) as u8,
                                       (bits >> 8) as u8,
                                       bits as u8);
                Ok(SocketAddr::V4(SocketAddrV4::new(ip, ::ntoh((*sa).sin_port))))
            }
        }
        AF_INET6 => {
            unsafe {
                assert!(len as usize >= mem::size_of::<sockaddr_in6>());

                let sa = storage as *const _ as *const sockaddr_in6;
                let arr = (*sa).sin6_addr.s6_addr;

                let ip = Ipv6Addr::new(
                    (arr[0] as u16) << 8 | (arr[1] as u16),
                    (arr[2] as u16) << 8 | (arr[3] as u16),
                    (arr[4] as u16) << 8 | (arr[5] as u16),
                    (arr[6] as u16) << 8 | (arr[7] as u16),
                    (arr[8] as u16) << 8 | (arr[9] as u16),
                    (arr[10] as u16) << 8 | (arr[11] as u16),
                    (arr[12] as u16) << 8 | (arr[13] as u16),
                    (arr[14] as u16) << 8 | (arr[15] as u16),
                );

                Ok(SocketAddr::V6(SocketAddrV6::new(ip,
                                                    ::ntoh((*sa).sin6_port),
                                                    (*sa).sin6_flowinfo,
                                                    (*sa).sin6_scope_id)))
            }
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid argument")),
    }
}

fn dur2ms(dur: Option<Duration>) -> io::Result<DWORD> {
    match dur {
        Some(dur) => {
            // Note that a duration is a (u64, u32) (seconds, nanoseconds)
            // pair, and the timeouts in windows APIs are typically u32
            // milliseconds. To translate, we have two pieces to take care of:
            //
            // * Nanosecond precision is rounded up
            // * Greater than u32::MAX milliseconds (50 days) is rounded up to
            //   INFINITE (never time out).
            let ms = dur.as_secs().checked_mul(1000).and_then(|ms| {
                ms.checked_add((dur.subsec_nanos() as u64) / 1_000_000)
            }).and_then(|ms| {
                ms.checked_add(if dur.subsec_nanos() % 1_000_000 > 0 {1} else {0})
            }).map(|ms| {
                if ms > <DWORD>::max_value() as u64 {
                    INFINITE
                } else {
                    ms as DWORD
                }
            }).unwrap_or(INFINITE);
            if ms == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                          "cannot set a 0 duration timeout"));
            }
            Ok(ms)
        }
        None => Ok(0),
    }
}

fn ms2dur(raw: DWORD) -> Option<Duration> {
    if raw == 0 {
        None
    } else {
        let secs = raw / 1000;
        let nsec = (raw % 1000) * 1000000;
        Some(Duration::new(secs as u64, nsec as u32))
    }
}

fn to_s_addr(addr: &Ipv4Addr) -> ULONG {
    let octets = addr.octets();
    ::hton(((octets[0] as ULONG) << 24) |
           ((octets[1] as ULONG) << 16) |
           ((octets[2] as ULONG) <<  8) |
           ((octets[3] as ULONG) <<  0))
}

fn to_in6_addr(addr: &Ipv6Addr) -> in6_addr {
    let mut ret: in6_addr = unsafe { mem::zeroed() };
    ret.s6_addr = addr.octets();
    return ret
}

fn linger2dur(linger_opt: linger) -> Option<Duration> {
    if linger_opt.l_onoff == 0 {
        None
    } else {
        Some(Duration::from_secs(linger_opt.l_linger as u64))
    }
}

fn dur2linger(dur: Option<Duration>) -> linger {
    match dur {
        Some(d) => {
            linger {
                l_onoff: 1,
                l_linger: d.as_secs() as u16,
            }
        }
        None => linger { l_onoff: 0, l_linger: 0 },
    }
}
