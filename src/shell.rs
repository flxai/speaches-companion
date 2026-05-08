use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context};

#[derive(Debug, Clone)]
pub struct ShellTypingDemoOptions {
    pub shell_path: PathBuf,
    pub text: String,
    pub startup_delay: Duration,
    pub char_delay: Duration,
    pub settle_delay: Duration,
}

impl ShellTypingDemoOptions {
    pub fn new(shell_path: PathBuf, text: String) -> Self {
        Self {
            shell_path,
            text,
            startup_delay: Duration::from_millis(400),
            char_delay: Duration::from_millis(50),
            settle_delay: Duration::from_millis(800),
        }
    }
}

pub fn default_shell_path() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

pub fn run_shell_typing_demo(options: ShellTypingDemoOptions) -> anyhow::Result<()> {
    let mut process = PtyProcess::spawn(&options.shell_path, ["-i"])?;
    let output_thread = process.forward_output_to_stdout()?;

    if !options.startup_delay.is_zero() {
        thread::sleep(options.startup_delay);
    }

    process
        .type_text(&options.text, options.char_delay)
        .context("failed to type demo text into shell PTY")?;
    process
        .press_enter()
        .context("failed to submit demo text to shell PTY")?;

    if !options.settle_delay.is_zero() {
        thread::sleep(options.settle_delay);
    }
    let _ = process.type_text("exit", options.char_delay);
    let _ = process.press_enter();

    let status = process.wait()?;
    join_output_thread(output_thread)?;
    if !status.success() {
        bail!("shell demo exited with status {status}");
    }
    Ok(())
}

struct PtyProcess {
    master: File,
    child_pid: libc::pid_t,
}

impl PtyProcess {
    fn spawn<S, I>(program: &Path, args: I) -> anyhow::Result<Self>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        let (master, slave) = open_pty_pair()?;
        let child_pid = spawn_pty_child(program, &args.into_iter().collect::<Vec<_>>(), &slave)?;
        drop(slave);
        Ok(Self {
            master: File::from(master),
            child_pid,
        })
    }

    fn clone_reader(&self) -> anyhow::Result<File> {
        self.master
            .try_clone()
            .context("failed to clone PTY master")
    }

    fn forward_output_to_stdout(&self) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
        let reader = self.clone_reader()?;
        Ok(thread::spawn(move || {
            let mut stdout = io::stdout().lock();
            pump_pty_output(reader, &mut stdout)
        }))
    }

    fn type_text(&mut self, text: &str, char_delay: Duration) -> anyhow::Result<()> {
        let mut buffer = [0_u8; 4];
        for ch in text.chars() {
            let encoded = ch.encode_utf8(&mut buffer);
            self.write_bytes(encoded.as_bytes())?;
            if !char_delay.is_zero() {
                thread::sleep(char_delay);
            }
        }
        Ok(())
    }

    fn press_enter(&mut self) -> anyhow::Result<()> {
        self.write_bytes(b"\r")
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.master
            .write_all(bytes)
            .context("failed to write to PTY master")?;
        self.master.flush().context("failed to flush PTY master")?;
        Ok(())
    }

    fn wait(self) -> anyhow::Result<ExitStatus> {
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(self.child_pid, &mut status, 0) };
            if result == self.child_pid {
                return Ok(ExitStatus::from_raw(status));
            }
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("failed to wait for PTY child");
            }
        }
    }
}

fn join_output_thread(handle: JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => bail!("shell demo output thread panicked"),
    }
}

fn open_pty_pair() -> anyhow::Result<(OwnedFd, OwnedFd)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error()).context("failed to open PTY master");
    }

    let master = unsafe { OwnedFd::from_raw_fd(master) };
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error()).context("failed to grant PTY slave access");
    }
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error()).context("failed to unlock PTY slave");
    }

    let slave_path = slave_pty_path(master.as_raw_fd())?;
    let slave = unsafe { libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        return Err(io::Error::last_os_error()).context("failed to open PTY slave");
    }

    Ok((master, unsafe { OwnedFd::from_raw_fd(slave) }))
}

fn slave_pty_path(master_fd: libc::c_int) -> anyhow::Result<CString> {
    let mut buffer = vec![0_u8; 128];
    let code = unsafe { libc::ptsname_r(master_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code)).context("failed to resolve PTY slave path");
    }

    let nul = buffer
        .iter()
        .position(|byte| *byte == 0)
        .context("PTY slave path was not NUL-terminated")?;
    CString::new(&buffer[..nul]).context("PTY slave path contained an interior NUL byte")
}

fn spawn_pty_child<S>(program: &Path, args: &[S], slave: &OwnedFd) -> anyhow::Result<libc::pid_t>
where
    S: AsRef<OsStr>,
{
    let program = cstring_from_os_str(program.as_os_str())
        .with_context(|| format!("invalid shell path: {}", program.display()))?;
    let mut argv = Vec::with_capacity(args.len() + 2);
    argv.push(program.clone());
    for arg in args {
        argv.push(
            cstring_from_os_str(arg.as_ref()).context("argument contained an interior NUL byte")?,
        );
    }
    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error()).context("failed to fork PTY child");
    }
    if pid == 0 {
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
        if unsafe { libc::setsid() } < 0 {
            unsafe { libc::_exit(1) };
        }
        if unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY, 0) } < 0 {
            unsafe { libc::_exit(1) };
        }
        for target_fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if unsafe { libc::dup2(slave.as_raw_fd(), target_fd) } < 0 {
                unsafe { libc::_exit(1) };
            }
        }
        if slave.as_raw_fd() > libc::STDERR_FILENO {
            unsafe {
                libc::close(slave.as_raw_fd());
            }
        }

        unsafe {
            libc::execvp(program.as_ptr(), argv_ptrs.as_ptr());
            libc::_exit(127);
        }
    }

    Ok(pid)
}

fn cstring_from_os_str(value: &OsStr) -> anyhow::Result<CString> {
    CString::new(value.as_bytes()).context("value contained an interior NUL byte")
}

fn pump_pty_output(mut reader: File, writer: &mut impl Write) -> anyhow::Result<()> {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                writer
                    .write_all(&buffer[..read])
                    .context("failed to write PTY output")?;
                writer.flush().context("failed to flush PTY output")?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if is_pty_end_of_output(&error) => return Ok(()),
            Err(error) => return Err(error).context("failed to read PTY output"),
        }
    }
}

fn is_pty_end_of_output(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_process_receives_typed_input() {
        let mut process = PtyProcess::spawn(
            Path::new("/bin/sh"),
            [
                "-lc",
                r#"IFS= read -r line; printf 'captured:%s\n' "$line""#,
            ],
        )
        .unwrap();
        let reader = process.clone_reader().unwrap();

        process
            .type_text("echo typed through speaches-companion", Duration::ZERO)
            .unwrap();
        process.press_enter().unwrap();

        let status = process.wait().unwrap();
        assert!(status.success());

        let output = collect_pty_output(reader).unwrap();
        assert!(output.contains("captured:echo typed through speaches-companion"));
    }

    #[test]
    fn default_shell_path_falls_back_to_bin_sh() {
        let path = default_shell_path();
        assert!(path.is_absolute());
    }

    fn collect_pty_output(reader: File) -> anyhow::Result<String> {
        let mut output = Vec::new();
        pump_pty_output(reader, &mut output)?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }
}
