/// Async ZNP transport — serial (default) or sequential-bulk CH340 mode.
///
/// # Features `usbdevfs-serial` / `ch340-usbdevfs`
///
/// Some dwc2 USB host controllers cannot multiplex Bulk-IN and Bulk-OUT URBs
/// simultaneously on the same full-speed device.  When the kernel serial driver
/// is active, `usb_serial_generic_open()` submits persistent Bulk-IN URBs
/// that permanently starve outgoing frames; ZNP SREQ frames are never
/// transmitted.  Building with `--features ch340-usbdevfs` (which implies
/// `usbdevfs-serial`) bypasses the kernel driver entirely via USBDEVFS ioctls
/// and issues Bulk-OUT before each Bulk-IN poll, so the host controller never
/// sees concurrent URBs on the device.
///
/// `usbdevfs-serial` provides the shared USBDEVFS struct definitions and bulk
/// I/O loop.  `ch340-usbdevfs` adds the CH340-specific vendor control
/// transfers for UART initialisation.  Other chips can be supported by adding
/// a parallel `<chip>-usbdevfs` feature that also implies `usbdevfs-serial`.
///
/// This feature is a targeted workaround and must be explicitly opted into.
/// The transport config must supply a raw USB device path
/// (`/dev/bus/usb/BUS/DEV`), not a TTY path.  Determine the path with
/// `lsusb` or `dmesg | grep 'New USB device'`.
///
/// Ioctl struct sizes and ioctl numbers are derived from the target pointer
/// width at compile time (armhf/32-bit vs aarch64/x86_64/64-bit).
///
/// ## Deployment
///
/// ```text
/// # /etc/modprobe.d/blacklist-ch341.conf
/// blacklist ch341
///
/// # /etc/udev/rules.d/99-ch340.rules
/// SUBSYSTEM=="usb", ATTRS{idVendor}=="1a86", ATTRS{idProduct}=="7523", \
///     MODE="0660", GROUP="plugdev"
/// ```
///
/// Non-Linux targets always use the tokio-serial path regardless of the flag.
use std::time::Duration;

use bytes::BytesMut;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, trace, warn};

use super::frame::{FrameType, Subsystem, ZnpCodec, ZnpFrame};
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize  = 64;

// Guard against enabling the generic `usbdevfs-serial` mechanism without a
// chip-specific feature that supplies the UART init logic.  Each concrete
// `<chip>-usbdevfs` feature implies `usbdevfs-serial` in Cargo.toml, so this
// fires only when someone enables `usbdevfs-serial` directly.
//
// When adding a new chip feature, add `not(feature = "<chip>-usbdevfs")` here.
#[cfg(all(
    feature = "usbdevfs-serial",
    not(feature = "ch340-usbdevfs"),
    target_os = "linux",
))]
compile_error!(
    "`usbdevfs-serial` must not be enabled directly on Linux; \
     use a chip-specific feature such as `ch340-usbdevfs` (which implies it). \
     If adding a new chip, add its feature to the not() list above."
);

// ── Linux USBDEVFS ioctl constants ───────────────────────────────────────────
// CLAIMINTERFACE / RELEASEINTERFACE / DISCONNECT take a fixed-size 4-byte arg
// so their ioctl numbers are the same on all architectures.
//
// BULK and CONTROL take a struct that contains a `void *` data pointer, so
// their ioctl numbers differ by pointer width:
//   armhf (32-bit): struct size = 16 bytes → 0xC010_5502 / 0xC010_5500
//   x86_64/aarch64: struct size = 24 bytes → 0xC018_5502 / 0xC018_5500
// We derive the correct number at compile time from the actual struct size.
#[cfg(all(feature = "usbdevfs-serial", target_os = "linux"))]
mod usbdevfs {
    // _IOR('U', 15, unsigned int)  — kernel reads interface number from userspace
    pub const CLAIMINTERFACE:   libc::c_ulong = 0x8004_550F;
    // _IOR('U', 16, unsigned int)  — kernel reads interface number from userspace
    pub const RELEASEINTERFACE: libc::c_ulong = 0x8004_5510;
    // _IO('U', 0x16)
    pub const DISCONNECT:       libc::c_ulong = 0x5516;
}

/// Bulk-transfer descriptor (`struct usbdevfs_bulktransfer` from
/// `linux/usbdevice_fs.h`).  The data pointer is naturally pointer-sized
/// so `size_of` returns 16 on 32-bit and 24 on 64-bit targets.
#[repr(C)]
#[cfg(all(feature = "usbdevfs-serial", target_os = "linux"))]
struct BulkXfer {
    ep:      u32,
    len:     u32,
    timeout: u32,               // milliseconds
    data:    *mut libc::c_void, // pointer to transfer buffer
}

/// Control-transfer descriptor (`struct usbdevfs_ctrltransfer`).
/// Same pointer-width dependency as `BulkXfer`.
#[repr(C)]
#[cfg(all(feature = "usbdevfs-serial", target_os = "linux"))]
struct CtrlXfer {
    request_type: u8,
    request:      u8,
    value:        u16,
    index:        u16,
    length:       u16,
    timeout:      u32,          // milliseconds
    data:         *mut libc::c_void,
}

// _IOWR('U', nr, sizeof(T)):  direction RW = 3, encoded per linux/ioctl.h.
#[cfg(all(feature = "usbdevfs-serial", target_os = "linux"))]
const fn _iowr(t: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((3u32 << 30) | (t << 8) | nr | (size << 16)) as libc::c_ulong
}

/// `USBDEVFS_BULK` ioctl number — correct for current target pointer width.
#[cfg(all(feature = "usbdevfs-serial", target_os = "linux"))]
const USBDEVFS_BULK: libc::c_ulong =
    _iowr(b'U' as u32, 2, std::mem::size_of::<BulkXfer>() as u32);

/// `USBDEVFS_CONTROL` ioctl number — correct for current target pointer width.
#[cfg(all(feature = "usbdevfs-serial", target_os = "linux"))]
const USBDEVFS_CONTROL: libc::c_ulong =
    _iowr(b'U' as u32, 0, std::mem::size_of::<CtrlXfer>() as u32);

// ── Public events ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ZnpEvent {
    ResetInd,
    EndDeviceAnnceInd(Vec<u8>),
    LeaveInd(Vec<u8>),
    AfIncomingMsg(Vec<u8>),
    ActiveEpRsp(Vec<u8>),
    SimpleDescRsp(Vec<u8>),
    IeeeAddrRsp(Vec<u8>),
    TcDevInd(Vec<u8>),
    StateChangeInd,
    Other,
}

enum ActorMsg {
    Send(ZnpFrame),
    Request {
        frame:    ZnpFrame,
        reply_tx: oneshot::Sender<Result<ZnpFrame>>,
    },
}

// ── CH340 direct I/O thread ──────────────────────────────────────────────────
/// Send a CH340 vendor OUT control transfer to configure the UART.
#[cfg(all(feature = "ch340-usbdevfs", target_os = "linux"))]
unsafe fn ch340_ctrl(fd: libc::c_int, req: u8, value: u16, index: u16) -> libc::c_int {
    let xfer = CtrlXfer {
        request_type: 0x40,   // USB_DIR_OUT | USB_TYPE_VENDOR | USB_RECIP_DEVICE
        request:      req,
        value,
        index,
        length:       0,
        timeout:      1000,
        data:         std::ptr::null_mut(),
    };
    libc::ioctl(fd, USBDEVFS_CONTROL, &xfer as *const CtrlXfer)
}

/// Runs in a dedicated std::thread.  All USB Bulk transfers are sequential:
/// write (OUT) then read (IN), so the dwc2 scheduler never sees concurrent
/// Bulk-IN and Bulk-OUT URBs on the same full-speed device.
#[cfg(all(feature = "ch340-usbdevfs", target_os = "linux"))]
struct UsbIoThread {
    fd:       std::os::unix::io::OwnedFd,
    write_rx: mpsc::Receiver<bytes::Bytes>,
    frame_tx: mpsc::Sender<ZnpFrame>,
}

#[cfg(all(feature = "ch340-usbdevfs", target_os = "linux"))]
impl UsbIoThread {
    fn run(mut self) {
        use std::os::unix::io::AsRawFd;
        use tokio_util::codec::Decoder;
        use usbdevfs::RELEASEINTERFACE;

        // Borrow the raw fd — OwnedFd (self.fd) retains ownership and closes
        // the descriptor when this function returns, regardless of exit path.
        let fd = self.fd.as_raw_fd();

        // ── Configure CH340 UART (115200 8N1, no flow control) ───────────────
        // All control transfers happen before any Bulk-IN is submitted, so
        // they succeed regardless of the dwc2 scheduler.
        // A failed control transfer means the UART is misconfigured; no valid
        // ZNP frames will follow, so treat any error here as fatal.
        // Each transfer is checked individually so the error log names the
        // specific request that failed.
        // fd is closed by OwnedFd drop on return; no explicit close needed.
        for &(name, req, value, index) in &[
            ("SERIAL_INIT",    0xA1u8, 0x0000u16, 0x0000u16),
            ("BAUD_PRESCALER", 0x9A,   0x1312,    0xCC83),
            ("BAUD_LCR",       0x9A,   0x2518,    0x00C3),
            ("MODEM_CTRL",     0xA4,   0xFFFF,    0x0000),
        ] {
            if unsafe { ch340_ctrl(fd, req, value, index) } < 0 {
                error!("IO: CH340 UART init failed at {name}: {}",
                       std::io::Error::last_os_error());
                /* closing the usbdevfs fd implicitly releases all claimed interfaces */
                return;
            }
        }
        trace!("IO: CH340 configured at 115200 8N1");

        let mut codec  = ZnpCodec;
        let mut rxbuf  = BytesMut::new();
        let mut tmp    = vec![0u8; 256];

        trace!("IO: entering bulk transfer loop");

        loop {
            // ── Drain any pending outgoing bytes first ────────────────────────
            loop {
                match self.write_rx.try_recv() {
                    Ok(bytes) => {
                        trace!("IO: writing {} bytes: {}", bytes.len(),
                              bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
                        let ok = unsafe {
                            let xfer = BulkXfer {
                                ep:      0x02,   // CH340 Bulk-OUT endpoint
                                len:     bytes.len() as u32,
                                timeout: 2000,
                                data:    bytes.as_ptr() as *mut libc::c_void,
                            };
                            libc::ioctl(fd, USBDEVFS_BULK, &xfer as *const BulkXfer)
                        };
                        if ok < 0 {
                            error!("IO: Bulk-OUT failed: {}; thread exiting",
                                   std::io::Error::last_os_error());
                            // A failed Bulk-OUT means the dongle is in an unknown state;
                            // continuing to send more frames would corrupt the ZNP stream.
                            unsafe { libc::ioctl(fd, RELEASEINTERFACE, &0u32 as *const u32); }
                            return;
                        } else {
                            trace!("IO: write OK ({} bytes transmitted)", ok);
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        // actor dropped; fd closed by OwnedFd drop
                        unsafe { libc::ioctl(fd, RELEASEINTERFACE, &0u32 as *const u32); }
                        return;
                    }
                }
            }

            // ── Short Bulk-IN poll ────────────────────────────────────────────
            // Timeout is intentionally short (10 ms): the loop returns here on
            // every ETIMEDOUT to drain any pending outgoing writes before the
            // next read.  Increasing it reduces CPU load but adds that many ms
            // of worst-case SREQ latency; decreasing it increases CPU load on
            // the Pi Zero.  10 ms is a reasonable default for ZNP traffic.
            let n = unsafe {
                let xfer = BulkXfer {
                    ep:      0x82,   // CH340 Bulk-IN endpoint
                    len:     tmp.len() as u32,
                    timeout: 10,     // ms
                    data:    tmp.as_mut_ptr() as *mut libc::c_void,
                };
                libc::ioctl(fd, USBDEVFS_BULK, &xfer as *const BulkXfer)
            };

            if n > 0 {
                let n = n as usize;
                trace!("IO: read {n} bytes: {}",
                      tmp[..n].iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
                rxbuf.extend_from_slice(&tmp[..n]);
                loop {
                    match codec.decode(&mut rxbuf) {
                        Ok(Some(frame)) => {
                            trace!(?frame, "← ZNP recv");
                            if self.frame_tx.blocking_send(frame).is_err() {
                                // actor dropped; fd closed by OwnedFd drop
                                unsafe { libc::ioctl(fd, RELEASEINTERFACE, &0u32 as *const u32); }
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            warn!("ZNP codec error: {e}");
                            rxbuf.clear();
                            break;
                        }
                    }
                }
            } else if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ETIMEDOUT) {
                    // Normal read timeout — loop to check writes
                } else {
                    error!("IO: Bulk-IN error: {e}; thread exiting");
                    break;
                }
            }
            // n == 0: empty read → loop
        }

        // fd closed by OwnedFd drop
        unsafe { libc::ioctl(fd, RELEASEINTERFACE, &0u32 as *const u32); }
    }
}

// ── Actor task ────────────────────────────────────────────────────────────────

struct TransportActor {
    /// Encoded bytes to forward to the I/O thread for writing.
    write_tx:  mpsc::Sender<bytes::Bytes>,
    /// Decoded frames arriving from the I/O thread.
    frame_rx:  mpsc::Receiver<ZnpFrame>,
    actor_rx:  mpsc::Receiver<ActorMsg>,
    event_tx:  mpsc::Sender<ZnpEvent>,
    pending:   Option<(u8, u8, oneshot::Sender<Result<ZnpFrame>>)>, // (cmd0_sub, cmd1, tx)
}

impl TransportActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                msg = self.actor_rx.recv() => {
                    match msg {
                        None => break,
                        Some(ActorMsg::Send(frame)) => {
                            trace!(?frame, "→ ZNP send AREQ");
                            let mut buf = BytesMut::new();
                            frame.encode_to(&mut buf);
                            if self.write_tx.send(buf.freeze()).await.is_err() {
                                error!("ZNP write channel closed");
                            }
                        }
                        Some(ActorMsg::Request { frame, reply_tx }) => {
                            let expected_cmd0 = frame.cmd0();
                            let cmd1          = frame.cmd1;
                            trace!(?frame, "→ ZNP send SREQ");
                            let mut buf = BytesMut::new();
                            frame.encode_to(&mut buf);
                            if self.write_tx.send(buf.freeze()).await.is_err() {
                                let _ = reply_tx.send(Err(Error::ChannelClosed));
                                continue;
                            }
                            trace!("actor: SREQ sent subsys=0x{:02X} cmd1=0x{:02X}", expected_cmd0 & 0x1F, cmd1);
                            self.pending = Some((expected_cmd0 & 0x1F, cmd1, reply_tx));
                        }
                    }
                }

                frame = self.frame_rx.recv() => {
                    match frame {
                        None => {
                            // The I/O source (USB thread or serial decode task) exited.
                            // Cancel any pending SRSP waiter immediately with ChannelClosed
                            // so callers see the correct error rather than timing out.
                            if let Some((_, _, reply_tx)) = self.pending.take() {
                                let _ = reply_tx.send(Err(Error::ChannelClosed));
                            }
                            error!("ZNP I/O source exited unexpectedly; transport is dead");
                            break;
                        }
                        Some(f) => self.dispatch(f),
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, frame: ZnpFrame) {
        trace!(?frame, "← ZNP recv");

        if frame.frame_type == FrameType::SRsp {
            if let Some((expected_sub, expected_cmd1, _)) = &self.pending {
                if frame.subsystem as u8 == *expected_sub && frame.cmd1 == *expected_cmd1 {
                    let (_, _, reply_tx) = self.pending.take().unwrap();
                    let _ = reply_tx.send(Ok(frame));
                    return;
                }
                // Mismatched subsystem but there's a pending request — deliver it
                // anyway. Z-Stack 1.2 may return subsystem 0 for errors.
                if frame.cmd1 == *expected_cmd1 {
                    warn!(
                        "SRSP subsystem mismatch: expected 0x{:02X} got 0x{:02X}, delivering anyway",
                        expected_sub, frame.subsystem as u8
                    );
                    let (_, _, reply_tx) = self.pending.take().unwrap();
                    let _ = reply_tx.send(Ok(frame));
                    return;
                }
            }
            warn!(
                "Unexpected SRSP sub=0x{:02X} cmd1=0x{:02X}",
                frame.subsystem as u8, frame.cmd1
            );
            return;
        }

        let event = match (frame.subsystem, frame.cmd1) {
            (Subsystem::Sys, 0x80) => ZnpEvent::ResetInd,
            (Subsystem::Zdo, 0x81) => ZnpEvent::IeeeAddrRsp(frame.data),
            (Subsystem::Zdo, 0x84) => ZnpEvent::SimpleDescRsp(frame.data),
            (Subsystem::Zdo, 0x85) => ZnpEvent::ActiveEpRsp(frame.data),
            (Subsystem::Zdo, 0xC0) => ZnpEvent::StateChangeInd,
            (Subsystem::Zdo, 0xC1) => ZnpEvent::EndDeviceAnnceInd(frame.data),
            (Subsystem::Zdo, 0xC9) => ZnpEvent::LeaveInd(frame.data),
            (Subsystem::Zdo, 0xCA) => ZnpEvent::TcDevInd(frame.data),
            (Subsystem::Af, 0x81) => ZnpEvent::AfIncomingMsg(frame.data),
            _ => ZnpEvent::Other,
        };

        if let Err(e) = self.event_tx.try_send(event) {
            warn!("ZNP event queue full, dropping: {e}");
        }
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ZnpTransport {
    actor_tx: mpsc::Sender<ActorMsg>,
}

impl ZnpTransport {
    /// Open the ZNP transport.
    ///
    /// With `--features ch340-usbdevfs` on Linux: sequential USBDEVFS bulk
    /// transfers bypassing the ch341 kernel driver, with CH340 vendor UART
    /// init.  `device_path` must be a raw USB device node
    /// (`/dev/bus/usb/BUS/DEV`).  All other targets and configurations use
    /// the standard tokio-serial path.
    pub fn open(device_path: &str, baud: u32) -> Result<(Self, mpsc::Receiver<ZnpEvent>)> {
        let (write_tx, write_rx) = mpsc::channel::<bytes::Bytes>(CHANNEL_CAPACITY);
        let (frame_tx, frame_rx) = mpsc::channel::<ZnpFrame>(CHANNEL_CAPACITY);
        let (actor_tx, actor_rx) = mpsc::channel::<ActorMsg>(CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel::<ZnpEvent>(CHANNEL_CAPACITY);

        // The two cfg blocks below are mutually exclusive by construction:
        // ch340-usbdevfs implies usbdevfs-serial, and the compile_error! guard
        // above prevents usbdevfs-serial without a chip flag on Linux.  Each
        // block consumes the opposite ends of the (write_tx/write_rx) and
        // (frame_tx/frame_rx) channel pairs — they are disjoint and never
        // both active in the same binary.
        //
        // CH340 USBDEVFS bulk transport (ch340-usbdevfs feature, Linux only).
        // device_path must be a /dev/bus/usb/BUS/DEV node; see module doc.
        #[cfg(all(feature = "ch340-usbdevfs", target_os = "linux"))]
        {
            use std::fs;
            use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
            use usbdevfs::{CLAIMINTERFACE, DISCONNECT};

            let _ = baud; // baud rate set via CH340 vendor control transfers, not tokio-serial

            info!("ZNP: opening CH340 device {device_path}");
            // into_raw_fd() transfers ownership; the fd is naked until OwnedFd
            // wraps it below.  On the CLAIMINTERFACE error path we close it
            // manually because OwnedFd does not yet exist.
            let fd = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(device_path)
                .map_err(|e| Error::Znp(format!("open {device_path}: {e}")))?
                .into_raw_fd();

            // Detach ch341 kernel driver (ignore error if already detached)
            unsafe { libc::ioctl(fd, DISCONNECT, &0u32 as *const u32); }

            // Claim interface 0 exclusively
            let claim = unsafe { libc::ioctl(fd, CLAIMINTERFACE, &0u32 as *const u32) };
            if claim < 0 {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd); } // OwnedFd not yet created; must close manually
                return Err(Error::Znp(format!("USBDEVFS_CLAIMINTERFACE: {e}")));
            }
            info!("ZNP: USB interface claimed");

            let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) }; // fd ownership transferred; drop closes it
            let io_thread = UsbIoThread { fd: owned_fd, write_rx, frame_tx };
            std::thread::Builder::new()
                .name("znp-usb-io".into())
                .spawn(move || io_thread.run())
                .expect("failed to spawn ZNP USB I/O thread");
        }

        // ── Serial transport: all non-Linux targets, or Linux without any usbdevfs chip feature ──
        // NOTE: this guard uses `usbdevfs-serial` (the parent) rather than each individual chip
        // feature.  Every concrete `<chip>-usbdevfs` feature must imply `usbdevfs-serial` in
        // Cargo.toml, so that enabling any chip flag automatically suppresses this path.  If you
        // add a new chip feature, ensure it lists `usbdevfs-serial` as a dependency — otherwise
        // both this serial block and no usbdevfs block will be compiled, which is wrong.
        #[cfg(not(all(feature = "usbdevfs-serial", target_os = "linux")))]
        {
            use futures::StreamExt;
            use tokio_serial::SerialPortBuilderExt;
            use tokio_util::codec::FramedRead;

            info!("ZNP: opening serial port {device_path} at {baud} baud");
            let port = tokio_serial::new(device_path, baud)
                .open_native_async()
                .map_err(|e| Error::Znp(format!("open serial {device_path}: {e}")))?;

            let (read_half, mut write_half) = tokio::io::split(port);
            let mut framed = FramedRead::new(read_half, ZnpCodec);

            tokio::spawn(async move {
                while let Some(result) = framed.next().await {
                    match result {
                        Ok(frame) => {
                            trace!(?frame, "← ZNP recv");
                            /* actor dropped; actor task will notice frame_rx closed */
                            if frame_tx.send(frame).await.is_err() { break; }
                        }
                        Err(e) => { error!("ZNP serial decode: {e}"); break; }
                    }
                }
            });

            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut write_rx = write_rx;
                while let Some(bytes) = write_rx.recv().await {
                    if let Err(e) = write_half.write_all(&bytes).await {
                        error!("ZNP serial write error: {e}");
                        break;
                    }
                }
            });
        }

        let actor = TransportActor { write_tx, frame_rx, actor_rx, event_tx, pending: None };
        tokio::spawn(actor.run());

        Ok((Self { actor_tx }, event_rx))
    }

    /// Fire-and-forget AREQ send.
    pub async fn send(&self, frame: ZnpFrame) -> Result<()> {
        self.actor_tx
            .send(ActorMsg::Send(frame))
            .await
            .map_err(|_| Error::ChannelClosed)
    }

    /// Send SREQ and wait for matching SRSP.
    pub async fn request(&self, frame: ZnpFrame) -> Result<ZnpFrame> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.actor_tx
            .send(ActorMsg::Request { frame, reply_tx })
            .await
            .map_err(|_| Error::ChannelClosed)?;

        trace!("request: waiting up to {}s for SRSP", REQUEST_TIMEOUT.as_secs());
        let result = tokio::time::timeout(REQUEST_TIMEOUT, reply_rx)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::ChannelClosed)?;
        trace!("request: SRSP received");
        result
    }
}

// ── Ioctl-number sanity checks ────────────────────────────────────────────────
//
// These are compile-time assertions (via `const`) that the ioctl numbers we
// derive match the values in linux/usbdevice_fs.h for the current target.
//
// Expected values:
//   32-bit targets (arm-unknown-linux-gnueabihf): struct size = 16 → 0xC010_5502 / 0xC010_5500
//   64-bit targets (aarch64, x86_64):             struct size = 24 → 0xC018_5502 / 0xC018_5500
//
// Cross-compile checks (run from the zigbee2mqtt-rs directory):
//   cargo test --features ch340-usbdevfs --target arm-unknown-linux-gnueabihf
//   cargo test --features ch340-usbdevfs --target aarch64-unknown-linux-gnu
//   cargo test --features ch340-usbdevfs --target x86_64-unknown-linux-gnu
//
// Using cross (Docker sysroot, no host linker required):
//   cross test --features ch340-usbdevfs --target arm-unknown-linux-gnueabihf
#[cfg(all(test, feature = "usbdevfs-serial", target_os = "linux"))]
mod ioctl_sanity {
    use super::*;

    #[test]
    fn bulk_ioctl_number_matches_kernel_header() {
        // _IOWR('U', 2, struct usbdevfs_bulktransfer)
        let expected = _iowr(b'U' as u32, 2, std::mem::size_of::<BulkXfer>() as u32);
        assert_eq!(
            USBDEVFS_BULK, expected,
            "USBDEVFS_BULK mismatch on this target (pointer width = {} bytes)",
            std::mem::size_of::<*mut ()>()
        );
        // Known values for common targets:
        #[cfg(target_pointer_width = "32")]
        assert_eq!(USBDEVFS_BULK, 0xC010_5502, "expected 0xC010_5502 on 32-bit");
        #[cfg(target_pointer_width = "64")]
        assert_eq!(USBDEVFS_BULK, 0xC018_5502, "expected 0xC018_5502 on 64-bit");
    }

    #[test]
    fn control_ioctl_number_matches_kernel_header() {
        // _IOWR('U', 0, struct usbdevfs_ctrltransfer)
        let expected = _iowr(b'U' as u32, 0, std::mem::size_of::<CtrlXfer>() as u32);
        assert_eq!(
            USBDEVFS_CONTROL, expected,
            "USBDEVFS_CONTROL mismatch on this target (pointer width = {} bytes)",
            std::mem::size_of::<*mut ()>()
        );
        #[cfg(target_pointer_width = "32")]
        assert_eq!(USBDEVFS_CONTROL, 0xC010_5500, "expected 0xC010_5500 on 32-bit");
        #[cfg(target_pointer_width = "64")]
        assert_eq!(USBDEVFS_CONTROL, 0xC018_5500, "expected 0xC018_5500 on 64-bit");
    }
}
