use core::cell::UnsafeCell;
use core::mem;
use core::task::Poll;

use futures::future::poll_fn;
use smoltcp::iface::{Interface, SocketHandle};
use smoltcp::socket::udp::{self, PacketMetadata};
use smoltcp::wire::{IpEndpoint, IpListenEndpoint};

use super::stack::SocketStack;
use crate::{Device, Stack};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BindError {
    /// The socket was already open.
    InvalidState,
    /// No route to host.
    NoRoute,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// No route to host.
    NoRoute,
}

pub struct UdpSocket<'a> {
    stack: &'a UnsafeCell<SocketStack>,
    handle: SocketHandle,
}

impl<'a> UdpSocket<'a> {
    pub fn new<D: Device>(
        stack: &'a Stack<D>,
        rx_meta: &'a mut [PacketMetadata],
        rx_buffer: &'a mut [u8],
        tx_meta: &'a mut [PacketMetadata],
        tx_buffer: &'a mut [u8],
    ) -> Self {
        // safety: not accessed reentrantly.
        let s = unsafe { &mut *stack.socket.get() };

        let rx_meta: &'static mut [PacketMetadata] = unsafe { mem::transmute(rx_meta) };
        let rx_buffer: &'static mut [u8] = unsafe { mem::transmute(rx_buffer) };
        let tx_meta: &'static mut [PacketMetadata] = unsafe { mem::transmute(tx_meta) };
        let tx_buffer: &'static mut [u8] = unsafe { mem::transmute(tx_buffer) };
        let handle = s.sockets.add(udp::Socket::new(
            udp::PacketBuffer::new(rx_meta, rx_buffer),
            udp::PacketBuffer::new(tx_meta, tx_buffer),
        ));

        Self {
            stack: &stack.socket,
            handle,
        }
    }

    pub fn bind<T>(&mut self, endpoint: T) -> Result<(), BindError>
    where
        T: Into<IpListenEndpoint>,
    {
        let mut endpoint = endpoint.into();

        // safety: not accessed reentrantly.
        if endpoint.port == 0 {
            // If user didn't specify port allocate a dynamic port.
            endpoint.port = unsafe { &mut *self.stack.get() }.get_local_port();
        }

        // safety: not accessed reentrantly.
        match unsafe { self.with_mut(|s, _| s.bind(endpoint)) } {
            Ok(()) => Ok(()),
            Err(udp::BindError::InvalidState) => Err(BindError::InvalidState),
            Err(udp::BindError::Unaddressable) => Err(BindError::NoRoute),
        }
    }

    /// SAFETY: must not call reentrantly.
    unsafe fn with<R>(&self, f: impl FnOnce(&udp::Socket, &Interface) -> R) -> R {
        let s = &*self.stack.get();
        let socket = s.sockets.get::<udp::Socket>(self.handle);
        f(socket, &s.iface)
    }

    /// SAFETY: must not call reentrantly.
    unsafe fn with_mut<R>(&self, f: impl FnOnce(&mut udp::Socket, &mut Interface) -> R) -> R {
        let s = &mut *self.stack.get();
        let socket = s.sockets.get_mut::<udp::Socket>(self.handle);
        let res = f(socket, &mut s.iface);
        s.waker.wake();
        res
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, IpEndpoint), Error> {
        poll_fn(move |cx| unsafe {
            self.with_mut(|s, _| match s.recv_slice(buf) {
                Ok(x) => Poll::Ready(Ok(x)),
                // No data ready
                Err(udp::RecvError::Exhausted) => {
                    //s.register_recv_waker(cx.waker());
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
        })
        .await
    }

    pub async fn send_to<T>(&self, buf: &[u8], remote_endpoint: T) -> Result<(), Error>
    where
        T: Into<IpEndpoint>,
    {
        let remote_endpoint = remote_endpoint.into();
        poll_fn(move |cx| unsafe {
            self.with_mut(|s, _| match s.send_slice(buf, remote_endpoint) {
                // Entire datagram has been sent
                Ok(()) => Poll::Ready(Ok(())),
                Err(udp::SendError::BufferFull) => {
                    s.register_send_waker(cx.waker());
                    Poll::Pending
                }
                Err(udp::SendError::Unaddressable) => Poll::Ready(Err(Error::NoRoute)),
            })
        })
        .await
    }

    pub fn endpoint(&self) -> IpListenEndpoint {
        unsafe { self.with(|s, _| s.endpoint()) }
    }

    pub fn is_open(&self) -> bool {
        unsafe { self.with(|s, _| s.is_open()) }
    }

    pub fn close(&mut self) {
        unsafe { self.with_mut(|s, _| s.close()) }
    }

    pub fn may_send(&self) -> bool {
        unsafe { self.with(|s, _| s.can_send()) }
    }

    pub fn may_recv(&self) -> bool {
        unsafe { self.with(|s, _| s.can_recv()) }
    }
}

impl Drop for UdpSocket<'_> {
    fn drop(&mut self) {
        // safety: not accessed reentrantly.
        let s = unsafe { &mut *self.stack.get() };
        s.sockets.remove(self.handle);
    }
}
