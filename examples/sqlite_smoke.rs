//! Validate the native SQLite VFS independently of WebCore and page scripts.
use std::ffi::{c_char, c_int, c_void, CStr, CString};

unsafe extern "C" {
    fn sqlite3_open_v2(path: *const c_char, db: *mut *mut c_void, flags: c_int, vfs: *const c_char) -> c_int;
    fn sqlite3_exec(db: *mut c_void, sql: *const c_char, callback: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>, data: *mut c_void, error: *mut *mut c_char) -> c_int;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
    fn sqlite3_close(db: *mut c_void) -> c_int;
}

unsafe extern "C" fn read_value(data: *mut c_void, count: c_int, values: *mut *mut c_char, _: *mut *mut c_char) -> c_int {
    // SAFETY: SQLite calls this synchronously with the supplied bool and row.
    unsafe {
        if count == 1 && !(*values).is_null() && CStr::from_ptr(*values).to_bytes() == b"native-disk-write" {
            *(data.cast::<bool>()) = true;
        }
    }
    0
}

fn main() -> std::process::ExitCode {
    let path = CString::new(std::env::args().nth(1).unwrap_or_else(|| "/tmp/myrica-sqlite-probe.sqlite".into())).unwrap();
    let mut db = std::ptr::null_mut();
    // SAFETY: All C strings and output pointers remain alive for each call.
    unsafe {
        let status = sqlite3_open_v2(path.as_ptr(), &mut db, 6, c"unix-flock".as_ptr());
        println!("SQLITE_NATIVE_OPEN status={status} message={}", CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy());
        if status != 0 { return std::process::ExitCode::FAILURE; }
        for sql in [c"PRAGMA journal_mode=DELETE;", c"CREATE TABLE IF NOT EXISTS probe (value TEXT);", c"DELETE FROM probe;", c"INSERT INTO probe VALUES ('native-disk-write');"] {
            let status = sqlite3_exec(db, sql.as_ptr(), None, std::ptr::null_mut(), std::ptr::null_mut());
            println!("SQLITE_NATIVE_EXEC sql={} status={status} message={}", sql.to_string_lossy(), CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy());
            if status != 0 { sqlite3_close(db); return std::process::ExitCode::FAILURE; }
        }
        sqlite3_close(db);
        db = std::ptr::null_mut();
        if sqlite3_open_v2(path.as_ptr(), &mut db, 6, c"unix-flock".as_ptr()) != 0 { return std::process::ExitCode::FAILURE; }
        let mut found = false;
        let status = sqlite3_exec(db, c"SELECT value FROM probe;".as_ptr(), Some(read_value), (&raw mut found).cast(), std::ptr::null_mut());
        if status != 0 || !found { sqlite3_close(db); return std::process::ExitCode::FAILURE; }
        // A second open description must conflict with a real exclusive lock.
        if sqlite3_exec(db, c"BEGIN EXCLUSIVE;".as_ptr(), None, std::ptr::null_mut(), std::ptr::null_mut()) != 0 { return std::process::ExitCode::FAILURE; }
        let mut other = std::ptr::null_mut();
        if sqlite3_open_v2(path.as_ptr(), &mut other, 6, c"unix-flock".as_ptr()) != 0 { return std::process::ExitCode::FAILURE; }
        let locked = sqlite3_exec(other, c"SELECT value FROM probe;".as_ptr(), None, std::ptr::null_mut(), std::ptr::null_mut());
        sqlite3_close(other);
        sqlite3_exec(db, c"ROLLBACK;".as_ptr(), None, std::ptr::null_mut(), std::ptr::null_mut());
        sqlite3_close(db);
        if locked != 5 { eprintln!("SQLITE_NATIVE_FAIL expected BUSY, got {locked}"); return std::process::ExitCode::FAILURE; }
        println!("SQLITE_NATIVE_PASS stages=disk-reopen-readback,exclusive-lock-conflict");
    }
    std::process::ExitCode::SUCCESS
}
