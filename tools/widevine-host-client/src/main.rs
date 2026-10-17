//! Scarlet-native client for widevine-host (W1 of the Widevine track).
//!
//! Creates the shared memory region and a socket pair, spawns widevine-host in
//! the linux-aarch64 view with both handles mapped to fixed descriptor
//! numbers, and drives the control protocol until it has read the CDM's
//! licence request out of shared memory.
//!
//! The native browser never loads the CDM: this process only speaks the
//! protocol defined in ../widevine-host/protocol.h.

use std::io::{Read, Write};

use scarlet_os::environment::{Environment, HandleMapping};
use scarlet_os::handle::{Handle, HandleError, HandleResult};
use scarlet_sys::{Syscall, syscall1};
use scarlet_os::handle::capability::memory_mapping::{flags as map_flags, prot};
use scarlet_os::ipc::{permissions, SharedMemory};
use scarlet_os::socket::Socket;

const WVH_MAGIC: u32 = 0x3156_4857;
const WVH_VERSION: u32 = 1;

const WVH_CMD_HELLO: u32 = 1;
const WVH_CMD_INIT: u32 = 2;
const WVH_CMD_CREATE_SESSION: u32 = 3;
const WVH_CMD_SHUTDOWN: u32 = 8;
const WVH_CMD_RELEASE: u32 = 9;

const WVH_EV_READY: u32 = 100;
const WVH_EV_RESULT: u32 = 101;
const WVH_EV_SESSION: u32 = 102;
const WVH_EV_MESSAGE: u32 = 103;
const WVH_EV_KEYS: u32 = 104;
const WVH_EV_EXPIRATION: u32 = 105;
const WVH_EV_SESSION_CLOSED: u32 = 106;

const WVH_OK: i32 = 0;
const WVH_SESSION_TEMPORARY: u32 = 0;
const WVH_INIT_DATA_CENC: u32 = 0;
const WVH_SHM_HEADER_BYTES: usize = 4096;

const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21,
    0xed,
];

struct Options {
    host_path: String,
    cdm_path: String,
    host_ld_path: Option<String>,
    key_id: [u8; 16],
    slot_count: u32,
    slot_size: u32,
}

fn usage() -> ! {
    eprintln!(
        "usage: widevine-host-client --host PATH --cdm PATH [--host-ld-path DIR] [--kid-hex HEX] [--slots N] [--slot-size BYTES]"
    );
    std::process::exit(2);
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_hex_16(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for index in 0..16 {
        let high = hex_digit(bytes[index * 2])?;
        let low = hex_digit(bytes[index * 2 + 1])?;
        out[index] = (high << 4) | low;
    }
    Some(out)
}

fn parse_arguments() -> Options {
    let args: Vec<String> = std::env::args().collect();
    let mut host_path = String::new();
    let mut cdm_path = String::new();
    let mut host_ld_path: Option<String> = None;
    let mut key_id = [0u8; 16];
    let mut slot_count = 4u32;
    let mut slot_size = 1024u32 * 1024u32;
    let mut index = 1;
    while index < args.len() {
        let argument = args[index].as_str();
        let value = || -> String { args.get(index + 1).cloned().unwrap_or_else(|| usage()) };
        match argument {
            "--host" => {
                host_path = value();
                index += 2;
            }
            "--cdm" => {
                cdm_path = value();
                index += 2;
            }
            "--host-ld-path" => {
                host_ld_path = Some(value());
                index += 2;
            }
            "--kid-hex" => {
                key_id = parse_hex_16(&value()).unwrap_or_else(|| {
                    eprintln!("invalid --kid-hex: expected 16 bytes");
                    std::process::exit(2);
                });
                index += 2;
            }
            "--slots" => {
                slot_count = value().parse().unwrap_or_else(|_| usage());
                index += 2;
            }
            "--slot-size" => {
                slot_size = value().parse().unwrap_or_else(|_| usage());
                index += 2;
            }
            _ => usage(),
        }
    }
    if host_path.is_empty() || cdm_path.is_empty() {
        usage();
    }
    Options {
        host_path,
        cdm_path,
        host_ld_path,
        key_id,
        slot_count,
        slot_size,
    }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32_be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn take_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
    if *offset + 4 > data.len() {
        return None;
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&data[*offset..*offset + 4]);
    *offset += 4;
    Some(u32::from_le_bytes(bytes))
}

fn take_string(data: &[u8], offset: &mut usize) -> Option<String> {
    let size = take_u32(data, offset)? as usize;
    if *offset + size > data.len() {
        return None;
    }
    let text = String::from_utf8_lossy(&data[*offset..*offset + size]).into_owned();
    *offset += size;
    Some(text)
}

fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    push_u32(&mut out, WVH_MAGIC);
    push_u32(&mut out, kind);
    push_u32(&mut out, 0);
    push_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(payload);
    out
}

fn make_pssh(key_id: &[u8; 16]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(18);
    payload.push(0x0a);
    payload.push(0x10);
    payload.extend_from_slice(key_id);
    let box_size = 4 + 4 + 4 + 16 + 4 + payload.len();
    let mut out = Vec::with_capacity(box_size);
    push_u32_be(&mut out, box_size as u32);
    out.extend_from_slice(b"pssh");
    push_u32_be(&mut out, 0);
    out.extend_from_slice(&WIDEVINE_SYSTEM_ID);
    push_u32_be(&mut out, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

fn duplicate_stdio(raw: usize) -> HandleResult<Handle> {
    // SAFETY: duplication borrows the standard descriptor for this call only.
    let duplicate = unsafe { syscall1(Syscall::HandleDuplicate, raw) };
    if duplicate == usize::MAX {
        return Err(HandleError::SystemError(-1));
    }
    // SAFETY: the successful syscall returned a new, exclusively owned handle.
    unsafe { Handle::from_raw(duplicate as i32) }
}

#[derive(Default)]
struct Collected {
    session_id: Option<String>,
    message: Option<(u32, u32, u32, u32)>,
}

fn read_frame(socket: &mut Socket) -> std::io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 16];
    socket.read_exact(&mut header)?;
    let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let kind = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let length = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    if magic != WVH_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad frame magic",
        ));
    }
    let mut payload = vec![0u8; length];
    if length > 0 {
        socket.read_exact(&mut payload)?;
    }
    Ok((kind, payload))
}

fn handle_event(kind: u32, payload: &[u8], collected: &mut Collected) {
    let mut offset = 0usize;
    match kind {
        WVH_EV_SESSION => {
            let request = take_u32(payload, &mut offset).unwrap_or(0);
            let session = take_string(payload, &mut offset).unwrap_or_default();
            println!("WVHC_EVENT session request={request} id={session}");
            collected.session_id = Some(session);
        }
        WVH_EV_MESSAGE => {
            let session = take_string(payload, &mut offset).unwrap_or_default();
            let message_type = take_u32(payload, &mut offset).unwrap_or(0);
            let slot = take_u32(payload, &mut offset).unwrap_or(0);
            let message_offset = take_u32(payload, &mut offset).unwrap_or(0);
            let size = take_u32(payload, &mut offset).unwrap_or(0);
            println!(
                "WVHC_EVENT message session={session} type={message_type} slot={slot} size={size}"
            );
            collected.message = Some((message_type, slot, message_offset, size));
        }
        WVH_EV_KEYS => println!("WVHC_EVENT keys"),
        WVH_EV_EXPIRATION => println!("WVHC_EVENT expiration"),
        WVH_EV_SESSION_CLOSED => println!("WVHC_EVENT session-closed"),
        other => println!("WVHC_EVENT unknown kind={other}"),
    }
}

fn wait_result(
    socket: &mut Socket,
    request_id: u32,
    collected: &mut Collected,
) -> std::io::Result<(i32, u32, u32)> {
    loop {
        let (kind, payload) = read_frame(socket)?;
        if kind != WVH_EV_RESULT {
            handle_event(kind, &payload, collected);
            continue;
        }
        let mut offset = 0usize;
        let id = take_u32(&payload, &mut offset).unwrap_or(0);
        let status = take_u32(&payload, &mut offset).unwrap_or(0) as i32;
        let value0 = take_u32(&payload, &mut offset).unwrap_or(0);
        let value1 = take_u32(&payload, &mut offset).unwrap_or(0);
        if id != request_id {
            println!("WVHC_EVENT result-for-other request={id}");
            continue;
        }
        return Ok((status, value0, value1));
    }
}

fn wait_message(socket: &mut Socket, collected: &mut Collected) -> std::io::Result<()> {
    while collected.message.is_none() {
        let (kind, payload) = read_frame(socket)?;
        if kind == WVH_EV_RESULT {
            continue;
        }
        handle_event(kind, &payload, collected);
    }
    Ok(())
}

fn main() {
    let options = parse_arguments();
    let region_size =
        WVH_SHM_HEADER_BYTES + options.slot_count as usize * options.slot_size as usize;

    let memory = match SharedMemory::create(region_size, permissions::READ_WRITE) {
        Ok(memory) => memory,
        Err(error) => {
            eprintln!("shared memory create failed: {error:?}");
            std::process::exit(3);
        }
    };
    let mapper = match memory.as_handle().as_memory_mapping() {
        Ok(mapper) => mapper,
        Err(error) => {
            eprintln!("memory mapping capability failed: {error:?}");
            std::process::exit(3);
        }
    };
    // SAFETY: the region is created above, nothing else maps this address range,
    // and every access below stays inside the mapped length.
    let base = match unsafe {
        mapper.mmap(
            0,
            region_size,
            prot::READ | prot::WRITE,
            map_flags::SHARED,
            0,
        )
    } {
        Ok(base) => base,
        Err(_) => {
            eprintln!("shared memory mmap failed");
            std::process::exit(3);
        }
    };
    println!(
        "WVHC_REGION size={region_size} slots={} slot_size={}",
        options.slot_count, options.slot_size
    );

    let (mut client_socket, host_socket) = match Socket::pair() {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("socketpair failed: {error:?}");
            std::process::exit(4);
        }
    };

    let environment = match Environment::current() {
        Ok(environment) => environment,
        Err(error) => {
            eprintln!("Environment::current failed: {error:?}");
            std::process::exit(5);
        }
    };
    let view = match environment.root("linux-aarch64") {
        Ok(view) => view,
        Err(error) => {
            eprintln!("linux-aarch64 view missing: {error:?}");
            std::process::exit(5);
        }
    };
    let executable = match view.open(&options.host_path, 0) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!(
                "cannot open {} in the Linux view: {error:?}",
                options.host_path
            );
            std::process::exit(5);
        }
    };

    let host_argv = [
        "widevine-host",
        "--cdm",
        options.cdm_path.as_str(),
        "--fd",
        "3",
        "--shm-fd",
        "4",
        "--verbose",
    ];
    let stdin = duplicate_stdio(0).unwrap_or_else(|error| {
        eprintln!("stdin duplication failed: {error:?}");
        std::process::exit(6);
    });
    let stdout = duplicate_stdio(1).unwrap_or_else(|error| {
        eprintln!("stdout duplication failed: {error:?}");
        std::process::exit(6);
    });
    let stderr = duplicate_stdio(2).unwrap_or_else(|error| {
        eprintln!("stderr duplication failed: {error:?}");
        std::process::exit(6);
    });
    let mappings = [
        HandleMapping {
            source: &stdin,
            target: 0,
        },
        HandleMapping {
            source: &stdout,
            target: 1,
        },
        HandleMapping {
            source: &stderr,
            target: 2,
        },
        HandleMapping {
            source: host_socket.as_handle(),
            target: 3,
        },
        HandleMapping {
            source: memory.as_handle(),
            target: 4,
        },
    ];
    let mut host_env_owned: Vec<String> = Vec::new();
    if let Some(dir) = &options.host_ld_path {
        host_env_owned.push(format!("LD_LIBRARY_PATH={dir}"));
    }
    let host_envp: Vec<&str> = host_env_owned.iter().map(String::as_str).collect();
    let pid = match environment.spawn_with_abi(
        "linux-aarch64",
        &executable,
        &host_argv,
        &host_envp,
        "/",
        &mappings,
    ) {
        Ok(pid) => pid,
        Err(error) => {
            eprintln!("spawn failed: {error:?}");
            std::process::exit(6);
        }
    };
    println!("WVHC_SPAWN pid={pid}");
    drop(host_socket);

    let hello = frame(WVH_CMD_HELLO, &WVH_VERSION.to_le_bytes());
    if client_socket.write_all(&hello).is_err() {
        eprintln!("HELLO write failed");
        std::process::exit(7);
    }
    let mut collected = Collected::default();
    let (kind, payload) = match read_frame(&mut client_socket) {
        Ok(frame) => frame,
        Err(error) => {
            eprintln!("READY read failed: {error}");
            std::process::exit(7);
        }
    };
    if kind != WVH_EV_READY {
        eprintln!("unexpected first event kind={kind}");
        std::process::exit(7);
    }
    let mut offset = 0usize;
    let host_version = take_u32(&payload, &mut offset).unwrap_or(0);
    let header_bytes = take_u32(&payload, &mut offset).unwrap_or(0);
    let slot_count = take_u32(&payload, &mut offset).unwrap_or(0);
    let slot_size = take_u32(&payload, &mut offset).unwrap_or(0);
    println!(
        "WVHC_READY version={host_version} header={header_bytes} slots={slot_count} slot_size={slot_size}"
    );

    let mut init = Vec::new();
    push_u32(&mut init, 0);
    push_u32(&mut init, 0);
    push_u32(&mut init, 0);
    if client_socket.write_all(&frame(WVH_CMD_INIT, &init)).is_err() {
        eprintln!("INIT write failed");
        std::process::exit(7);
    }
    match wait_result(&mut client_socket, 0, &mut collected) {
        Ok((status, _, _)) => {
            println!("WVHC_INIT status={status}");
            if status != WVH_OK {
                std::process::exit(8);
            }
        }
        Err(error) => {
            eprintln!("INIT reply failed: {error}");
            std::process::exit(8);
        }
    }

    let init_data = make_pssh(&options.key_id);
    let mut create = Vec::new();
    push_u32(&mut create, 1);
    push_u32(&mut create, WVH_SESSION_TEMPORARY);
    push_u32(&mut create, WVH_INIT_DATA_CENC);
    push_u32(&mut create, init_data.len() as u32);
    create.extend_from_slice(&init_data);
    if client_socket
        .write_all(&frame(WVH_CMD_CREATE_SESSION, &create))
        .is_err()
    {
        eprintln!("CREATE_SESSION write failed");
        std::process::exit(7);
    }
    match wait_result(&mut client_socket, 1, &mut collected) {
        Ok((status, _, _)) => {
            println!("WVHC_CREATE_SESSION status={status}");
            if status != WVH_OK {
                std::process::exit(9);
            }
        }
        Err(error) => {
            eprintln!("CREATE_SESSION reply failed: {error}");
            std::process::exit(9);
        }
    }
    if let Err(error) = wait_message(&mut client_socket, &mut collected) {
        eprintln!("message event failed: {error}");
        std::process::exit(9);
    }

    if let Some((message_type, slot, message_offset, size)) = collected.message {
        let start =
            WVH_SHM_HEADER_BYTES + slot as usize * options.slot_size as usize + message_offset as usize;
        if start + size as usize <= region_size {
            // SAFETY: start + size was checked against the mapped region above.
            let bytes =
                unsafe { std::slice::from_raw_parts((base + start) as *const u8, size as usize) };
            let head: Vec<String> = bytes.iter().take(4).map(|byte| byte.to_string()).collect();
            println!(
                "WVHC_MESSAGE type={message_type} slot={slot} size={size} head={}",
                head.join(",")
            );
        } else {
            eprintln!("message outside the mapped region");
            std::process::exit(9);
        }
        let mut release = Vec::new();
        push_u32(&mut release, slot);
        let _ = client_socket.write_all(&frame(WVH_CMD_RELEASE, &release));
    }

    let _ = client_socket.write_all(&frame(WVH_CMD_SHUTDOWN, &[]));
    println!("WVHC_OK");
}
