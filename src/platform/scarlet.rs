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

// Keep the Scarlet process/runtime crate linked even though ScarletUI owns main-loop setup.
use scarlet_os as _;
