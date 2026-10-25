// Asyncio wrapper for serial communications (port of moonraker/utils/async_serial.py)
//
// pyserial is replaced by direct termios configuration with the same raw
// settings pyserial applies.

use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::spawn;
use std::cell::{Cell, RefCell};
use std::os::fd::{FromRawFd, RawFd};
use std::rc::Rc;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};

type SendResult = Result<(), String>;

pub struct AsyncSerial {
    pub name: String,
    pub port: String,
    baud: i64,
    fd: Cell<Option<RawFd>>,
    chunk_tx: RefCell<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    chunk_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>,
    buf: RefCell<Vec<u8>>,
    reader_task: crate::server::TaskSlot,
    send_buffer: RefCell<Vec<(oneshot::Sender<SendResult>, Vec<u8>)>>,
    sending: Cell<bool>,
}

fn baud_const(baud: i64) -> Option<libc::speed_t> {
    Some(match baud {
        50 => libc::B50,
        75 => libc::B75,
        110 => libc::B110,
        134 => libc::B134,
        150 => libc::B150,
        200 => libc::B200,
        300 => libc::B300,
        600 => libc::B600,
        1200 => libc::B1200,
        1800 => libc::B1800,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        460800 => libc::B460800,
        500000 => libc::B500000,
        576000 => libc::B576000,
        921600 => libc::B921600,
        1000000 => libc::B1000000,
        1152000 => libc::B1152000,
        1500000 => libc::B1500000,
        2000000 => libc::B2000000,
        2500000 => libc::B2500000,
        3000000 => libc::B3000000,
        3500000 => libc::B3500000,
        4000000 => libc::B4000000,
        _ => return None,
    })
}

fn os_err(port: &str) -> String {
    let e = std::io::Error::last_os_error();
    let code = e.raw_os_error().unwrap_or(0);
    let msg = e.to_string();
    let msg = msg.split(" (os error").next().unwrap_or(&msg).to_string();
    format!("[Errno {}] could not open port {}: [Errno {}] {}: '{}'", code, port, code, msg, port)
}

impl AsyncSerial {
    pub fn new(name: &str, port: &str, baud: i64) -> Rc<Self> {
        Rc::new(AsyncSerial {
            name: name.to_string(),
            port: port.to_string(),
            baud,
            fd: Cell::new(None),
            chunk_tx: RefCell::new(None),
            chunk_rx: tokio::sync::Mutex::new(None),
            buf: RefCell::new(Vec::new()),
            reader_task: Default::default(),
            send_buffer: RefCell::new(Vec::new()),
            sending: Cell::new(false),
        })
    }

    pub fn from_config(config: &ConfigHelper, default_baud: i64) -> Result<Rc<Self>, ConfigError> {
        let port = config.get("serial")?;
        let baud = config.getint_or("baud", default_baud)?;
        Ok(Self::new(config.get_name(), &port, baud))
    }

    pub fn connected(&self) -> bool {
        self.fd.get().is_some()
    }

    pub fn open(self: &Rc<Self>, exclusive: bool) -> Result<(), String> {
        if self.connected() {
            return Ok(());
        }
        log::info!("{} :Attempting to open serial device: {}", self.name, self.port);
        let cpath = std::ffi::CString::new(self.port.as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: cpath is a NUL-terminated CString alive for the call
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(os_err(&self.port));
        }
        let fail = |msg: String| {
            // SAFETY: fd was opened above and nothing owns it yet; each failure path closes it once and returns
            unsafe { libc::close(fd) };
            Err(msg)
        };
        // SAFETY: fd is the open port; flock takes no pointers
        if exclusive && unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return fail(format!("Could not exclusively lock port {}: {}", self.port, std::io::Error::last_os_error()));
        }
        // SAFETY: a C struct of integers and arrays: all zero is a valid value
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is the open port and tio is exclusively borrowed for tcgetattr to fill
        if unsafe { libc::tcgetattr(fd, &mut tio) } == 0 {
            tio.c_cflag |= libc::CLOCAL | libc::CREAD;
            tio.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL | libc::ISIG | libc::IEXTEN);
            tio.c_oflag &= !(libc::OPOST | libc::ONLCR | libc::OCRNL);
            tio.c_iflag &= !(libc::INLCR | libc::IGNCR | libc::ICRNL | libc::IGNBRK | libc::PARMRK | libc::INPCK | libc::ISTRIP);
            tio.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
            tio.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS);
            tio.c_cflag |= libc::CS8;
            tio.c_cc[libc::VMIN] = 0;
            tio.c_cc[libc::VTIME] = 0;
            match baud_const(self.baud) {
                // SAFETY: tio is a valid termios, exclusively borrowed
                Some(b) => unsafe {
                    libc::cfsetispeed(&mut tio, b);
                    libc::cfsetospeed(&mut tio, b);
                },
                None => return fail(format!("Invalid baud rate: {}", self.baud)),
            }
            // SAFETY: fd is the open port and tio a valid termios
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) } != 0 {
                return fail(format!("Could not configure port: {}", std::io::Error::last_os_error()));
            }
        }
        self.fd.set(Some(fd));
        let (tx, rx) = mpsc::unbounded_channel();
        *self.chunk_tx.borrow_mut() = Some(tx.clone());
        if let Ok(mut g) = self.chunk_rx.try_lock() {
            *g = Some(rx);
        }
        self.buf.borrow_mut().clear();
        let w = Rc::downgrade(self);
        // the reader task owns the descriptor and closes it when it ends, so
        // its epoll registration never outlives the fd (abort is deferred)
        // SAFETY: fd was opened above and is owned by nothing else: from here the reader task owns it
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let h = spawn(async move {
            // SAFETY: the OwnedFd keeps fd open for the AsyncFd's lifetime
            let afd = match unsafe { AsyncFd::register(owned) } {
                Ok(a) => a,
                Err(_) => return,
            };
            let mut data = vec![0u8; 4096];
            loop {
                let mut guard = match afd.readable().await {
                    Ok(g) => g,
                    Err(_) => return,
                };
                // SAFETY: data is data.len() writable bytes; fd is open while afd (which owns it) lives
                let n = unsafe { libc::read(fd, data.as_mut_ptr() as *mut libc::c_void, data.len()) };
                if n < 0 {
                    // Moonraker's _handle_incoming ignores an OSError and
                    // keeps reading; returning would close the fd that
                    // self.fd still names for sending
                    guard.clear_ready();
                    continue;
                }
                if n == 0 {
                    if let Some(s) = w.upgrade() {
                        log::info!("{}: No data received, disconnecting", s.name);
                        drop(afd);
                        s.close_now();
                    }
                    return;
                }
                if tx.send(data[..n as usize].to_vec()).is_err() {
                    return;
                }
                // a tty in VMIN=0 mode reads 0 bytes instead of EAGAIN when
                // drained, so wait for the next readiness event once the
                // buffer is empty.  A 0 byte read on a fresh event is a hangup.
                if (n as usize) < data.len() {
                    guard.clear_ready();
                }
            }
        });
        self.reader_task.set(h);
        log::info!("{} Connected", self.name);
        Ok(())
    }

    /// Reads one line including the terminating newline, None at EOF.
    /// Dropping the future (cancel) leaves buffered data intact.
    pub async fn read_line(&self) -> Option<Vec<u8>> {
        let mut rx = self.chunk_rx.lock().await;
        loop {
            {
                let mut buf = self.buf.borrow_mut();
                if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    return Some(buf.drain(..=pos).collect());
                }
            }
            let chunk = match rx.as_mut() {
                None => None,
                Some(r) => r.recv().await,
            };
            match chunk {
                Some(c) => self.buf.borrow_mut().extend(c),
                None => {
                    *rx = None;
                    let rest: Vec<u8> = std::mem::take(&mut *self.buf.borrow_mut());
                    return if rest.is_empty() { None } else { Some(rest) };
                }
            }
        }
    }

    fn close_now(&self) {
        // aborting the reader drops its AsyncFd, which closes the fd
        self.reader_task.cancel();
        if self.fd.take().is_some() {
            log::info!("{}: Disconnected", self.name);
        }
        for (tx, _) in self.send_buffer.borrow_mut().drain(..) {
            let _ = tx.send(Err("Serial Device Closed".into()));
        }
        *self.chunk_tx.borrow_mut() = None;
    }

    pub async fn close(&self) {
        self.close_now();
        for _ in 0..200 {
            if !self.sending.get() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub fn send(self: &Rc<Self>, data: Vec<u8>) -> oneshot::Receiver<SendResult> {
        let (tx, rx) = oneshot::channel();
        if !self.connected() {
            let _ = tx.send(Err("Serial Device Closed".into()));
            return rx;
        }
        self.send_buffer.borrow_mut().push((tx, data));
        if !self.sending.get() {
            self.sending.set(true);
            let s = self.clone();
            spawn(async move { s.do_send().await });
        }
        rx
    }

    async fn do_send(&self) {
        loop {
            // upstream pops from the end of the buffer
            let item = self.send_buffer.borrow_mut().pop();
            let (tx, mut data) = match item {
                None => break,
                Some(i) => i,
            };
            while !data.is_empty() {
                let sent = match self.fd.get() {
                    None => 0,
                    Some(fd) => {
                        // SAFETY: data is data.len() readable bytes; self.fd holds the port's fd while the reader task owns it open, and close_now() clears it before the task is aborted
                        let r = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
                        if r < 0 {
                            let e = std::io::Error::last_os_error();
                            if matches!(e.raw_os_error(), Some(libc::EBADF) | Some(libc::EPIPE)) {
                                0
                            } else {
                                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                                continue;
                            }
                        } else {
                            r as usize
                        }
                    }
                };
                if sent == 0 {
                    log::error!("{}: Error writing data, closing serial connection", self.name);
                    let _ = tx.send(Err("Serial Device Closed".into()));
                    self.sending.set(false);
                    self.close_now();
                    return;
                }
                data.drain(..sent);
            }
            let _ = tx.send(Ok(()));
        }
        self.sending.set(false);
    }
}
