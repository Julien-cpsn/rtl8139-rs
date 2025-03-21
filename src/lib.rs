//! RTL8140 Network driver tested inside qemu.
//! Based on:
//! * http://www.jbox.dk/sanos/source/sys/dev/rtl8139.c.html
//! * https://www.cs.usfca.edu/~cruse/cs326f04/RTL8139_ProgrammersGuide.pdf
//! * https://www.cs.usfca.edu/~cruse/cs326f04/RTL8139D_DataSheet.pdf
#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::convert::TryInto;

#[cfg(feature = "async")]
use core::marker::PhantomData;
#[cfg(feature = "async")]
use core::pin::Pin;
#[cfg(feature = "async")]
use core::task::Context;
#[cfg(feature = "async")]
use core::task::Poll;
#[cfg(feature = "async")]
use futures_util::sink::Sink;
#[cfg(feature = "async")]
use futures_util::stream::Stream;
#[cfg(feature = "async")]
use futures_util::task::AtomicWaker;

use crossbeam_queue::SegQueue;
use log::debug;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::PhysAddr;
use x86_64::VirtAddr;

// Here we define all the RX Buffer lengths that we are gonna use
// NOTE: Make sure you do all your logic and math with RX_BUF_LEN as technically
//       the padding bytes and the Wrap bytes shouldnt exist, seriously it took me a long long long
//       time to figure out that you should do all the math for calculating the new cursor without
//       also adding the padding length.
const RX_BUF_LEN: usize = 8192;
const RX_BUF_WRAP: usize = 1500; // Extra 1500 bytes with a WRAP mask for Rx because i really cant be fucked
const RX_BUF_PAD: usize = 16;
const RX_BUF_LEN_WRAPPED: usize = RX_BUF_LEN + RX_BUF_PAD + RX_BUF_WRAP;

// Bit flags specific to the RCR
const APM: u32 = 0b10;
const AB: u32 = 0b1000;
const WRAP: u32 = 0b1000_0000;
const MXDMA_UNLIMITED: u32 = 0b111_0000_0000;
const RXFTH_NONE: u32 = 0b1110_0000_0000_0000;

// Bit flags specific to the CR
#[allow(dead_code)]
const RX_BUF_EMPTY: u8 = 0b1;
const TX_ENABLE: u8 = 0b100;
const RX_ENABLE: u8 = 0b1000;
const RST: u8 = 0b10000;

// Bit flags for IMR
const RX_OK: u16 = 0b1;
const RX_ERR: u16 = 0b10;
const TX_OK: u16 = 0b100;
const TX_ERR: u16 = 0b1000;
const RDU: u16 = 0b10000;
const TDU: u16 = 0b1000_0000;
const SYS_ERR: u16 = 0b1000_0000_0000_0000;

#[cfg(feature = "async")]
static WAKER: AtomicWaker = AtomicWaker::new();

/// This is our inner struct that holds all of our ports that we will use to talk with the nic, as
/// well as our rx and tx buffers.
#[derive(Debug)]
pub struct Rtl8139State {
    pub config_1: Port<u32>,
    pub cmd_reg: Port<u8>,
    pub rbstart: Port<u32>,
    pub imr: Port<u16>,
    pub rcr: Port<u32>,
    #[allow(unused)]
    pub tppoll: Port<u8>,
    pub ack: Port<u16>,
    #[allow(unused)]
    pub cpcr: Port<u16>,
    pub capr: Port<u16>,

    // Registers holding our MAC bytes
    pub idr: [Port<u8>; 6],

    pub tx_dat: [Port<u32>; 4],
    pub tx_cmd: [Port<u32>; 4],
    pub tx_cursor: usize,

    pub buffer: Box<[u8; RX_BUF_LEN_WRAPPED]>,
    pub rx_cursor: usize,

    pub virt_to_phys: fn(VirtAddr) -> PhysAddr,
}

#[derive(Debug)]
pub struct RTL8139 {
    pub mac: [u8; 6],
    pub state: Rtl8139State,
    pub frames: SegQueue<Vec<u8>>,
    #[cfg(feature = "async")]
    pub rx: RxSink,
    #[cfg(feature = "async")]
    pub tx: TxSink
}

impl RTL8139 {
    /// Returns the PCI manifest for devices compatible with this driver.
    pub fn manifest() -> (u16, u16, u16, u16) {
        (0x2, 0x00, 0x10ec, 0x8139)
    }

    /// Function preloads the driver.
    /// # Arguments
    /// * `base` - base port for the device
    /// * `virt_to_phys` - function or closure that converts a virtaddr into a physaddr (used for
    /// DMA).
    /// # Safety
    /// The caller of this function must pass a valid port base for a valid PCI device. Furthermore
    /// the caller must ensure that mastering and interrupts for the PCI device are enabled.
    pub unsafe fn preload_unchecked(base: u16, virt_to_phys: fn(VirtAddr) -> PhysAddr) -> Self {
        debug!("Preloading RTL8139");
        let inner = Rtl8139State {
            config_1: Port::new(base + 0x52),
            cmd_reg: Port::new(base + 0x37),
            rbstart: Port::new(base + 0x30),
            imr: Port::new(base + 0x3c),
            rcr: Port::new(base + 0x44),
            tppoll: Port::new(base + 0xd9),
            ack: Port::new(base + 0x3e),
            cpcr: Port::new(base + 0xe0),
            capr: Port::new(base + 0x38),

            idr: [
                Port::new(base + 0x00),
                Port::new(base + 0x01),
                Port::new(base + 0x02),
                Port::new(base + 0x03),
                Port::new(base + 0x04),
                Port::new(base + 0x05),
            ],

            tx_dat: [
                Port::new(base + 0x20),
                Port::new(base + 0x24),
                Port::new(base + 0x28),
                Port::new(base + 0x2c),
            ],
            tx_cmd: [
                Port::new(base + 0x10),
                Port::new(base + 0x14),
                Port::new(base + 0x18),
                Port::new(base + 0x1c),
            ],
            tx_cursor: 0,

            buffer: Box::from([0u8; RX_BUF_LEN_WRAPPED]),
            rx_cursor: 0,
            virt_to_phys,
        };
        debug!("RTL8139 preloaded");

        Self {
            mac: [0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            state: inner,
            frames: SegQueue::new(),
            #[cfg(feature = "async")]
            rx: RxSink::new(),
            #[cfg(feature = "async")]
            tx: TxSink::new(),
        }
    }

    /// Function sets up the device.
    pub fn load(&mut self) -> Result<(), ()> {
        debug!("Loading RTL8139");
        // Turn the device on by writing to config_1 then reset the device to clear all data in the
        // buffers by writing 0x10 to cmd_reg
        unsafe {
            self.state.config_1.write(0x0);
            self.state.cmd_reg.write(RST);
        }

        // Wait while the device resets
        loop {
            if (unsafe { self.state.cmd_reg.read() } & 0x10) == 0 {
                break;
            }
        }

        let raw_mac = self.state
            .idr
            .iter_mut()
            .map(|x| unsafe { x.read() })
            .collect::<Vec<u8>>();

        self.mac = raw_mac
            .as_slice()
            .try_into()
            .expect("rtl8139: failed to read mac");

        let buffer_virt = unsafe { VirtAddr::new_unsafe(self.state.buffer.as_ptr() as u64) };
        let virt_to_phys = &self.state.virt_to_phys;
        let buffer_ptr = virt_to_phys(buffer_virt).as_u64() as u32;

        // Unsafe block specific for pre-launch NIC config
        unsafe {
            // Accept Physically Match packets
            // Accept Broadcast packets
            // Enable Max DMA burst
            // No RX Threshold
            self.state
                .rcr
                .write(APM | AB | MXDMA_UNLIMITED | RXFTH_NONE | WRAP);

            // Enable Tx on the CR register
            self.state.cmd_reg.write(RX_ENABLE | TX_ENABLE);

            // Write the PHYSICAL address of our Rx buffer to the NIC
            self.state.rbstart.write(buffer_ptr);
        }

        // Unsafe block specific to launch of NIC
        unsafe {
            // Enable Tx/Rx
            // NOTE: TX is technically already enabled but fuck it
            self.state.cmd_reg.write(RX_ENABLE | TX_ENABLE);

            // Mask only RxOk, TxOk, and some Err registers for internal book-keeping
            self.state
                .imr
                .write(0xffff | RX_OK | TX_OK | RX_ERR | TX_ERR | SYS_ERR | RDU | TDU);
        }

        debug!("RTL8139 loaded");

        Ok(())
    }

    /// Function that we need to call when we receive an interrupt for this device.
    /// The callee must ensure that the PIC/APIC has received an EOI.
    pub fn on_interrupt(&mut self) {
        // At some point here we will want to also wake the network stack because there are packets
        // available.
        let isr = unsafe { self.state.ack.read() };

        if (isr & RX_OK) != 0 {
            while (unsafe { self.state.cmd_reg.read() } & RX_BUF_EMPTY) == 0 {
                self.frames.push(self.state.rok());

                #[cfg(feature = "async")]
                WAKER.wake();
            }
        }

        unsafe {
            self.state.ack.write(isr);
        }
    }

    pub fn try_recv_sync(&mut self) -> Option<Vec<u8>> {
        self.frames.pop()
    }

    /// FIXME: Disable interrupts for the PCI device only instead of globally.
    pub fn send_sync(&mut self, buffer: &[u8]) {
        interrupts::without_interrupts(||
            unsafe {
                self.state.write(buffer)
            }
        );
    }
}

impl Rtl8139State {
    /// Function called on a ROK interrupt from the RTL8139 NIC, it parses the data written into
    /// the buffer as a ethernet frame and pushes it into our Vec.
    fn rok(&mut self) -> Vec<u8> {
        // A packet frame looks something like this
        // +--------------------------------------------+
        // | |     HEADER     |            |   DATA   | |
        // | +----------------+            +----------+ |
        // | |??|len = 2 bytes| = 4 bytes  |data = len| |
        // | +----------------+            +----------+ |
        // +--------------------------------------------+
        //
        // As per the diagram the packet structure is a 4 byte header where the last 2 bytes is the
        // length of the incoming data.
        // The length given also includes the length of the header itself.
        let buffer = &self.buffer[self.rx_cursor..];
        let length = u16::from_le_bytes(buffer[2..4].try_into().expect("Got wrong len")) as usize;

        // NOTE: The length in the header will never be less than 64, if a packet is received that
        //       has a length less than 64, the NIC will simply pad the packet with 0x00.
        assert!(
            length >= 64,
            "rtl8139: ROK Len is less than 64. THIS IS A BUG."
        );

        // NOTE: We are currently not zeroing out memory after a packet has been parsed and pushed.
        //       Are we sure that if packets with length less than 64 bytes will not contain
        //       remnants of the old packets?
        // If the frame is correctly parsed we push it into the queue, otherwise just skip it

        // NOTE: *** HACK ***
        // basically for some reason when receiving an `ACK` packet from a client as part of a
        // 3-way handshake the tcp packet is padded with extra zeroes at the end. My guessing is
        // that because the packet is less than 64 bytes the packet gets padded.
        let frame = buffer[4..length].to_vec(); // skip 4 bytes length and dont copy 4 bytes crc at the end.

        // Here we set the new index/cursor from where to read new packets, self.rx_cursor should
        // always point to the start of the header.
        // To calculate the new cursor we add the length of the previous frame which SHOULD include
        // the 4 bytes for the header, we also add 3 for 32 bit alignment and then mask the result.
        self.rx_cursor = (self.rx_cursor + length as usize + 4 + 3) & !3;

        if self.rx_cursor > RX_BUF_LEN {
            self.rx_cursor -= RX_BUF_LEN
        }

        unsafe {
            // The NIC is then informed of the new cursor. We remove 0x10 to avoid a overflow as
            // the NIC takes the padding into account I think.
            self.capr.write((self.rx_cursor - 0x10) as u16);
        }

        frame
    }

    /// # Safety
    /// The caller must make sure that interrupts are disabled before calling and are re-enabled
    /// after calling or the program will deadlock.
    pub unsafe fn write(&mut self, data: &[u8]) {
        // NOTE: Are we sure we absolutely need to disable interrupts? maybe we can bypass this
        //       with DMA.
        // We clone the inner PCI device to avoid a deadlock when we re-enable PCI interrupts for
        // this device

        // Disable interrupts for this PCI device to avoid deadlock to do with inner
        let virt_to_phys = &self.virt_to_phys;
        let cursor = self.tx_cursor;
        let data_ptr = virt_to_phys(VirtAddr::new_unsafe(data.as_ptr() as u64)).as_u64();

        self.tx_dat[cursor].write(data_ptr as u32);
        self.tx_cmd[cursor].write((data.len() as u32) & 0xfff);

        loop {
            if (self.tx_cmd[cursor].read() & 0x8000) != 0 {
                break;
            }
        }

        self.tx_cursor = (cursor + 1) % 4;
    }
}

#[cfg(feature = "async")]
#[derive(Debug)]
pub struct RxSink {
    _private: PhantomData<()>,
}

#[cfg(feature = "async")]
impl RxSink {
    fn new() -> Self {
        Self {
            _private: PhantomData,
        }
    }
}

#[cfg(feature = "async")]
#[derive(Debug)]
pub struct TxSink {
    buffer: Vec<Vec<u8>>,
}

#[cfg(feature = "async")]
impl TxSink {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }
}

#[cfg(feature = "async")]
impl Stream for RTL8139 {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if let Some(x) = self.frames.pop() {
            return Poll::Ready(Some(x));
        }

        WAKER.register(&cx.waker());

        match self.frames.pop() {
            Some(x) => {
                WAKER.take();
                Poll::Ready(Some(x))
            }
            None => Poll::Pending,
        }
    }
}

#[cfg(feature = "async")]
impl Sink<Vec<u8>> for RTL8139 {
    type Error = ();

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        self.tx.buffer.push(item);
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        interrupts::without_interrupts(|| {
            let items = self.tx.buffer.drain(..).collect::<Vec<_>>();
            for item in items {
                unsafe { self.state.write(&item) };
            }
        });
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}

