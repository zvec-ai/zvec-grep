use std::{ffi::OsStr, mem::MaybeUninit};

/// Apply the CLI's llama.cpp Metal default during single-threaded Darwin startup.
/// Returns false when the caller needs to supply it through a fresh process image.
#[must_use]
pub fn try_set_metal_residency_default() -> bool {
    if std::env::var_os("GGML_METAL_NO_RESIDENCY").is_some()
        || std::env::var_os("ZVEC_GREP_METAL_KEEP_RESIDENCY").as_deref() == Some(OsStr::new("1"))
    {
        return true;
    }
    let mut task = MaybeUninit::<libc::proc_taskinfo>::uninit();
    let Ok(size) = i32::try_from(size_of_val(&task)) else {
        return false;
    };
    // SAFETY: proc_pidinfo fills the correctly sized buffer for our own task.
    // Read it only on a full response. The kernel's count includes native threads.
    // When this is the sole thread, no other thread can access environ or spawn
    // a reader before setenv returns. Both strings are static and NUL-terminated.
    unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            task.as_mut_ptr().cast(),
            size,
        ) == size
            && task.assume_init().pti_threadnum == 1
            && libc::setenv(c"GGML_METAL_NO_RESIDENCY".as_ptr(), c"1".as_ptr(), 0) == 0
    }
}
