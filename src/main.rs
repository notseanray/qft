use lz4_flex::block::{compress_prepend_size, decompress_size_prepended};
use std::{
    collections::{hash_map::Entry, HashMap},
    env,
    fs::{File, OpenOptions},
    io::{stdout, Error, Read, Seek, SeekFrom, Write},
    net::*,
    ops::Mul,
    str::FromStr,
    thread,
    time::{Duration, SystemTime},
};

#[derive(Ord, Eq, PartialOrd, PartialEq)]
enum SafeReadWritePacket {
    Write,
    Ack,
    ResendRequest,
    End,
}
use SafeReadWritePacket::*;

struct SafeReadWrite {
    socket: UdpSocket,
    last_transmitted: HashMap<u16, Vec<u8>>,
    packet_count_out: u64,
    packet_count_in: u64,
}

struct Wrap<T>(T);

impl Mul<Wrap<&str>> for u64 {
    type Output = String;

    fn mul(self, rhs: Wrap<&str>) -> Self::Output {
        let strings: Vec<&str> = (0..self).map(|_| rhs.0).collect();
        strings.join("")
    }
}

impl SafeReadWrite {
    pub fn new(socket: UdpSocket) -> SafeReadWrite {
        SafeReadWrite {
            socket,
            last_transmitted: HashMap::new(),
            packet_count_in: 0,
            packet_count_out: 0,
        }
    }

    pub fn write_safe(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.write_flush_safe(buf, false)
    }

    pub fn write_flush_safe(&mut self, buf: &[u8], flush: bool) -> Result<(), Error> {
        self.internal_write_safe(buf, Write, flush, false)
    }

    pub fn read_safe(&mut self, buf: &[u8]) -> Result<(Vec<u8>, usize), Error> {
        if buf.len() > 0xfffc {
            panic!(
                "attempted to receive too large data packet with SafeReadWrite ({} > 0xfffc)",
                buf.len()
            );
        }

        let mut mbuf = Vec::from(buf);
        mbuf.insert(0, 0);
        mbuf.insert(0, 0);
        mbuf.insert(0, 0);
        let buf: &mut [u8] = mbuf.as_mut();

        let mut r = (vec![], 0);

        let mut try_again = true;
        let mut is_catching_up = false;
        while try_again {
            if let Ok(x) = self.socket.recv(buf) {
                if x < 3 {
                    continue;
                }
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                if id <= self.packet_count_in as u16 {
                    self.socket
                        .send(&[buf[0], buf[1], Ack as u8])
                        .expect("send error");
                }
                if id == self.packet_count_in as u16 {
                    if id == 0xffff {
                        println!("\nPacket ID wrap successful.");
                    }
                    try_again = false;
                    self.packet_count_in += 1;
                    r.1 = x - 3;
                } else if id > self.packet_count_in as u16
                    && (id - self.packet_count_in as u16) < 0xC000
                {
                    if !is_catching_up && env::var("QFT_HIDE_DROPS").is_err() {
                        println!(
                            "\r\x1b[KA packet dropped: {} (got) is newer than {} (expected)",
                            &id,
                            &(self.packet_count_in as u16)
                        );
                    }
                    is_catching_up = true;
                    // ask to resend, then do nothing
                    let id = (self.packet_count_in as u16).to_be_bytes();
                    self.socket
                        .send(&[id[0], id[1], ResendRequest as u8])
                        .expect("send error");
                }
                if buf[2] == End as u8 {
                    return Ok((vec![], 0));
                }
            }
        }
        mbuf.remove(0);
        mbuf.remove(0);
        mbuf.remove(0);
        r.0 = mbuf;
        Ok(r)
    }

    pub fn end(mut self) -> UdpSocket {
        let _ = self.internal_write_safe(&[], End, true, true);

        self.socket
    }

    fn internal_write_safe(
        &mut self,
        buf: &[u8],
        packet: SafeReadWritePacket,
        flush: bool,
        exit_on_lost: bool,
    ) -> Result<(), Error> {
        if buf.len() > 0xfffc {
            panic!(
                "too large data packet sent over SafeReadWrite ({} > 0xfffc)",
                buf.len()
            );
        }

        let id = (self.packet_count_out as u16).to_be_bytes();
        let idn = self.packet_count_out as u16;
        self.packet_count_out += 1;

        let mut vbuf = Vec::from(buf);
        vbuf.insert(0, packet as u8);
        vbuf.insert(0, id[1]);
        vbuf.insert(0, id[0]); // this is now the first byte
        let buf = vbuf.as_slice();

        loop {
            match self.socket.send(buf) {
                Ok(x) => {
                    if x != buf.len() {
                        continue;
                    }
                }
                Err(_) => {
                    continue;
                }
            }
            self.last_transmitted.insert(idn, vbuf);
            break;
        }
        let mut buf = [0, 0, 0];
        let mut wait = idn == 0xffff || flush;
        if self.last_transmitted.len() < 256 {
            self.socket
                .set_read_timeout(Some(Duration::from_millis(1)))
                .unwrap();
        } else {
            wait = true;
        }
        let mut start = unix_millis();
        if idn == 0xffff {
            print!("\nPacket ID needs to wrap. Waiting for partner to catch up...")
        }
        let mut is_catching_up = false;
        loop {
            match self.socket.recv(&mut buf).ok() {
                Some(x) => {
                    if x != 3 {
                        continue;
                    }
                    if buf[2] == Ack as u8 {
                        let n = u16::from_be_bytes([buf[0], buf[1]]);
                        self.last_transmitted.remove(&n);
                        if n == idn {
                            if idn == 0xffff {
                                println!("\r\x1b[KPacket ID wrap successful.");
                            }
                            wait = false;
                            self.last_transmitted.clear(); // if the latest packet is ACK'd, all
                                                           // previous ones must be as well.
                        }
                    }
                    if buf[2] == ResendRequest as u8 {
                        let mut n = u16::from_be_bytes([buf[0], buf[1]]);
                        if !is_catching_up && env::var("QFT_HIDE_DROPS").is_err() {
                            println!("\r\x1b[KA packet dropped: {}", &n);
                        }
                        wait = true;
                        is_catching_up = true;
                        while n <= idn && !(idn == 0xffff && n == 0) {
                            let buf = self.last_transmitted.get(&n);
                            if let Some(buf) = buf {
                                loop {
                                    // resend until success
                                    match self.socket.send(&compress_prepend_size(buf.as_slice())) {
                                        Ok(x) => {
                                            if x != buf.len() {
                                                continue;
                                            }
                                        }
                                        Err(_) => {
                                            continue;
                                        }
                                    };
                                    break;
                                }
                            } else {
                                break;
                            }
                            // do NOT remove from last_transmitted yet, wait for Ack to do that.
                            n += 1;
                        }
                    }
                }
                None => {
                    if unix_millis() - start > 5000 && exit_on_lost {
                        break;
                    }
                    if unix_millis() - start > 10000 {
                        println!("\n10s passed since last packet ==> Contact broke. Trying to resend packet...");
                        if let Some(buf) = self.last_transmitted.get(&idn) {
                            loop {
                                match self.socket.send(buf) {
                                    Ok(x) => {
                                        if x != buf.len() {
                                            continue;
                                        }
                                    }
                                    Err(_) => {
                                        continue;
                                    }
                                }
                                break;
                            }
                            start = unix_millis();
                        } else {
                            break; // Latest packet was already ACK'd ==> No packets properly lost ==> Can continue with next packet.
                        }
                    }
                    if !wait {
                        break;
                    }
                }
            }
        }
        self.socket
            .set_read_timeout(Some(Duration::from_millis(1000)))
            .unwrap();
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.is_empty() {
        panic!("no args");
    }
    match args
        .get(1)
        .unwrap() // checked in previous if-statement
        .as_str()
    {
        "helper" => helper(&args),
        "sender" => sender(&args, |_| {}),
        "receiver" => receiver(&args, |_| {}),
        "version" => println!("QFT version: {}", env!("CARGO_PKG_VERSION")),
        _ => print_args(&args),
    }
}

pub fn helper(args: &[String]) {
    let bind_addr = (
        "0.0.0.0",
        args[2]
            .as_str()
            .parse::<u16>()
            .expect("invalid port: must be integer"),
    );
    let mut map: HashMap<[u8; 200], SocketAddr> = HashMap::new();
    let listener = UdpSocket::bind(bind_addr).expect("unable to create socket");
    let mut buf = [0; 200];
    // let mut last_log_time = unix_millis();
    // let mut amount_since_log = 0;
    // let mut helper_log = OpenOptions::new()
    //     .create(true)
    //     .write(true)
    //     .append(true)
    //     .open("qft_helper_log.txt")
    //     .expect("unable to create helper log");
    loop {
        let (l, addr) = listener.recv_from(&mut buf).expect("read error");
        if l != 200 {
            continue;
        }
        if let Entry::Vacant(_) = map.entry(buf) {
            map.insert(buf, addr);
        } else {
            let other = map.get(&buf).unwrap();
            // we got a connection
            let mut bytes: &[u8] = addr.to_string().bytes().collect::<Vec<u8>>().leak();
            let mut addr_buf = [0; 200];
            addr_buf[..bytes.len().min(200)].copy_from_slice(&bytes[..bytes.len().min(200)]);
            bytes = other.to_string().bytes().collect::<Vec<u8>>().leak();
            let mut other_buf = [0; 200];
            other_buf[..bytes.len().min(200)].copy_from_slice(&bytes[..bytes.len().min(200)]);
            if listener.send_to(&addr_buf, other).is_ok()
                && listener.send_to(&other_buf, addr).is_ok()
            {
                // success!
                println!("Helped {} and {}! :D", addr, other);
                // amount_since_log += 1;
                // if unix_millis() - last_log_time > 10000 {
                // let d = PrimitiveDateTime::new(
                //     Date::from_calendar_date(1970, time::Month::January, 1).unwrap(),
                //     Time::MIDNIGHT,
                // ) + Duration::from_millis(unix_millis());
                // helper_log
                //     .write(
                //         format!(
                //             "{} | {} {}>\n",
                //             d,
                //             amount_since_log,
                //             amount_since_log * Wrap("=")
                //         )
                //         .as_bytes(),
                //     )
                //     .expect("error writing to log");
                // helper_log.flush().expect("error writing to log");
                // last_log_time = unix_millis();
                // amount_since_log = 0;
                // }
            }
            map.remove(&buf);
        }
    }
}

pub fn sender<F: Fn(f32)>(args: &[String], on_progress: F) {
    let connection = holepunch(args);
    let br = args
        .get(5)
        .map(|s| s.parse::<usize>())
        .unwrap_or(Ok(256))
        .expect("bad bitrate argument");
    let begin = args
        .get(6)
        .map(|s| s.parse::<u64>())
        .unwrap_or(Ok(0))
        .expect("bad begin operand");
    let mut buf: Vec<u8> = Vec::new();
    buf.resize(br, 0);
    let buf = buf.leak();
    let mut file = File::open(args.get(4).unwrap_or_else(|| {
        print_args(args);
        panic!("unreachable")
    }))
    .expect("file not readable");

    if begin != 0 {
        println!("Skipping to {}...", begin);
        file.seek(SeekFrom::Start(begin)).expect("unable to skip");
        println!("Done.");
    }

    let mut sc = SafeReadWrite::new(connection);
    let mut bytes_sent: u64 = 0;
    let mut last_update = unix_millis();
    let len = file.metadata().expect("bad metadata").len();
    sc.write_safe(&len.to_be_bytes())
        .expect("unable to send file length");
    println!("Length: {}", &len);
    let mut time = unix_millis();
    loop {
        let read = file.read(buf).expect("file read error");
        if read == 0 && env::var("QFT_STREAM").is_err() {
            println!();
            println!("Transfer done. Thank you!");
            sc.end();
            return;
        }

        sc.write_safe(&buf[..read]).expect("send error");
        bytes_sent += read as u64;
        if (bytes_sent % (br * 20) as u64) < (br as u64) {
            let elapsed = unix_millis() - time;
            let elapsed = if elapsed == 0 { 1 } else { elapsed };

            print!(
                "\r\x1b[KSent {} bytes; Speed: {} kb/s",
                bytes_sent,
                br * 20 / elapsed as usize
            );
            stdout().flush().unwrap();
            time = unix_millis();
        }
        if unix_millis() - last_update > 100 {
            on_progress((bytes_sent + begin) as f32 / len as f32);
            last_update = unix_millis();
        }
    }
}

pub fn receiver<F: Fn(f32)>(args: &[String], on_progress: F) {
    let connection = holepunch(args);
    let br = args
        .get(5)
        .map(|s| s.parse::<u32>())
        .unwrap_or(Ok(256))
        .expect("bad bitrate argument");
    let begin = args
        .get(6)
        .map(|s| s.parse::<u64>())
        .unwrap_or(Ok(0))
        .expect("bad begin operand");
    let mut buf: Vec<u8> = Vec::new();
    buf.resize(br as usize, 0);
    let buf: &[u8] = buf.leak();
    let mut file = OpenOptions::new()
        .truncate(false)
        .write(true)
        .create(true)
        .open(args.get(4).unwrap_or_else(|| {
            print_args(args);
            panic!("unreachable")
        }))
        .expect("file not writable");

    if begin != 0 {
        println!("Skipping to {}...", begin);
        file.seek(SeekFrom::Start(begin)).expect("unable to skip");
        println!("Done.");
    }

    let mut sc = SafeReadWrite::new(connection);
    let mut bytes_received: u64 = 0;
    let mut last_update = unix_millis();
    let len_bytes = [0; 8];
    let len = sc
        .read_safe(&len_bytes)
        .expect("unable to read length from sender")
        .0;
    let len = u64::from_be_bytes([
        len[0], len[1], len[2], len[3], len[4], len[5], len[6], len[7],
    ]);
    let _ = file.set_len(len);
    println!("Length: {}", &len);
    let mut time = unix_millis();
    loop {
        let (mbuf, amount) = sc.read_safe(buf).expect("read error");
        let buf = &mbuf.leak()[..amount];
        if amount == 0 {
            println!();
            println!("Transfer done. Thank you!");
            return;
        }

        let _ = file
            .write(decompress_size_prepended(buf).unwrap().as_slice())
            .expect("write error");
        file.flush().expect("file flush error");
        bytes_received += amount as u64;
        if (bytes_received % (br * 20) as u64) < (br as u64) {
            let elapsed = unix_millis() - time;
            let elapsed = if elapsed == 0 { 1 } else { elapsed };

            print!(
                "\r\x1b[KReceived {} bytes; Speed: {} kb/s",
                bytes_received,
                br as usize * 20 / elapsed as usize
            );
            stdout().flush().unwrap();
            time = unix_millis();
        }
        if unix_millis() - last_update > 100 {
            on_progress((bytes_received + begin) as f32 / len as f32);
            last_update = unix_millis();
        }
    }
}

fn holepunch(args: &[String]) -> UdpSocket {
    let bind_addr = (Ipv4Addr::from(0), 0);
    let holepunch = UdpSocket::bind(bind_addr).expect("unable to create socket");
    holepunch
        .connect(args.get(2).unwrap_or_else(|| {
            print_args(args);
            panic!("unreachable")
        }))
        .expect("unable to connect to helper");
    let bytes = args
        .get(3)
        .unwrap_or_else(|| {
            print_args(args);
            panic!("unreachable")
        })
        .as_bytes();
    let mut buf = [0; 200];
    buf[..bytes.len().min(200)].copy_from_slice(&bytes[..bytes.len().min(200)]);
    holepunch.send(&buf).expect("unable to talk to helper");
    holepunch
        .recv(&mut buf)
        .expect("unable to receive from helper");
    // buf should now contain our partner's address data.
    let mut s = Vec::from(buf);
    s.retain(|e| *e != 0);
    let bind_addr = String::from_utf8_lossy(s.as_slice()).to_string();
    println!(
        "Holepunching {} (partner) and :{} (you).",
        bind_addr,
        holepunch.local_addr().unwrap().port()
    );
    holepunch
        .connect(SocketAddrV4::from_str(bind_addr.as_str()).unwrap())
        .expect("connection failed");
    holepunch
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    holepunch
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    if env::var("QFT_USE_TIMED_HOLEPUNCH").is_ok() {
        println!("Warning: You are using the QFT_USE_TIMED_HOLEPUNCH environment variable. This won't allow for more \
            backwards-compatibility, rather it only exists as a fallback for bad connections. Please make absolutely \
            sure your partner uses QFT_USE_TIMED_HOLEPUNCH as well, data might otherwise get corrupted on the receiver.");
        println!("Waiting...");
        let mut stop = false;
        while !stop {
            thread::sleep(Duration::from_millis(500 - (unix_millis() % 500)));
            println!("CONNECT {}", unix_millis());
            let _ = holepunch.send(&[0]);
            let result = holepunch.recv(&mut [0, 0]);
            if result.is_ok() && result.unwrap() == 1 {
                holepunch.send(&[0, 0]).expect("connection failed");
                let result = holepunch.recv(&mut [0, 0]);
                if result.is_ok() && result.unwrap() == 2 {
                    stop = true;
                }
            }
        }
    } else {
        println!("Connecting...");
        thread::sleep(Duration::from_millis(500 - (unix_millis() % 500)));
        for _ in 0..40 {
            let m = unix_millis();
            let _ = holepunch.send(&[0]);
            thread::sleep(Duration::from_millis((50 - (unix_millis() - m)).max(0)));
        }
        let mut result = Ok(1);
        while result.is_ok() && result.unwrap() == 1 {
            result = holepunch.recv(&mut [0, 0]);
        }
        holepunch.send(&[0, 0]).expect("connection failed");
        holepunch.send(&[0, 0]).expect("connection failed");
        result = Ok(1);
        while result.is_ok() && result.unwrap() != 2 {
            result = holepunch.recv(&mut [0, 0]);
        }
        result = Ok(1);
        while result.is_ok() && result.unwrap() == 2 {
            result = holepunch.recv(&mut [0, 0]);
        }
    }
    println!("Holepunch and connection successful.");
    holepunch
}

fn print_args(args: &[String]) {
    let f = args.get(0).unwrap();
    println!(
        "No arguments. Needed: \n\
         | {} helper <bind-port>\n\
         | {} sender <helper-address>:<helper-port> <phrase> <filename> [bitrate] [skip]\n\
         | {} receiver <helper-address>:<helper-port> <phrase> <filename> [bitrate] [skip]\n\
         | {} gui\n\
         | {} version\n",
        f, f, f, f, f
    );
    panic!("No arguments");
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
