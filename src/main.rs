use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::{debug, error, info, warn};
use serialport::{DataBits, SerialPort, StopBits, TTYPort};
use std::error::Error;
use std::fs::OpenOptions;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::{io::Read, io::Write, net::SocketAddr};

use env_logger::{Builder, Env, Target};
use prometheus_exporter::prometheus::register_gauge;

mod parser;
use parser::{parser, IpAddr, PanDesc};
mod command;
use command::Command;
mod echonet_lite;

use crate::echonet_lite::{
    EData, EDataFormat1, EDataProperty, EchonetLite, EpcLowVoltageSmartMeter,
    EOJ_HOUSING_LOW_VOLTAGE_SMART_METER,
};
use crate::parser::Response;

#[derive(Debug)]
struct UartReader {
    inner: TTYPort,
    is_closed: Arc<AtomicBool>,
}

#[derive(Debug)]
struct UartWriter {
    inner: TTYPort,
    is_closed: Arc<AtomicBool>,
}

fn split_uart(uart: TTYPort) -> (UartReader, UartWriter) {
    let is_closed = Arc::new(AtomicBool::new(false));
    (
        UartReader {
            inner: uart.try_clone_native().unwrap(),
            is_closed: is_closed.clone(),
        },
        UartWriter {
            inner: uart,
            is_closed,
        },
    )
}

impl Read for UartReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.is_closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "uart writer is disconnected",
            ));
        }
        self.inner.read(buf)
    }
}

impl Drop for UartReader {
    fn drop(&mut self) {
        self.is_closed.store(true, Ordering::Release);
    }
}

impl UartWriter {
    fn send_command(&mut self, cmd: Command) -> Result<(), Box<dyn Error>> {
        debug!("sending command: {:?}", cmd);

        let cmd: Bytes = cmd.into();
        self.write_all(&cmd)?;
        Ok(())
    }
}

impl Write for UartWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.is_closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "uart reader is disconnected",
            ));
        }

        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.is_closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "uart reader is disconnected",
            ));
        }
        self.inner
            .flush()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

impl Drop for UartWriter {
    fn drop(&mut self) {
        self.is_closed.store(true, Ordering::Release);
    }
}

fn active_scan(
    sensor: &mut UartWriter,
    receiver: &mut Receiver<Response>,
) -> Result<PanDesc, Box<dyn Error>> {
    sensor.send_command(Command::ActiveScan { duration: 6 })?;
    let r = receiver.recv()?;
    if !matches!(r, Response::SkScan { .. }) {
        return Err("SKSCAN failed".into());
    }

    let mut tmp = Err("unable to find sensor within duration".into());
    loop {
        let r = receiver.recv()?;
        match r {
            Response::Event { num, sender, param } => {
                if num == 0x22 {
                    return tmp;
                }
            }
            Response::EPanDesc(pandesc) => {
                tmp = Ok(pandesc);
            }
            _ => {}
        }
    }
}

fn wait_for_connect(
    sensor: &mut UartWriter,
    receiver: &mut Receiver<Response>,
) -> Result<(), Box<dyn Error>> {
    let total_wait_time = Duration::from_millis(0);
    loop {
        if total_wait_time > Duration::from_secs(30) {
            return Err("connect timeout".into());
        }

        let r = receiver.recv()?;
        match r {
            Response::Event { num: 0x24, .. } => {
                return Err("failed to connect to PANA".into());
            }
            Response::Event { num: 0x25, .. } => {
                return Ok(());
            }
            _ => {}
        }
    }
}

fn send_initialize_command_sequence(
    writer: &mut UartWriter,
    receiver: &mut Receiver<Response>,
) -> Result<(IpAddr, f64), Box<dyn Error>> {
    // reset
    writer.send_command(Command::SkReset)?;
    let r = receiver.recv()?;
    if !matches!(r, Response::SkReset) {
        return Err("SKRESET failed".into());
    }

    // send id
    writer.send_command(Command::SkSetRbid { id: B_ID })?;
    let r = receiver.recv()?;

    if !matches!(r, Response::SkSetRbid { .. }) {
        return Err("SKSETRBID failed".into());
    }

    // send pw
    writer.send_command(Command::SkSetPwd { pwd: B_PW })?;
    let r = receiver.recv()?;
    if !matches!(r, Response::SkSetPwd { .. }) {
        return Err("SKSETPWD failed".into());
    }

    let pan_desc = active_scan(writer, receiver)?;
    debug!("pan_desc: {:?}", pan_desc);

    // set channel
    writer.send_command(Command::SkSreg {
        sreg: 0x02,
        val: pan_desc.channel as u32,
    })?;
    let r = receiver.recv()?;
    if !matches!(r, Response::SkSreg { .. }) {
        return Err("SKSREG failed".into());
    }

    // set pan id
    writer.send_command(Command::SkSreg {
        sreg: 0x03,
        val: pan_desc.pan_id as u32,
    })?;
    let r = receiver.recv()?;
    if !matches!(r, Response::SkSreg { .. }) {
        return Err("SKSREG failed".into());
    }

    // convert addr
    writer.send_command(Command::SkLl64 {
        addr64: &pan_desc.addr,
    })?;
    let r = receiver.recv()?;
    let ipv6_addr = match r {
        Response::SkLl64 { ipaddr, .. } => ipaddr,
        _ => {
            return Err("SKLL64 failed".into());
        }
    };

    // connect to pana
    writer.send_command(Command::SkJoin { ipaddr: &ipv6_addr })?;
    let r = receiver.recv()?;
    if !matches!(r, Response::SkJoin { .. }) {
        return Err("SKJOIN failed".into());
    }

    wait_for_connect(writer, receiver)?;

    writer.send_command(Command::SendCumulativeEnergyUnitRequeest { ipaddr: &ipv6_addr })?;
    let mut unit = 0.0;
    let total_wait_time = std::time::Instant::now();

    'wait_response: loop {
        if total_wait_time.elapsed() > Duration::from_secs(19) {
            break 'wait_response;
        }

        let r = receiver.recv()?;
        match r {
            Response::SkSendTo { result: 0x00, .. } => {}
            Response::SkSendTo { result: _, .. } => {
                return Err("Send cumulative energy unit request failed".into());
            }
            Response::ERxUdp {
                data:
                    EchonetLite {
                        edata:
                            EData::EDataFormat1(EDataFormat1 {
                                seoj: EOJ_HOUSING_LOW_VOLTAGE_SMART_METER,
                                props,
                                ..
                            }),
                        ..
                    },
                ..
            } => {
                for prop in props {
                    match prop {
                        EDataProperty {
                            epc: EpcLowVoltageSmartMeter::CUMULATIVE_ENERGY_UNIT,
                            pdc: 0x01,
                            mut edt,
                            ..
                        } => {
                            let unit_data = edt.get_u8();
                            unit = match unit_data {
                                0x0 => 1.0,
                                0x1 => 0.1,
                                0x2 => 0.01,
                                0x3 => 0.001,
                                0x4 => 0.0001,
                                0xa => 10.0,
                                0xb => 100.0,
                                0xc => 1000.0,
                                0xd => 10000.0,
                                _ => {
                                    return Err("Invalid cumulative energy unit".into());
                                }
                            };
                        }
                        _ => {
                            // ignore
                        }
                    }
                }
            }
            _ => {
            }
        }
    }

    if unit == 0.0 {
        return Err("Get cumulative energy unit failed".into());
    }

    Ok((ipv6_addr, unit))
}

// # cancellation
// It is caller responsibility to ensure that the previous reader thread closes before calling initialize again.
// By dropping thre writer, reader.read() will get error and then the reader thread closes.
// Note that reader.read() yield something no later than reader timeout set by uart.set_read_mode().
// So, if you drop the writer, you can successfully join the reader thread within the timeout.
fn initialize(
) -> Result<(UartWriter, Receiver<Response>, IpAddr, JoinHandle<()>, f64), Box<dyn Error>> {
    let mut uart =
        TTYPort::open(&serialport::new("/dev/ttyO1", 115200)).expect("Failed to open serial port");
    uart.set_parity(serialport::Parity::None)?;
    uart.set_data_bits(DataBits::Eight)?;
    uart.set_stop_bits(StopBits::One)?;
    uart.set_timeout(Duration::from_millis(5000))?;

    let (sender, mut receiver) = channel();
    let (mut reader, mut writer) = split_uart(uart);

    let handle = std::thread::spawn(move || {
        let mut buf = BytesMut::with_capacity(1024);
        loop {
            let mut b = [0; 1024];

            match reader.read(&mut b) {
                Ok(n) if n > 0 => {
                    debug!("read: {:?}", &b[..n]);
                    buf.put(&b[..n]);
                }
                Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                    sender.send(Response::UartTimeOut).unwrap();
                    continue;
                }
                Err(e) => {
                    error!("uart read error: {:?}", e);
                    break;
                }
                _ => {}
            }

            debug!("current buf: {:?}", buf);
            match parser(&buf) {
                Ok((rest, line)) => {
                    debug!("parsed response: {:?}", line);
                    sender.send(line).unwrap();

                    buf = BytesMut::from(rest);
                }
                Err(nom::Err::Incomplete(n)) => {
                    // not enough data
                    debug!("parse incomplate: {:?}", n);
                }
                Err(e) => {
                    error!("parse error: {:?}", e);

                    // finish reading from device
                    break;
                }
            }
        }
        // explicitly drop sender, so that receiver.recv() will return Err
        drop(sender);
    });

    let (ipv6_addr, unit) = match send_initialize_command_sequence(&mut writer, &mut receiver) {
        Ok(ipv6_addr) => ipv6_addr,
        Err(e) => {
            drop(writer);
            handle.join().expect("failed to join the reader thread");
            return Err(e);
        }
    };

    Ok((writer, receiver, ipv6_addr, handle, unit))
}

const B_ID: &str = std::env!("B_ID");
const B_PW: &str = std::env!("B_PW");

fn main() -> Result<(), Box<dyn Error>> {
    let env = Env::default().default_filter_or("debug");
    let mut builder = Builder::from_env(env);

    if let Ok(dest) = std::env::var("RUST_LOG_DESTINATION") {
        if dest == "file" {
            let file = OpenOptions::new()
                .append(true)
                .create(true)
                .open("/var/log/smartmeter-exporter/smartmeter-exporter.log")?;
            builder.target(Target::Pipe(Box::new(file)));
        }
    }
    builder.init();

    let addr_raw = "0.0.0.0:9186";
    let addr: SocketAddr = addr_raw.parse().expect("can not parse listen addr");

    let exporter = prometheus_exporter::start(addr).expect("can not start exporter");
    let duration = std::time::Duration::from_millis(10000);

    let counter_error_initialize = register_gauge!(
        "counter_error_initialize",
        "# of error when try to initialize sensor with PANA"
    )
    .expect("can not create gauge counter_error_initialize");
    let counter_error_sksendto = register_gauge!(
        "counter_error_sksendto",
        "# of error when sending data to sensor"
    )
    .expect("can not create gauge counter_error_sksendto");
    let counter_success_initialize = register_gauge!(
        "counter_success_initialize",
        "# of times client finished initialization"
    )
    .expect("can not create gauge counter_success_initialize");
    let counter_request_energy = register_gauge!(
        "counter_request_energy",
        "# of times client send energy request"
    )
    .expect("can not create gauge counter_request_energy");
    let instantaneous_energy =
        register_gauge!("instantaneous_energy", "Current Power Consumption in Watt")
            .expect("can not create gauge instantaneous_energy");
    let cumulative_energy =
        register_gauge!("cumulative_energy", "Cumulative Power Consumption in Watt")
            .expect("can not create gauge cumulative_energy");

    loop {
        let (mut writer, mut receiver, ipv6_addr, handle, cumulative_energy_unit) =
            match initialize() {
                Ok(ipv6_addr) => ipv6_addr,
                Err(e) => {
                    error!("unable to initialize smartmeter: {:?}", e);
                    std::thread::sleep(Duration::from_secs(30));
                    counter_error_initialize.inc();
                    continue;
                }
            };
        counter_success_initialize.inc();
        info!("initialize completed");

        // main loop
        'main: loop {
            let _guard = exporter.wait_duration(duration);
            if let Err(e) = writer.send_command(Command::SendEnergyRequest { ipaddr: &ipv6_addr }) {
                error!("failed to send command: {:?}", e);
                counter_error_sksendto.inc();
                break 'main;
            }
            counter_request_energy.inc();
            let total_wait_time = std::time::Instant::now();

            // wait response for energy request
            'wait_response: loop {
                if total_wait_time.elapsed() > Duration::from_secs(19) {
                    break 'wait_response;
                }

                let r = match receiver.recv() {
                    Ok(r) => r,
                    Err(e) => {
                        error!("reader thread closed when they encouter error: {:?}", e);
                        break 'main;
                    }
                };
                info!("got response {:?}", r);

                match r {
                    Response::SkSendTo { result: 0x00, .. } => {
                        debug!("send energy request success");
                    }
                    Response::SkSendTo { result: _, .. } => {
                        warn!("failed to send energy request: {:?}", r);
                        counter_error_sksendto.inc();
                        break 'wait_response;
                    }
                    Response::ERxUdp {
                        data:
                            EchonetLite {
                                edata:
                                    EData::EDataFormat1(EDataFormat1 {
                                        seoj: EOJ_HOUSING_LOW_VOLTAGE_SMART_METER,
                                        props,
                                        ..
                                    }),
                                ..
                            },
                        ..
                    } => {
                        for prop in props {
                            match prop {
                                EDataProperty {
                                    epc: EpcLowVoltageSmartMeter::INSTANTANEOUS_ENERGY,
                                    pdc: 0x04,
                                    mut edt,
                                    ..
                                } => {
                                    let power = edt.get_u32();
                                    instantaneous_energy.set(power as f64);
                                }
                                EDataProperty {
                                    epc: EpcLowVoltageSmartMeter::CUMULATIVE_ENERGY_FIXED_TIME_NORMAL_DIRECTION,
                                    pdc: 0x0b,
                                    mut edt,
                                    ..
                                } => {
                                    let power = edt.slice(7..11).get_u32();
                                    cumulative_energy.set((power as f64 )*cumulative_energy_unit);
                                }
                                _ => {
                                    // ignore
                                }
                            }
                        }
                        break 'wait_response;
                    }
                    _ => {
                        // ignore
                    }
                }
            }
        }
        drop(writer);
        handle.join().expect("failed to join the reader thread");
    }
}
