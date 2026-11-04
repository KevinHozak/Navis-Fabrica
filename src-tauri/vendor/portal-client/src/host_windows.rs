//! Metadata and handle operations only; no target memory, payload or credential reads.
use crate::{error, ClientError};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::net::TcpStream;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

type H = *mut c_void;
#[repr(C)]
struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}
#[repr(C)]
#[derive(Default)]
struct Time {
    low: u32,
    high: u32,
}
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> H;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> H;
    fn CloseHandle(h: H) -> i32;
    fn LocalFree(h: H) -> H;
    fn GetProcessTimes(h: H, a: *mut Time, b: *mut Time, c: *mut Time, d: *mut Time) -> i32;
    fn QueryFullProcessImageNameW(h: H, flags: u32, buf: *mut u16, size: *mut u32) -> i32;
    fn WaitForSingleObject(h: H, ms: u32) -> u32;
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        sa: H,
        how: u32,
        flags: u32,
        template: H,
    ) -> H;
    fn PeekNamedPipe(
        h: H,
        buf: H,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
    fn GetLastError() -> u32;
}
#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: H, access: u32, token: *mut H) -> i32;
    fn GetTokenInformation(token: H, class: u32, buffer: H, size: u32, needed: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: H, out: *mut *mut u16) -> i32;
    fn GetNamedSecurityInfoW(
        path: *const u16,
        kind: u32,
        info: u32,
        owner: *mut H,
        group: H,
        dacl: *mut H,
        sacl: H,
        descriptor: *mut H,
    ) -> u32;
    fn GetAce(acl: H, index: u32, ace: *mut H) -> i32;
}
#[link(name = "shell32")]
extern "system" {
    fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: H, out: *mut *mut u16) -> i32;
}
#[link(name = "ole32")]
extern "system" {
    fn CoTaskMemFree(p: H);
}

fn bad() -> ClientError {
    error("not_sent", "HOST_PROFILE_INVALID")
}
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}
unsafe fn text(p: *const u16) -> Result<String, ClientError> {
    if p.is_null() {
        return Err(bad());
    }
    let mut n = 0;
    while n < 32768 && *p.add(n) != 0 {
        n += 1
    }
    if n == 32768 {
        return Err(bad());
    }
    String::from_utf16(std::slice::from_raw_parts(p, n)).map_err(|_| bad())
}
unsafe fn sid_text(sid: H) -> Result<String, ClientError> {
    let mut out = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut out) == 0 {
        return Err(bad());
    }
    let result = text(out);
    LocalFree(out.cast());
    result
}
unsafe fn process_sid(process: H) -> Result<String, ClientError> {
    let mut token = std::ptr::null_mut();
    if OpenProcessToken(process, 8, &mut token) == 0 {
        return Err(bad());
    }
    let result = (|| {
        let mut needed = 0;
        GetTokenInformation(token, 1, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 || needed > 65536 {
            return Err(bad());
        }
        // Pointer-aligned TOKEN_USER buffer, with lifetime covering SID conversion.
        let mut buf = vec![0usize; (needed as usize + 7) / 8];
        if GetTokenInformation(token, 1, buf.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return Err(bad());
        }
        sid_text(*(buf.as_ptr() as *const H))
    })();
    CloseHandle(token);
    result
}
pub(crate) fn sid() -> Result<String, ClientError> {
    unsafe { process_sid(GetCurrentProcess()) }
}
pub(crate) fn root() -> Result<PathBuf, ClientError> {
    unsafe {
        let id = Guid {
            a: 0x5e6c858f,
            b: 0x0e22,
            c: 0x4760,
            d: [0x9a, 0xfe, 0xea, 0x33, 0x17, 0xb6, 0x71, 0x73],
        };
        let mut out = std::ptr::null_mut();
        if SHGetKnownFolderPath(&id, 0, std::ptr::null_mut(), &mut out) != 0 {
            return Err(bad());
        }
        let result = text(out).map(|s| PathBuf::from(s).join("NavisFabrica-Portal"));
        CoTaskMemFree(out.cast());
        result
    }
}
fn no_links(path: &Path) -> Result<(), ClientError> {
    if !path.is_absolute() {
        return Err(bad());
    }
    for p in path.ancestors() {
        if std::fs::symlink_metadata(p)
            .map_err(|_| bad())?
            .file_attributes()
            & 0x400
            != 0
        {
            return Err(bad());
        }
    }
    Ok(())
}
pub(crate) fn private(path: &Path) -> Result<(), ClientError> {
    no_links(path)?;
    let current = sid()?;
    let allowed = [current.as_str(), "S-1-5-18", "S-1-5-32-544"];
    unsafe {
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        if GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            1,
            5,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        ) != 0
        {
            return Err(bad());
        }
        let result = (|| {
            if acl.is_null() || !allowed.contains(&sid_text(owner)?.as_str()) {
                return Err(bad());
            }
            // ACL header: revision/reserved/size/count/reserved.
            let count = *(acl.cast::<u8>().add(4) as *const u16) as u32;
            for i in 0..count {
                let mut ace = std::ptr::null_mut();
                if GetAce(acl, i, &mut ace) == 0 {
                    return Err(bad());
                }
                let kind = *ace.cast::<u8>();
                if kind == 1 {
                    continue;
                }
                if kind != 0
                    || !allowed.contains(&sid_text(ace.cast::<u8>().add(8).cast())?.as_str())
                {
                    return Err(bad());
                }
            }
            Ok(())
        })();
        LocalFree(descriptor);
        result
    }
}
pub(crate) fn file(path: &Path) -> Result<File, ClientError> {
    no_links(path)?;
    OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(path)
        .map_err(|_| bad())
}
pub(crate) fn directories(path: &Path) -> Result<Vec<File>, ClientError> {
    no_links(path)?;
    let mut pins = Vec::new();
    for p in path.ancestors().skip(1) {
        unsafe {
            let h = CreateFileW(
                wide(p).as_ptr(),
                0x80,
                3,
                std::ptr::null_mut(),
                3,
                0x02000000,
                std::ptr::null_mut(),
            );
            if h.is_null() || h as isize == -1 {
                return Err(bad());
            }
            pins.push(File::from_raw_handle(h));
        }
    }
    Ok(pins)
}

pub(crate) fn pipe_available(file: &std::process::ChildStdout) -> Result<Option<u32>, ClientError> {
    unsafe {
        let mut available = 0;
        if PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        ) == 0
        {
            if GetLastError() == 109 {
                return Ok(None);
            }
            return Err(error("not_sent", "HOST_UNAVAILABLE"));
        }
        Ok(Some(available))
    }
}

pub(crate) fn peer(stream: &TcpStream, identity: &super::Peer) -> Result<(), ClientError> {
    let fail = || error("not_sent", "HOST_LISTENER_UNTRUSTED");
    unsafe {
        let h = OpenProcess(0x100000 | 0x1000, 0, identity.pid);
        if h.is_null() {
            return Err(fail());
        }
        let result = (|| {
            if WaitForSingleObject(h, 0) != 258 {
                return Err(fail());
            }
            let (mut a, mut b, mut c, mut d) = (
                Time::default(),
                Time::default(),
                Time::default(),
                Time::default(),
            );
            if GetProcessTimes(h, &mut a, &mut b, &mut c, &mut d) == 0
                || ((a.high as u64) << 32 | a.low as u64) != identity.created
            {
                return Err(fail());
            }
            let mut name = vec![0u16; 32768];
            let mut size = name.len() as u32;
            if QueryFullProcessImageNameW(h, 0, name.as_mut_ptr(), &mut size) == 0 {
                return Err(fail());
            }
            let image = String::from_utf16(&name[..size as usize]).map_err(|_| fail())?;
            if image.to_lowercase() != identity.executable.to_lowercase()
                || process_sid(h)? != identity.sid
            {
                return Err(fail());
            }
            let server = stream.peer_addr().map_err(|_| fail())?;
            let client = stream.local_addr().map_err(|_| fail())?;
            if !server.ip().is_loopback() {
                return Err(fail());
            }
            verify_tcp(server.port(), client.port(), identity.pid)
        })();
        CloseHandle(h);
        result
    }
}
#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        table: H,
        size: *mut u32,
        order: i32,
        family: u32,
        class: u32,
        reserved: u32,
    ) -> u32;
}
unsafe fn verify_tcp(port: u16, remote: u16, pid: u32) -> Result<(), ClientError> {
    let fail = || error("not_sent", "HOST_LISTENER_UNTRUSTED");
    let mut size = 0;
    if GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, 2, 5, 0) != 122 {
        return Err(fail());
    }
    for _ in 0..3 {
        if !(4..=16 * 1024 * 1024).contains(&size) {
            return Err(fail());
        }
        let mut raw = vec![0u32; (size as usize + 3) / 4];
        let status = GetExtendedTcpTable(raw.as_mut_ptr().cast(), &mut size, 0, 2, 5, 0);
        if status == 122 {
            continue;
        }
        if status != 0 {
            return Err(fail());
        }
        let bytes = std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), size as usize);
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        if count > 100000 || 4 + count * 24 > bytes.len() {
            return Err(fail());
        }
        let mut matched = false;
        for row in bytes[4..4 + count * 24].chunks_exact(24) {
            if u16::from_be_bytes(row[8..10].try_into().unwrap()) == port
                && u16::from_be_bytes(row[16..18].try_into().unwrap()) == remote
            {
                if row[4..8] != [127, 0, 0, 1]
                    || row[12..16] != [127, 0, 0, 1]
                    || u32::from_le_bytes(row[20..24].try_into().unwrap()) != pid
                {
                    return Err(fail());
                }
                matched = true;
            }
        }
        return if matched { Ok(()) } else { Err(fail()) };
    }
    Err(fail())
}
