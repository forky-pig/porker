// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::{CString, NulError, OsString};
use std::fmt::{Debug, Display};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::{env as p_env, fs, io};

use env::PROC_MOUNTS;
use utils::arg_parser::{ArgParser, Argument, UtilsArgParserError as ParsingError};
use utils::time::{ClockType, get_time_us};
use utils::validators;
use vmm_sys_util::syscall::SyscallReturnCode;

use crate::env::Env;

mod cgroup;
mod chroot;
mod env;
mod resource_limits;

const JAILER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PORKER_VERSION: &str = "0.1.0";
const PORKER_FORK_BASE: &str = "a481d51d0";

#[derive(Debug, thiserror::Error)]
pub enum JailerError {
    #[error("Failed to parse arguments: {0}")]
    ArgumentParsing(ParsingError),
    #[error("{}", format!("Failed to canonicalize path {:?}: {}", .0, .1).replace('\"', ""))]
    Canonicalize(PathBuf, io::Error),
    #[error("{}", format!("Failed to inherit cgroups configurations from file {} in path {:?}", .1, .0).replace('\"', ""))]
    CgroupInheritFromParent(PathBuf, String),
    #[error("{1} configurations not found in {0}")]
    CgroupLineNotFound(String, String),
    #[error("Cgroup invalid file: {0}")]
    CgroupInvalidFile(String),
    #[error("Invalid format for cgroups: {0}")]
    CgroupFormat(String),
    #[error("Hierarchy not found: {0}")]
    CgroupHierarchyMissing(String),
    #[error("Controller {0} is unavailable")]
    CgroupControllerUnavailable(String),
    #[error("{0} is an invalid cgroup version specifier")]
    CgroupInvalidVersion(String),
    #[error("Parent cgroup path is invalid. Path should not be absolute or contain '..' or '.'")]
    CgroupInvalidParentPath(),
    #[error(
        "Failed to move process to cgroup ({0}): {1}.\nHint: If you intended to create a child \
         cgroup under {0}, pass any --cgroup parameters."
    )]
    CgroupMove(PathBuf, io::Error),
    #[error("Failed to change owner for {0}: {1}")]
    ChangeFileOwner(PathBuf, io::Error),
    #[error("Failed to chdir into chroot directory: {0}")]
    ChdirNewRoot(io::Error),
    #[error("Failed to change permissions on {0}: {1}")]
    Chmod(PathBuf, io::Error),
    #[error("Failed cloning into a new child process: {0}")]
    Clone(io::Error),
    #[error("Failed to close netns fd: {0}")]
    CloseNetNsFd(io::Error),
    #[error("Failed to close /dev/null fd: {0}")]
    CloseDevNullFd(io::Error),
    #[error("Failed to call close range syscall: {0}")]
    CloseRange(io::Error),
    #[error("{}", format!("Failed to copy {:?} to {:?}: {}", .0, .1, .2).replace('\"', ""))]
    Copy(PathBuf, PathBuf, io::Error),
    #[error("{}", format!("Failed to create directory {:?}: {}", .0, .1).replace('\"', ""))]
    CreateDir(PathBuf, io::Error),
    #[error("Encountered interior \\0 while parsing a string")]
    CStringParsing(NulError),
    #[error("Failed to daemonize: {0}")]
    Daemonize(io::Error),
    #[error("Failed to open directory {0}: {1}")]
    DirOpen(String, String),
    #[error("Failed to duplicate fd: {0}")]
    Dup2(io::Error),
    #[error("Failed to exec into Firecracker: {0}")]
    Exec(io::Error),
    #[error("{}", format!("Failed to extract filename from path {:?}", .0).replace('\"', ""))]
    ExtractFileName(PathBuf),
    #[error("{}", format!("Failed to open file {:?}: {}", .0, .1).replace('\"', ""))]
    FileOpen(PathBuf, io::Error),
    #[error("Failed to decode string from byte array: {0}")]
    FromBytesWithNul(std::ffi::FromBytesWithNulError),
    #[error("Failed to get flags from fd: {0}")]
    GetOldFdFlags(io::Error),
    #[error("Failed to get PID (getpid): {0}")]
    GetPid(io::Error),
    #[error("Failed to get SID (getsid): {0}")]
    GetSid(io::Error),
    #[error("Invalid gid: {0}")]
    Gid(String),
    #[error("Detected hard link at: {0}")]
    HardLink(PathBuf),
    #[error("Invalid instance ID: {0}")]
    InvalidInstanceId(validators::ValidatorError),
    #[error("Cannot get metadata for a file: {0}: {1}")]
    Metadata(PathBuf, io::Error),
    #[error("{}", format!("File {:?} doesn't have a parent", .0).replace('\"', ""))]
    MissingParent(PathBuf),
    #[error("Failed to create the jail root directory before pivoting root: {0}")]
    MkdirOldRoot(io::Error),
    #[error("Failed to create {1} via mknod inside the jail: {0}")]
    MknodDev(io::Error, String),
    #[error("Failed to bind mount the jail root directory: {0}")]
    MountBind(io::Error),
    #[error("Failed to change the propagation type to slave: {0}")]
    MountPropagationSlave(io::Error),
    #[error("{}", format!("{:?} is not a file", .0).replace('\"', ""))]
    NotAFile(PathBuf),
    #[error("{}", format!("{:?} is not a directory", .0).replace('\"', ""))]
    NotADirectory(PathBuf),
    #[error("Failed to open {0}: {1}")]
    Open(PathBuf, io::Error),
    #[error("{}", format!("Failed to parse path {:?} into an OsString", .0).replace('\"', ""))]
    OsStringParsing(PathBuf, OsString),
    #[error("Failed to pivot root: {0}")]
    PivotRoot(io::Error),
    #[error("{}", format!("Failed to read line from {:?}: {}", .0, .1).replace('\"', ""))]
    ReadLine(PathBuf, io::Error),
    #[error("{}", format!("Failed to read file {:?} into a string: {}", .0, .1).replace('\"', ""))]
    ReadToString(PathBuf, io::Error),
    #[error("Regex failed: {0}")]
    RegEx(regex::Error),
    #[error("Invalid resource argument: {0}")]
    ResLimitArgument(String),
    #[error("Invalid format for resources limits: {0}")]
    ResLimitFormat(String),
    #[error("Invalid limit value for resource: {0}: {1}")]
    ResLimitValue(String, String),
    #[error("Failed to remove old jail root directory: {0}")]
    RmOldRootDir(io::Error),
    #[error("Failed to change current directory: {0}")]
    SetCurrentDir(io::Error),
    #[error("Failed to join network namespace: netns: {0}")]
    SetNetNs(io::Error),
    #[error("Failed to set limit for resource: {0}")]
    Setrlimit(String),
    #[error("Failed to daemonize: setsid: {0}")]
    SetSid(io::Error),
    #[error("Invalid uid: {0}")]
    Uid(String),
    #[error("Failed to unmount the old jail root: {0}")]
    UmountOldRoot(io::Error),
    #[error("Unexpected value for the socket listener fd: {0}")]
    UnexpectedListenerFd(i32),
    #[error("Failed to unshare into new mount namespace: {0}")]
    UnshareNewNs(io::Error),
    #[error("Failed to unset the O_CLOEXEC flag on the socket fd: {0}")]
    UnsetCloexec(io::Error),
    #[error("Slice contains invalid UTF-8 data : {0}")]
    UTF8Parsing(std::str::Utf8Error),
    #[error("{}", format!("Failed to write to {:?}: {}", .0, .1).replace('\"', ""))]
    Write(PathBuf, io::Error),
}

/// Create an ArgParser object which contains info about the command line argument parser and
/// populate it with the expected arguments and their characteristics.
pub fn build_arg_parser() -> ArgParser<'static> {
    ArgParser::new()
        .arg(
            Argument::new("id")
                .required(true)
                .takes_value(true)
                .help("Jail ID."),
        )
        .arg(
            Argument::new("exec-file")
                .required(true)
                .takes_value(true)
                .help("File path to exec into."),
        )
        .arg(
            Argument::new("uid")
                .required(true)
                .takes_value(true)
                .help("The user identifier the jailer switches to after exec."),
        )
        .arg(
            Argument::new("gid")
                .required(true)
                .takes_value(true)
                .help("The group identifier the jailer switches to after exec."),
        )
        .arg(
            Argument::new("chroot-base-dir")
                .takes_value(true)
                .default_value("/srv/jailer")
                .help("The base folder where chroot jails are located."),
        )
        .arg(
            Argument::new("netns")
                .takes_value(true)
                .help("Path to the network namespace this microVM should join."),
        )
        .arg(Argument::new("daemonize").takes_value(false).help(
            "Daemonize the jailer before exec, by invoking setsid(), and redirecting the standard \
             I/O file descriptors to /dev/null.",
        ))
        .arg(
            Argument::new("new-pid-ns")
                .takes_value(false)
                .help("Exec into a new PID namespace."),
        )
        .arg(Argument::new("cgroup").allow_multiple(true).help(
            "Cgroup and value to be set by the jailer. It must follow this format: \
             <cgroup_file>=<value> (e.g cpu.shares=10). This argument can be used multiple times \
             to add multiple cgroups.",
        ))
        .arg(Argument::new("resource-limit").allow_multiple(true).help(
            "Resource limit values to be set by the jailer. It must follow this format: \
             <resource>=<value> (e.g no-file=1024). This argument can be used multiple times to \
             add multiple resource limits. Current available resource values are:\n\t\tfsize: The \
             maximum size in bytes for files created by the process.\n\t\tno-file: Specifies a \
             value one greater than the maximum file descriptor number that can be opened by this \
             process.",
        ))
        .arg(
            Argument::new("cgroup-version")
                .takes_value(true)
                .default_value("1")
                .help("Select the cgroup version used by the jailer."),
        )
        .arg(
            Argument::new("parent-cgroup")
                .takes_value(true)
                .help("Parent cgroup in which the cgroup of this microvm will be placed."),
        )
        .arg(
            Argument::new("version")
                .takes_value(false)
                .help("Print the binary version number."),
        )
        .arg(Argument::new("vfio-group-fd").allow_multiple(true).help(
            "File descriptor (number) of an already-open VFIO group, e.g. /dev/vfio/14. \
             The caller pre-opens the file before exec'ing jailer; the fd is then preserved \
             across the chroot and exec into firecracker. Repeat to pass multiple groups. \
             Required for VFIO PCI passthrough.",
        ))
        .arg(Argument::new("vfio-container-fd").takes_value(true).help(
            "File descriptor (number) of an already-open /dev/vfio/vfio container. \
             Same lifecycle as --vfio-group-fd.",
        ))
}

// It's called writeln_special because we have to use this rather convoluted way of writing
// to special cgroup files, to avoid getting errors. It would be nice to know why that happens :-s
pub fn writeln_special<T, V>(file_path: &T, value: V) -> Result<(), JailerError>
where
    T: AsRef<Path> + Debug,
    V: Display + Debug,
{
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file_path.as_ref())
        .map_err(|err| JailerError::Write(PathBuf::from(file_path.as_ref()), err))?;
    io::Write::write_all(&mut file, format!("{}\n", value).as_bytes())
        .map_err(|err| JailerError::Write(PathBuf::from(file_path.as_ref()), err))
}

pub fn readln_special<T: AsRef<Path> + Debug>(file_path: &T) -> Result<String, JailerError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file_path.as_ref())
        .map_err(|err| JailerError::ReadToString(PathBuf::from(file_path.as_ref()), err))?;
    let mut line = String::new();
    file.read_to_string(&mut line)
        .map_err(|err| JailerError::ReadToString(PathBuf::from(file_path.as_ref()), err))?;

    // Remove the newline character at the end (if any).
    line.pop();

    Ok(line)
}

fn close_range_raw(start: u32, end: u32) -> Result<(), JailerError> {
    // SAFETY: if the syscall is not available then ENOSYS will be returned
    SyscallReturnCode(unsafe {
        libc::syscall(
            libc::SYS_close_range,
            start,
            end,
            libc::CLOSE_RANGE_UNSHARE,
        )
    })
    .into_empty_result()
    .map_err(JailerError::CloseRange)
}

fn close_fds_by_close_range() -> Result<(), JailerError> {
    // Close all open FDs in the range 3..UINT_MAX (no protected fds).
    close_range_raw(3, libc::c_uint::MAX)
}

// Closes all FDs other than 0 (STDIN), 1 (STDOUT), 2 (STDERR), and the
// caller-specified `protected` fds (e.g. VFIO group/container fds the caller
// pre-opened for the jailed firecracker). All entries in `protected` must be
// >= 3.
fn close_inherited_fds_except(protected: &[i32]) -> Result<(), JailerError> {
    if protected.is_empty() {
        return close_fds_by_close_range();
    }

    let mut sorted: Vec<u32> = protected
        .iter()
        .filter_map(|&fd| if fd >= 3 { Some(fd as u32) } else { None })
        .collect();
    sorted.sort();
    sorted.dedup();

    let mut cursor: u32 = 3;
    for fd in sorted {
        if fd > cursor {
            close_range_raw(cursor, fd - 1)?;
        }
        cursor = fd.checked_add(1).ok_or(JailerError::CloseRange(
            std::io::Error::from_raw_os_error(libc::EINVAL),
        ))?;
    }
    if cursor <= libc::c_uint::MAX {
        close_range_raw(cursor, libc::c_uint::MAX)?;
    }
    Ok(())
}

// Clears FD_CLOEXEC on the given fd so it survives exec().
fn clear_cloexec(fd: i32) -> Result<(), JailerError> {
    // SAFETY: standard fcntl(F_GETFD) on a caller-supplied fd. Errors are returned.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(JailerError::GetOldFdFlags(std::io::Error::last_os_error()));
    }
    let new_flags = flags & !libc::FD_CLOEXEC;
    // SAFETY: fcntl(F_SETFD) with a valid flags value.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
    if rc < 0 {
        return Err(JailerError::UnsetCloexec(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn sanitize_process_except(protected: &[i32]) -> Result<(), JailerError> {
    close_inherited_fds_except(protected)?;
    clean_env_vars();
    Ok(())
}

#[cfg(test)]
fn sanitize_process() -> Result<(), JailerError> {
    sanitize_process_except(&[])
}

fn clean_env_vars() {
    // Remove environment variables received from
    // the parent process so there are no leaks
    // inside the jailer environment
    for (key, _) in p_env::vars() {
        // SAFETY: the function is safe to call in a single-threaded program
        unsafe {
            p_env::remove_var(key);
        }
    }
}

/// Turns an [`AsRef<Path>`] into a [`CString`] (c style string).
/// The expect should not fail, since Linux paths only contain valid Unicode chars (do they?),
/// and do not contain null bytes (do they?).
pub fn to_cstring<T: AsRef<Path> + Debug>(path: T) -> Result<CString, JailerError> {
    let path_str = path
        .as_ref()
        .to_path_buf()
        .into_os_string()
        .into_string()
        .map_err(|err| JailerError::OsStringParsing(path.as_ref().to_path_buf(), err))?;
    CString::new(path_str).map_err(JailerError::CStringParsing)
}

/// We wrap the actual main in order to pretty print an error with Display trait.
fn main() -> Result<(), JailerError> {
    let result = main_exec();
    if let Err(e) = result {
        eprintln!("{}", e);
        Err(e)
    } else {
        Ok(())
    }
}

fn main_exec() -> Result<(), JailerError> {
    // We parse args BEFORE sanitizing, because args may name fds the caller
    // pre-opened for us (VFIO group/container fds). Those must be excluded
    // from the close_range that sanitize_process performs.
    let mut arg_parser = build_arg_parser();
    arg_parser
        .parse_from_cmdline()
        .map_err(JailerError::ArgumentParsing)?;
    let arguments = arg_parser.arguments();

    if arguments.flag_present("help") {
        println!(
            "Porker Jailer v{PORKER_VERSION} (firecracker-fork @ {PORKER_FORK_BASE}, FC Jailer v{JAILER_VERSION})\n"
        );
        println!("{}\n", arg_parser.formatted_help());
        println!("Any arguments after the -- separator will be supplied to the jailed binary.\n");
        return Ok(());
    }

    if arguments.flag_present("version") {
        println!(
            "Porker Jailer v{PORKER_VERSION} (firecracker-fork @ {PORKER_FORK_BASE}, FC Jailer v{JAILER_VERSION})\n"
        );
        return Ok(());
    }

    // Collect VFIO fds to protect across sanitize + exec. Validate each
    // is currently open before we proceed. If any are wrong, fail fast.
    let mut protected_fds: Vec<i32> = Vec::new();
    if let Some(group_fds) = arguments.multiple_values("vfio-group-fd") {
        for s in group_fds {
            let fd: i32 = s.parse().map_err(|_| {
                JailerError::ArgumentParsing(utils::arg_parser::UtilsArgParserError::MissingValue(
                    "vfio-group-fd".to_string(),
                ))
            })?;
            // SAFETY: F_GETFD on caller-supplied fd. Result checked below.
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(JailerError::GetOldFdFlags(std::io::Error::last_os_error()));
            }
            protected_fds.push(fd);
        }
    }
    if let Some(s) = arguments.single_value("vfio-container-fd") {
        let fd: i32 = s.parse().map_err(|_| {
            JailerError::ArgumentParsing(utils::arg_parser::UtilsArgParserError::MissingValue(
                "vfio-container-fd".to_string(),
            ))
        })?;
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(JailerError::GetOldFdFlags(std::io::Error::last_os_error()));
        }
        protected_fds.push(fd);
    }

    sanitize_process_except(&protected_fds)
        .unwrap_or_else(|err| panic!("Failed to sanitize the Jailer process: {}", err));

    // Clear FD_CLOEXEC so the protected fds survive the exec into firecracker.
    for &fd in &protected_fds {
        clear_cloexec(fd)
            .unwrap_or_else(|err| panic!("Failed to clear CLOEXEC on fd {}: {}", fd, err));
    }

    Env::new(
        arguments,
        get_time_us(ClockType::Monotonic),
        get_time_us(ClockType::ProcessCpu),
        PROC_MOUNTS,
    )
    .and_then(|env| {
        fs::create_dir_all(env.chroot_dir())
            .map_err(|err| JailerError::CreateDir(env.chroot_dir().to_owned(), err))?;
        env.run()
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::env;
    use std::ffi::CStr;
    use std::fs::File;
    use std::os::unix::io::IntoRawFd;

    use vmm_sys_util::rand;

    use super::*;

    fn run_close_fds_test(test_fn: fn() -> Result<(), JailerError>) {
        let n = 100;

        let tmp_dir_path = format!(
            "/tmp/jailer/tests/close_fds/_{}",
            rand::rand_alphanumerics(4).into_string().unwrap()
        );
        fs::create_dir_all(&tmp_dir_path).unwrap();

        let mut fds = Vec::new();
        for i in 0..n {
            let maybe_file = File::create(format!("{}/{}", &tmp_dir_path, i));
            fds.push(maybe_file.unwrap().into_raw_fd());
        }

        test_fn().unwrap();

        for fd in fds {
            let is_fd_opened = unsafe { libc::fcntl(fd, libc::F_GETFD) } == 0;
            assert!(!is_fd_opened);
        }

        fs::remove_dir_all(tmp_dir_path).unwrap();
    }

    #[test]
    fn test_fds_close_range() {
        // SAFETY: Always safe
        let mut n = unsafe { std::mem::zeroed() };
        // SAFETY: We check if the uname call succeeded
        assert_eq!(unsafe { libc::uname(&mut n) }, 0);
        // SAFETY: Always safe
        let release = unsafe { CStr::from_ptr(n.release.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        // Parse the major and minor version of the kernel
        let mut r = release.split('.');
        let major: i32 = str::parse(r.next().unwrap()).unwrap();
        let minor: i32 = str::parse(r.next().unwrap()).unwrap();

        // Skip this test if we're running on a too old kernel
        if major > 5 || (major == 5 && minor >= 9) {
            run_close_fds_test(close_fds_by_close_range);
        }
    }

    #[test]
    fn test_sanitize_process() {
        run_close_fds_test(sanitize_process);
    }

    #[test]
    fn test_close_inherited_fds_except_preserves_protected() {
        // Open 20 fds, protect 5 of them, verify the protected ones survive
        // and the rest are closed.
        let tmp_dir = format!(
            "/tmp/jailer/tests/close_except_{}",
            rand::rand_alphanumerics(4).into_string().unwrap()
        );
        fs::create_dir_all(&tmp_dir).unwrap();

        let mut all_fds = Vec::new();
        for i in 0..20 {
            let f = File::create(format!("{}/{}", &tmp_dir, i)).unwrap();
            all_fds.push(f.into_raw_fd());
        }
        let protected: Vec<i32> = vec![all_fds[2], all_fds[5], all_fds[10], all_fds[15], all_fds[19]];
        let unprotected: Vec<i32> = all_fds.iter().filter(|f| !protected.contains(f)).copied().collect();

        close_inherited_fds_except(&protected).unwrap();

        for fd in &unprotected {
            let still_open = unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0;
            assert!(!still_open, "fd {fd} should have been closed");
        }
        for fd in &protected {
            let still_open = unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0;
            assert!(still_open, "fd {fd} should still be open");
        }

        // Clean up: close protected fds explicitly so we don't leak.
        for fd in protected {
            unsafe {
                libc::close(fd);
            }
        }
        fs::remove_dir_all(tmp_dir).unwrap();
    }

    #[test]
    fn test_clear_cloexec_clears_flag() {
        let tmp_dir = format!(
            "/tmp/jailer/tests/cloexec_{}",
            rand::rand_alphanumerics(4).into_string().unwrap()
        );
        fs::create_dir_all(&tmp_dir).unwrap();
        let f = File::create(format!("{}/marker", &tmp_dir)).unwrap();
        let fd = f.into_raw_fd();

        // Files opened via std::fs::File have FD_CLOEXEC set by default.
        let before = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(before >= 0);
        assert_eq!(before & libc::FD_CLOEXEC, libc::FD_CLOEXEC);

        clear_cloexec(fd).unwrap();

        let after = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(after >= 0);
        assert_eq!(after & libc::FD_CLOEXEC, 0);

        unsafe {
            libc::close(fd);
        }
        fs::remove_dir_all(tmp_dir).unwrap();
    }

    #[test]
    fn test_clean_env_vars() {
        let env_vars: [&str; 5] = ["VAR1", "VAR2", "VAR3", "VAR4", "VAR5"];

        // Set environment variables
        for env_var in env_vars.iter() {
            // SAFETY: the function is safe to call in a single-threaded program
            unsafe {
                env::set_var(env_var, "0");
            }
        }

        // Cleanup the environment
        clean_env_vars();

        // Assert that the variables set beforehand
        // do not exist anymore
        for env_var in env_vars.iter() {
            assert_eq!(env::var_os(env_var), None);
        }
    }

    #[test]
    fn test_to_cstring() {
        let path = Path::new("some_path");
        let cstring_path = to_cstring(path).unwrap();
        assert_eq!(cstring_path, CString::new("some_path").unwrap());
        let path_with_nul = Path::new("some_path\0");
        assert_eq!(
            format!("{}", to_cstring(path_with_nul).unwrap_err()),
            "Encountered interior \\0 while parsing a string"
        );
    }
}
