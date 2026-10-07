//! Keep the machine awake while it does work for the party.

/// Ask the OS not to sleep (the display may still turn off) for as long
/// as the calling thread lives. Windows only for now: a worker laptop
/// left idle on its own would otherwise sleep mid-party. Needs no admin
/// rights. Elsewhere it does nothing.
pub fn keep_awake() {
    #[cfg(windows)]
    {
        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetThreadExecutionState(flags: u32) -> u32;
        }
        // SAFETY: plain Win32 call with documented flag values.
        let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        if prev == 0 {
            tracing::warn!("could not keep this machine awake");
        } else {
            tracing::info!("keeping this machine awake while the worker runs");
        }
    }
}
