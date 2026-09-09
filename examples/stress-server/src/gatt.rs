//! GATT application: the STRESS service and its six characteristics.
//!
//! Protocol reference: ../stress-protocol.md (shared with examples/stress-client).

use std::{sync::Arc, time::Duration};

use bluer::{
    gatt::local::{
        Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
        CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, ReqError, Service,
    },
    Address,
};
use futures::FutureExt;
use tokio::{
    sync::mpsc,
    time::{interval, sleep, MissedTickBehavior},
};
use uuid::{uuid, Uuid};

use crate::control::{log, ServerState, StormResult};

pub const SERVICE_UUID: Uuid = uuid!("b1ec5747-0000-4000-8000-000000000000");
pub const ECHO_UUID: Uuid = uuid!("b1ec5747-0001-4000-8000-000000000000");
pub const COUNTER_UUID: Uuid = uuid!("b1ec5747-0002-4000-8000-000000000000");
pub const LARGE_UUID: Uuid = uuid!("b1ec5747-0003-4000-8000-000000000000");
pub const CONTROL_UUID: Uuid = uuid!("b1ec5747-0004-4000-8000-000000000000");
pub const STATUS_UUID: Uuid = uuid!("b1ec5747-0005-4000-8000-000000000000");
pub const REPORT_UUID: Uuid = uuid!("b1ec5747-0006-4000-8000-000000000000");

/// Build the GATT application. Every callback holds an `Arc` to the shared state.
pub fn build_app(state: Arc<ServerState>) -> Application {
    Application {
        services: vec![Service {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![
                echo(state.clone()),
                counter(state.clone()),
                large(state.clone()),
                control(state.clone()),
                status(state.clone()),
                report(state),
            ],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Slice a full value according to the ATT read offset (`BlueZ` long-read semantics).
fn apply_offset(value: &[u8], offset: u16) -> Result<Vec<u8>, ReqError> {
    let offset = usize::from(offset);
    if offset > value.len() {
        return Err(ReqError::InvalidOffset);
    }
    Ok(value[offset..].to_vec())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// -------------------------------------------------------------------------------------------
// ECHO: write -> notification [seq u32 LE][payload]
// -------------------------------------------------------------------------------------------

fn echo(state: Arc<ServerState>) -> Characteristic {
    let write_state = state.clone();
    let notify_state = state;
    Characteristic {
        uuid: ECHO_UUID,
        write: Some(CharacteristicWrite {
            write: true,
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new(move |payload, req| {
                let state = write_state.clone();
                async move {
                    let addr = req.device_address;
                    state.note_request_from(addr).await;
                    if state.take_fail() {
                        log(
                            "fail",
                            format!("ECHO write from {addr} len={}", payload.len()),
                        );
                        return Err(ReqError::Failed);
                    }
                    state.record_echo_write();
                    state.slow_delay().await;
                    let seq = state.next_echo_seq();
                    let mut out = seq.to_le_bytes().to_vec();
                    out.extend_from_slice(&payload);
                    let notified = state.echo_notify(out).await;
                    log(
                        "echo",
                        format!("{addr} seq={seq} len={} notified={notified}", payload.len()),
                    );
                    Ok(())
                }
                .boxed()
            })),
            ..Default::default()
        }),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new(move |notifier| {
                let state = notify_state.clone();
                async move {
                    let id = state.next_session_id();
                    let stopped = notifier.stopped();
                    state.set_echo_notifier(Some((id, notifier))).await;
                    log("info", format!("ECHO notify session {id} start"));
                    tokio::spawn(async move {
                        stopped.await;
                        state.clear_echo_notifier(id).await;
                        log("info", format!("ECHO notify session {id} stop"));
                    });
                }
                .boxed()
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// -------------------------------------------------------------------------------------------
// COUNTER: periodic [seq u32 LE] notifications + storms
// -------------------------------------------------------------------------------------------

fn counter(state: Arc<ServerState>) -> Characteristic {
    Characteristic {
        uuid: COUNTER_UUID,
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new(move |mut notifier| {
                let state = state.clone();
                async move {
                    tokio::spawn(async move {
                        let id = state.next_session_id();
                        let (tx, mut rx) = mpsc::unbounded_channel();
                        state.set_counter_tx(Some((id, tx))).await;
                        let period = state.config.counter_interval;
                        log(
                            "counter_start",
                            format!("session={id} interval={}ms", period.as_millis()),
                        );

                        let mut seq: u32 = 0;
                        let mut ticker = interval(period);
                        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        let stopped = notifier.stopped();
                        tokio::pin!(stopped);

                        loop {
                            tokio::select! {
                                () = &mut stopped => break,
                                _ = ticker.tick() => {
                                    if notifier.notify(seq.to_le_bytes().to_vec()).await.is_err() {
                                        break;
                                    }
                                    seq = seq.wrapping_add(1);
                                }
                                Some(storm) = rx.recv() => {
                                    log(
                                        "storm",
                                        format!(
                                            "start count={} interval={}ms seq={seq}",
                                            storm.count,
                                            storm.interval.as_millis()
                                        ),
                                    );
                                    let mut sent = 0u32;
                                    let mut error = None;
                                    for _ in 0..storm.count {
                                        if notifier.is_stopped() {
                                            error = Some("session stopped".to_string());
                                            break;
                                        }
                                        if let Err(err) = notifier.notify(seq.to_le_bytes().to_vec()).await {
                                            error = Some(err.to_string());
                                            break;
                                        }
                                        seq = seq.wrapping_add(1);
                                        sent += 1;
                                        sleep(storm.interval).await;
                                    }
                                    log("storm", format!("done sent={sent} seq={seq}"));
                                    let _ = storm.done.send(StormResult { sent, error });
                                    ticker.reset();
                                }
                            }
                        }

                        state.clear_counter_tx(id).await;
                        log("counter_stop", format!("session={id} seq={seq}"));
                    });
                }
                .boxed()
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// -------------------------------------------------------------------------------------------
// LARGE: read returns `size` bytes, byte i == i & 0xff
// -------------------------------------------------------------------------------------------

fn large(state: Arc<ServerState>) -> Characteristic {
    Characteristic {
        uuid: LARGE_UUID,
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new(move |req| {
                let state = state.clone();
                async move {
                    let addr = req.device_address;
                    state.note_request_from(addr).await;
                    if state.take_fail() {
                        log(
                            "fail",
                            format!("LARGE read from {addr} offset={}", req.offset),
                        );
                        return Err(ReqError::Failed);
                    }
                    state.slow_delay().await;
                    let size = state.config.large_size;
                    let value: Vec<u8> = (0..size)
                        .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
                        .collect();
                    let out = apply_offset(&value, req.offset)?;
                    // A long read arrives as several offset reads; count the read once.
                    if req.offset == 0 {
                        state.record_large_read_ok();
                    }
                    log(
                        "read",
                        format!(
                            "LARGE {addr} offset={} mtu={} len={}",
                            req.offset,
                            req.mtu,
                            out.len()
                        ),
                    );
                    Ok(out)
                }
                .boxed()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// -------------------------------------------------------------------------------------------
// CONTROL: client -> server chaos requests
// -------------------------------------------------------------------------------------------

fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn control(state: Arc<ServerState>) -> Characteristic {
    Characteristic {
        uuid: CONTROL_UUID,
        write: Some(CharacteristicWrite {
            write: true,
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new(move |payload, req| {
                let state = state.clone();
                async move {
                    let addr = req.device_address;
                    state.note_request_from(addr).await;
                    // Script start must use the receipt time as t0, so run it before
                    // any slow delay; the other commands are delayed like every write.
                    if matches!(payload.first(), Some(0x10 | 0x11)) {
                        handle_control(&state, addr, &payload).await;
                        return Ok(());
                    }
                    state.slow_delay().await;
                    handle_control(&state, addr, &payload).await;
                    // Malformed or failing control writes are never an error to the client.
                    Ok(())
                }
                .boxed()
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn handle_control(state: &Arc<ServerState>, from: Address, data: &[u8]) {
    let invalid = || log("control_invalid", format!("{from} {}", hex(data)));
    let result = match (data.first(), data.len()) {
        (Some(0x01), 3) => {
            let delay = Duration::from_millis(u64::from(u16_le(&data[1..])));
            log(
                "control",
                format!("{from} drop after {}ms", delay.as_millis()),
            );
            let state = state.clone();
            tokio::spawn(async move {
                sleep(delay).await;
                if let Err(err) = state.drop_link(Some(from)).await {
                    log("error", format!("drop failed: {err:#}"));
                }
            });
            Ok(())
        }
        (Some(0x02), 2) => {
            log("control", format!("{from} hide {}s", data[1]));
            state.hide(u64::from(data[1])).await
        }
        (Some(0x03), 5) => {
            let count = u32::from(u16_le(&data[1..3]));
            let interval = Duration::from_millis(u64::from(u16_le(&data[3..5])));
            log(
                "control",
                format!(
                    "{from} storm count={count} interval={}ms",
                    interval.as_millis()
                ),
            );
            state.storm(count, interval).await.map(drop)
        }
        (Some(0x04), 3) => {
            let ms = u16_le(&data[1..]);
            log("control", format!("{from} slow {ms}ms"));
            state.set_slow(ms);
            Ok(())
        }
        (Some(0x05), 2) => {
            log("control", format!("{from} restart {}s", data[1]));
            state.restart(u64::from(data[1])).await.map(drop)
        }
        (Some(0x06), 2) => {
            log("control", format!("{from} fail {}", data[1]));
            state.fail_next(data[1]);
            Ok(())
        }
        (Some(0x10), 2) if (1..=7).contains(&data[1]) => {
            log("control", format!("{from} script {}", data[1]));
            state.script.start(state, data[1])
        }
        (Some(0x11), 1) => {
            log("control", format!("{from} abort"));
            if let Err(err) = state.script.abort() {
                log("info", format!("abort: {err}"));
            }
            Ok(())
        }
        _ => {
            invalid();
            Ok(())
        }
    };
    if let Err(err) = result {
        log("error", format!("control action failed: {err:#}"));
    }
}

// -------------------------------------------------------------------------------------------
// STATUS: read returns JSON
// -------------------------------------------------------------------------------------------

fn status(state: Arc<ServerState>) -> Characteristic {
    Characteristic {
        uuid: STATUS_UUID,
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new(move |req| {
                let state = state.clone();
                async move {
                    let addr = req.device_address;
                    state.note_request_from(addr).await;
                    if state.take_fail() {
                        log(
                            "fail",
                            format!("STATUS read from {addr} offset={}", req.offset),
                        );
                        return Err(ReqError::Failed);
                    }
                    state.slow_delay().await;
                    let json = state.status_json();
                    let out = apply_offset(json.as_bytes(), req.offset)?;
                    log(
                        "read",
                        format!("STATUS {addr} offset={} len={}", req.offset, out.len()),
                    );
                    Ok(out)
                }
                .boxed()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// -------------------------------------------------------------------------------------------
// REPORT: read returns the script report JSON (never fails, never slowed)
// -------------------------------------------------------------------------------------------

fn report(state: Arc<ServerState>) -> Characteristic {
    Characteristic {
        uuid: REPORT_UUID,
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new(move |req| {
                let state = state.clone();
                async move {
                    let addr = req.device_address;
                    state.note_request_from(addr).await;
                    let json = state.script.report_json();
                    let out = apply_offset(json.as_bytes(), req.offset)?;
                    log(
                        "read",
                        format!("REPORT {addr} offset={} len={}", req.offset, out.len()),
                    );
                    Ok(out)
                }
                .boxed()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
