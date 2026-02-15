use crate::net_stack::NETWORK_STACK;
use smoltcp::socket::tcp;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use alloc::vec::Vec;

pub struct TcpReadFuture<'a> {
    pub handle: smoltcp::iface::SocketHandle,
    pub buffer: &'a mut [u8],
}

impl<'a> Future for TcpReadFuture<'a> {
    type Output = Result<usize, ()>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut stack = NETWORK_STACK.lock();
        if let Some(ref mut stack_inner) = *stack {
            let socket = stack_inner.sockets.get_mut::<tcp::Socket>(self.handle);
            if socket.can_recv() {
                match socket.recv_slice(&mut self.buffer) {
                    Ok(n) if n > 0 => Poll::Ready(Ok(n)),
                    Ok(_) => Poll::Pending, // Non-blocking, keep polling
                    Err(_) => Poll::Ready(Err(())),
                }
            } else if !socket.is_active() || socket.state() == tcp::State::Closed {
                Poll::Ready(Err(()))
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(Err(()))
        }
    }
}

pub struct TcpWriteFuture<'a> {
    pub handle: smoltcp::iface::SocketHandle,
    pub data: &'a [u8],
}

impl<'a> Future for TcpWriteFuture<'a> {
    type Output = Result<usize, ()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut stack = NETWORK_STACK.lock();
        if let Some(ref mut stack_inner) = *stack {
            let socket = stack_inner.sockets.get_mut::<tcp::Socket>(self.handle);
            if socket.can_send() {
                match socket.send_slice(self.data) {
                    Ok(n) if n > 0 => Poll::Ready(Ok(n)),
                    Ok(_) => Poll::Pending,
                    Err(_) => Poll::Ready(Err(())),
                }
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(Err(()))
        }
    }
}

/// Helper for length-prefixed framing (simple P2P transport)
pub async fn send_framed(handle: smoltcp::iface::SocketHandle, data: &[u8]) -> Result<(), ()> {
    // 1. Send Length (u32 little endian)
    let len = data.len() as u32;
    let len_bytes = len.to_le_bytes();
    
    let mut sent = 0;
    while sent < 4 {
        match (TcpWriteFuture { handle, data: &len_bytes[sent..] }).await {
            Ok(n) => sent += n,
            Err(_) => return Err(()),
        }
    }
    
    // 2. Send Data
    let mut sent = 0;
    while sent < data.len() {
        match (TcpWriteFuture { handle, data: &data[sent..] }).await {
            Ok(n) => sent += n,
            Err(_) => return Err(()),
        }
    }
    
    Ok(())
}

pub async fn recv_framed(handle: smoltcp::iface::SocketHandle) -> Result<Vec<u8>, ()> {
    // 1. Read Length
    let mut len_bytes = [0u8; 4];
    let mut read = 0;
    while read < 4 {
        match (TcpReadFuture { handle, buffer: &mut len_bytes[read..] }).await {
            Ok(n) => read += n,
            Err(_) => return Err(()),
        }
    }
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > 1024 * 1024 { return Err(()); } // Sanity check 1MB
    
    // 2. Read Data
    let mut buffer = Vec::with_capacity(len);
    buffer.resize(len, 0);
    let mut read = 0;
    while read < len {
        match (TcpReadFuture { handle, buffer: &mut buffer[read..] }).await {
            Ok(n) => read += n,
            Err(_) => return Err(()),
        }
    }
    
    Ok(buffer)
}
