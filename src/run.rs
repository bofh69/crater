#![deny(unused_must_use)]

use dirs::{CARGO_HOME, RUSTUP_HOME};
use errors::*;
use futures::{future, Future, Stream};
use futures_cpupool::CpuPool;
use native;
use slog_scope;
use std::convert::AsRef;
use std::ffi::OsStr;
use std::io::{self, BufReader};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio_core::reactor::Core;
use tokio_io::io::lines;
use tokio_process::CommandExt;
use tokio_timer;

pub struct RunCommand<'a, S: AsRef<OsStr> + 'a> {
    name: &'a str,
    args: &'a [S],
    env: Vec<(&'a str, &'a str)>,
    cd: Option<&'a Path>,
    quiet: bool,
    enable_timeout: bool,
}

impl<'a, S: AsRef<OsStr>> RunCommand<'a, S> {
    pub fn new(name: &'a str, args: &'a [S]) -> Self {
        RunCommand {
            name,
            args,
            env: Vec::new(),
            cd: None,
            quiet: false,
            enable_timeout: true,
        }
    }

    pub fn env(mut self, key: &'a str, value: &'a str) -> Self {
        self.env.push((key, value));
        self
    }

    pub fn cd(mut self, path: &'a Path) -> Self {
        self.cd = Some(path);
        self
    }

    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    pub fn enable_timeout(mut self, enable_timeout: bool) -> Self {
        self.enable_timeout = enable_timeout;
        self
    }

    pub fn local_rustup(self) -> Self {
        self.env("CARGO_HOME", &*CARGO_HOME)
            .env("RUSTUP_HOME", &*RUSTUP_HOME)
    }

    pub fn run(self) -> Result<()> {
        self.run_inner(false)?;
        Ok(())
    }

    pub fn run_capture(self) -> Result<(Vec<String>, Vec<String>)> {
        let out = self.run_inner(true)?;
        Ok((out.stdout, out.stderr))
    }

    fn run_inner(&self, capture: bool) -> Result<ProcessOutput> {
        let mut cmd = Command::new(self.name);

        cmd.args(self.args);
        for &(k, v) in &self.env {
            cmd.env(k, v);
        }

        let cmdstr = format!("{:?}", cmd);

        if let Some(cd) = self.cd {
            cmd.current_dir(cd);
        }

        info!("running `{}`", cmdstr);
        let out = log_command(cmd, capture, self.quiet, self.enable_timeout).map_err(|e| {
            info!("error running command: {}", e);
            e
        })?;

        if out.status.success() {
            Ok(out)
        } else {
            Err(format!("command `{}` failed", cmdstr).into())
        }
    }
}

struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

const MAX_TIMEOUT_SECS: u64 = 60 * 15;
const HEARTBEAT_TIMEOUT_SECS: u64 = 60 * 5;

fn log_command(
    mut cmd: Command,
    capture: bool,
    quiet: bool,
    enable_timeout: bool,
) -> Result<ProcessOutput> {
    let (max_timeout, heartbeat_timeout) = if enable_timeout {
        let max_timeout = Duration::from_secs(MAX_TIMEOUT_SECS);
        let heartbeat_timeout = if quiet {
            // If the command is known to be slow, the heartbeat timeout is set to the same value as
            // the max timeout, so it can't be triggered.
            max_timeout
        } else {
            Duration::from_secs(HEARTBEAT_TIMEOUT_SECS)
        };

        (max_timeout, heartbeat_timeout)
    } else {
        // If timeouts are disabled just use a *really* long timeout
        let max = Duration::from_secs(7 * 24 * 60 * 60);
        (max, max)
    };

    let mut core = Core::new().unwrap();
    let timer = tokio_timer::wheel().max_timeout(max_timeout * 2).build();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_async(&core.handle())?;

    let stdout = child.stdout().take().expect("");
    let stderr = child.stderr().take().expect("");

    // Needed for killing after timeout
    let child_id = child.id();

    let logger = slog_scope::logger();
    let stdout = lines(BufReader::new(stdout)).map({
        let logger = logger.clone();
        move |line| {
            slog_info!(logger, "blam! {}", line);
            line
        }
    });
    let stderr = lines(BufReader::new(stderr)).map({
        let logger = logger.clone();
        move |line| {
            slog_info!(logger, "kablam! {}", line);
            line
        }
    });

    let output = Stream::select(stdout.map(future::Either::A), stderr.map(future::Either::B));
    let output = timer
        .timeout_stream(output, heartbeat_timeout)
        .map_err(move |e| {
            if e.kind() == io::ErrorKind::TimedOut {
                match native::kill_process(child_id) {
                    Err(err) => err,
                    Ok(()) => Error::from(ErrorKind::Timeout(
                        "not generating output for ",
                        heartbeat_timeout.as_secs(),
                    )),
                }
            } else {
                e.into()
            }
        });

    let output = if capture {
        unmerge(output)
    } else {
        Box::new(
            output
                .for_each(|_| Ok(()))
                .and_then(|_| Ok((Vec::new(), Vec::new()))),
        )
    };
    let pool = CpuPool::new(1);
    let output = pool.spawn(output);

    let child = timer.timeout(child, max_timeout).map_err(move |e| {
        if e.kind() == io::ErrorKind::TimedOut {
            match native::kill_process(child_id) {
                Err(err) => err,
                Ok(()) => ErrorKind::Timeout("max time of", MAX_TIMEOUT_SECS).into(),
            }
        } else {
            e.into()
        }
    });

    // TODO: Handle errors from tokio_timer better, in particular TimerError::TooLong
    let (status, (stdout, stderr)) = core.run(child.select2(output).then(|res| {
        let future: Box<Future<Item = _, Error = _>> = match res {
            // child exited, finish collecting output
            Ok(future::Either::A((status, output))) => {
                Box::new(output.map(move |sose| (status, sose)))
            }
            // output finished, wait for process to exit (possibly being killed by timeout)
            Ok(future::Either::B((sose, child))) => {
                Box::new(child.map(move |status| (status, sose)))
            }
            // child lived too long and was killed, finish collecting output so it goes to logs then
            // return timeout error (not interested in errors with output at this point, so ignore)
            Err(future::Either::A((e, output))) => Box::new(output.then(|_| future::err(e))),
            // output collection failed (timeout, misc io error) and child was killed, drop timeout
            Err(future::Either::B((e, _child))) => Box::new(future::err(e)),
        };
        future
    }))?;

    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg_attr(feature = "cargo-clippy", allow(type_complexity))]
fn unmerge<T1, T2, S>(reader: S) -> Box<Future<Item = (Vec<T1>, Vec<T2>), Error = S::Error> + Send>
where
    S: Stream<Item = future::Either<T1, T2>> + Send + 'static,
    S::Error: Send,
    T1: Send + 'static,
    T2: Send + 'static,
{
    Box::new(
        reader
            .map(|i| match i {
                future::Either::A(l) => (Some(l), None),
                future::Either::B(r) => (None, Some(r)),
            }).fold((Vec::new(), Vec::new()), |mut v, i| {
                if let Some(i) = i.0 {
                    v.0.push(i);
                }
                if let Some(i) = i.1 {
                    v.1.push(i);
                }
                Ok(v)
            }),
    )
}
