//! Prevent libcamera (including optional IPA isolation) from creating helper processes.
use anyhow::{Context, Result, ensure};

const LOAD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const HAS_BITS: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RETURN: u16 = 0x06; // BPF_RET | BPF_K
const ALLOW: u32 = 0x7fff0000;
const ERRNO: u32 = 0x00050000;
const KILL: u32 = 0x80000000;

#[cfg(target_arch = "x86_64")]
const ARCH: u32 = 0xc000003e;
#[cfg(target_arch = "aarch64")]
const ARCH: u32 = 0xc00000b7;
#[cfg(target_arch = "arm")]
const ARCH: u32 = 0x40000028;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "arm")))]
compile_error!("process guard supports ARMv7, AArch64 and x86-64 Linux");

fn instruction(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, k, jt, jf }
}

fn filter() -> Vec<libc::sock_filter> {
    let mut code = vec![
        instruction(LOAD, 4, 0, 0), // seccomp_data.arch
        instruction(EQUAL, ARCH, 1, 0),
        instruction(RETURN, KILL, 0, 0),
        instruction(LOAD, 0, 0, 0), // seccomp_data.nr
    ];
    #[cfg(target_arch = "x86_64")]
    code.extend([
        instruction(HAS_BITS, 0x40000000, 0, 1), // Reject the alternate x32 syscall ABI.
        instruction(RETURN, ERRNO | libc::EPERM as u32, 0, 0),
    ]);
    let forbidden = [
        libc::SYS_execve,
        libc::SYS_execveat,
        #[cfg(any(target_arch = "x86_64", target_arch = "arm"))]
        libc::SYS_fork,
        #[cfg(any(target_arch = "x86_64", target_arch = "arm"))]
        libc::SYS_vfork,
    ];
    for syscall in forbidden {
        code.extend([
            instruction(EQUAL, syscall as u32, 0, 1),
            instruction(RETURN, ERRNO | libc::EPERM as u32, 0, 0),
        ]);
    }
    code.extend([
        instruction(EQUAL, libc::SYS_clone3 as u32, 0, 1),
        // libc falls back to clone for pthread creation on ENOSYS.
        instruction(RETURN, ERRNO | libc::ENOSYS as u32, 0, 0),
        instruction(EQUAL, libc::SYS_clone as u32, 0, 3),
        instruction(LOAD, 16, 0, 0), // seccomp_data.args[0], low 32 bits (little endian).
        instruction(HAS_BITS, libc::CLONE_THREAD as u32, 1, 0),
        instruction(RETURN, ERRNO | libc::EPERM as u32, 0, 0),
        instruction(RETURN, ALLOW, 0, 0),
    ]);
    code
}

fn install_with_flags(flags: u32) -> Result<()> {
    let mut code = filter();
    let program = libc::sock_fprog {
        len: code.len() as u16,
        filter: code.as_mut_ptr(),
    };
    // SAFETY: prctl takes integer flags; the filter pointer remains valid during seccomp.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("set no_new_privs");
    }
    // SAFETY: program references initialized BPF instructions for the duration of the call.
    let status = unsafe { libc::syscall(libc::SYS_seccomp, 1u32, flags, &program) };
    if status < 0 {
        return Err(std::io::Error::last_os_error()).context("install no-child-process filter");
    }
    ensure!(
        status == 0,
        "cannot synchronize process filter to thread {status}"
    );
    Ok(())
}

pub fn install() -> Result<()> {
    install_with_flags(1) // SECCOMP_FILTER_FLAG_TSYNC, covering every existing thread.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_guard_allows_threads_and_blocks_processes() {
        std::thread::spawn(|| {
            install_with_flags(0).unwrap(); // Isolate the filter to this test thread and its descendants.
            assert_eq!(std::thread::spawn(|| 42).join().unwrap(), 42);
            {
                use crate::encoder::{software::SoftwareEncoder, yuv::Yuv420};
                let camera = crate::config::Camera {
                    width: 64,
                    height: 64,
                    ..Default::default()
                };
                let mut encoder = SoftwareEncoder::new(&camera).unwrap();
                let pixels = vec![80; 64 * 64 * 3 / 2];
                let frame = Yuv420::new(&[&pixels], 64, 64, 64).unwrap();
                let mut count = 0;
                encoder
                    .encode(&frame, 123456, true, |packet| {
                        assert_eq!(packet.timestamp_us, 123456);
                        assert!(!packet.data.is_empty());
                        count += 1;
                        Ok(())
                    })
                    .unwrap();
                encoder.finish();
                assert_eq!(count, 1);
            }
            // SAFETY: the installed filter rejects clone unless CLONE_THREAD is present.
            assert_eq!(unsafe { libc::syscall(libc::SYS_clone, 0, 0, 0, 0, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            // SAFETY: clone3 is always rejected with ENOSYS to allow the pthread fallback.
            assert_eq!(unsafe { libc::syscall(libc::SYS_clone3, 0, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOSYS)
            );
            // SAFETY: the filter rejects execve before examining these null arguments.
            assert_eq!(unsafe { libc::syscall(libc::SYS_execve, 0, 0, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            // SAFETY: execveat is rejected before its arguments are inspected.
            assert_eq!(
                unsafe { libc::syscall(libc::SYS_execveat, 0, 0, 0, 0, 0) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        })
        .join()
        .unwrap();
    }
}
