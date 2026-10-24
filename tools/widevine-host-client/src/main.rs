//! Native diagnostic CLI for the same CDM host client used by WebKit.
use cdm_host::{CdmHost, CdmHostOptions, HostEvent, INIT_DATA_CENC, SESSION_TEMPORARY};

const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];

struct Options {
    host_path: String,
    cdm_path: String,
    host_ld_path: Option<String>,
    key_id: [u8; 16],
    slot_count: u32,
    slot_size: u32,
    sessions: u32,
}

fn usage() -> ! {
    eprintln!(
        "usage: widevine-host-client --host PATH --cdm PATH [--host-ld-path DIR] [--kid-hex HEX] [--slots N] [--slot-size BYTES] [--sessions N]"
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
    let mut sessions = 1u32;
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
            "--sessions" => {
                sessions = value().parse().unwrap_or_else(|_| usage());
                if sessions == 0 || sessions > 100 {
                    usage();
                }
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
        sessions,
    }
}

fn make_pssh(key_id: &[u8; 16]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(18);
    payload.push(0x0a);
    payload.push(0x10);
    payload.extend_from_slice(key_id);
    let box_size = 4 + 4 + 4 + 16 + 4 + payload.len();
    let mut out = Vec::with_capacity(box_size);
    out.extend_from_slice(&(box_size as u32).to_be_bytes());
    out.extend_from_slice(b"pssh");
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&WIDEVINE_SYSTEM_ID);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

fn run(options: Options) -> cdm_host::Result<()> {
    let mut config = CdmHostOptions::new(options.host_path, options.cdm_path);
    config.host_ld_path = options.host_ld_path;
    config.slot_count = options.slot_count;
    config.slot_size = options.slot_size;
    let mut host = CdmHost::spawn(&config)?;
    host.initialize(false, false, false)?;
    println!("WVHC_INIT status=0");
    let init_data = make_pssh(&options.key_id);
    for index in 0..options.sessions {
        let mut events = Vec::new();
        let (session, message) =
            host.create_session(&init_data, SESSION_TEMPORARY, INIT_DATA_CENC, &mut events)?;
        let messages = events.iter().filter(|event| matches!(event, HostEvent::Message { session_id, .. } if session_id == &session)).count();
        if messages != 1 || message.as_ref().is_none_or(Vec::is_empty) {
            return Err(cdm_host::CdmHostError::Protocol(
                "expected one initial message",
            ));
        }
        println!("WVHC_CREATE_SESSION status=0 index={index} session={session}");
        println!(
            "WVHC_MESSAGE size={} messages={messages}",
            message.unwrap().len()
        );
        host.close_session(&session)?;
    }
    host.shutdown();
    println!("WVHC_OK sessions={}", options.sessions);
    Ok(())
}

fn main() {
    if let Err(error) = run(parse_arguments()) {
        eprintln!("WVHC_ERROR {error:?}");
        std::process::exit(1);
    }
}
