//! Scarlet-specific process integration.

use std::num::NonZeroU32;

const CUSTOM_RANDOM_ERROR: u32 = getrandom::Error::CUSTOM_START + 1;

getrandom::register_custom_getrandom!(scarlet_getrandom);

fn scarlet_getrandom(destination: &mut [u8]) -> Result<(), getrandom::Error> {
    let mut offset = 0usize;
    while offset < destination.len() {
        // SAFETY: The remaining slice is exclusively borrowed and writable for the call.
        let result = unsafe {
            scarlet_sys::syscall3(
                scarlet_sys::Syscall::GetRandom,
                destination[offset..].as_mut_ptr() as usize,
                destination.len() - offset,
                scarlet_sys::GET_RANDOM_FLAG_REQUIRE_ENTROPY,
            )
        };
        if result == usize::MAX || result == 0 || result > destination.len() - offset {
            let code = NonZeroU32::new(CUSTOM_RANDOM_ERROR)
                .expect("custom getrandom error code must be non-zero");
            return Err(getrandom::Error::from(code));
        }
        offset += result;
    }
    Ok(())
}

#[cfg(feature = "javascript")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    destination: *mut u8,
    length: usize,
) -> Result<(), getrandom_v04::Error> {
    if length == 0 {
        return Ok(());
    }
    // SAFETY: getrandom provides a writable buffer of `length` bytes. Initialize
    // it before creating a slice, including when the native syscall fails.
    unsafe { std::ptr::write_bytes(destination, 0, length) };
    let destination = unsafe { std::slice::from_raw_parts_mut(destination, length) };
    scarlet_getrandom(destination).map_err(|_| getrandom_v04::Error::UNEXPECTED)
}

// Keep the Scarlet process/runtime crate linked even though ScarletUI owns main-loop setup.
use scarlet_os as _;
