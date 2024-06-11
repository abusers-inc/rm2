use std::{collections::HashMap, path::PathBuf, process::Stdio};

use interprocess::local_socket::tokio::prelude::*;
use tokio::fs::OpenOptions;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
mod config;

pub struct ProcessStreams {
    stdout: ChildStdout,
    stderr: ChildStderr,
}
pub struct ProcessRuntime {
    child: Child,
    streams: ProcessStreams,
}

pub struct DaemonProcess {
    runtime: ProcessRuntime,
    state: ProcessState,
}

mod logger {
    use std::convert::Infallible;

    use shared::proto::{Log, LogKind};
    use shared::ProcID;
    use shared::{proto::ProcessAction, ProcessConfig, ProcessState};
    use tokio::io::AsyncReadExt;
    use tokio::sync::broadcast;
    use tokio::sync::mpsc::{self, error::SendError};

    use crate::ProcessStreams;

    pub struct Duplex<T, Y> {
        tx: mpsc::Sender<T>,
        rx: mpsc::Receiver<Y>,
    }
    impl<T, Y> Duplex<T, Y> {
        pub fn pair(buffer: usize) -> (Duplex<T, Y>, Duplex<Y, T>) {
            let (tx1, rx1) = mpsc::channel(buffer);
            let (tx2, rx2) = mpsc::channel(buffer);

            (Duplex { tx: tx1, rx: rx2 }, Duplex { tx: tx2, rx: rx1 })
        }

        pub async fn recv(&mut self) -> Option<Y> {
            self.rx.recv().await
        }

        pub async fn send(&self, message: T) -> Result<(), SendError<T>> {
            self.tx.send(message).await
        }
    }

    pub struct Logger {
        proc_id: ProcID,
        vent: broadcast::Sender<Log>,

        stream: ProcessStreams,
    }

    use shared::proto::LOG_BUFFER_SIZE;
    impl Logger {
        pub fn new(
            vent: broadcast::Sender<Log>,
            streams: ProcessStreams,
            proc_id: ProcID,
        ) -> tokio::task::JoinHandle<std::io::Error> {
            tokio::task::spawn(async move {
                return Self {
                    vent,
                    stream: streams,
                    proc_id,
                }
                .work()
                .await
                .unwrap_err();
            })
        }

        async fn process_buffer(&mut self, buffer: &mut Vec<u8>, kind: LogKind) {
            let mut line_end_index = None;
            for index in 0..buffer.len() {
                let current_elem = buffer[index] as char;

                match current_elem {
                    '\r' => {
                        if index + 1 < buffer.len() {
                            if buffer[index + 1] as char == '\n' {
                                line_end_index = Some(index + 1);
                                break;
                            }
                        }
                    }

                    '\n' => {
                        line_end_index = Some(index);
                        break;
                    }
                }
            }

            if let Some(end_index) = line_end_index {
                let to_send = buffer.drain(0..=end_index).collect();
                self.vent
                    .send(Log {
                        process: self.proc_id,
                        kind,
                        data: to_send,
                    })
                    .await;
            }
        }

        pub async fn work(mut self) -> Result<Infallible, std::io::Error> {
            let mut stdout_buffer = vec![0; LOG_BUFFER_SIZE];
            let mut stderr_buffer = vec![0; LOG_BUFFER_SIZE];

            let mut stdout_buffer_permanent: Vec<u8> = Vec::with_capacity(LOG_BUFFER_SIZE);
            let mut stderr_buffer_permanent: Vec<u8> = Vec::with_capacity(LOG_BUFFER_SIZE);

            loop {
                tokio::select! {
                    stdout_read_count = self.stream.stdout.read(stdout_buffer.as_mut_slice()) => {
                        let read_count = stdout_read_count?;
                        if read_count > 0{
                            stdout_buffer_permanent.extend(&stdout_buffer[0..read_count]);
                            stdout_buffer = vec![0; LOG_BUFFER_SIZE];


                            self.process_buffer(&mut stdout_buffer_permanent, LogKind::Stdout).await;

                        }
                    },
                    stderr_read_count = self.stream.stderr.read(stderr_buffer.as_mut_slice()) => {
                        let read_count = stderr_read_count?;
                        if read_count > 0{
                            stderr_buffer_permanent.extend(&stderr_buffer[0..read_count]);
                            stderr_buffer = vec![0; LOG_BUFFER_SIZE];

                            self.process_buffer(&mut stderr_buffer_permanent, LogKind::Stderr).await;


                        }

                    }
                }
            }
        }
    }
}

use shared::{formats::Toml, *};
pub struct Daemon<F: BytesFormat> {
    listener: LocalSocketListener,

    proc_configs: config::Manager<F, Vec<Process>>,

    processes: HashMap<ProcID, DaemonProcess>,
}

impl<F: BytesFormat> Daemon<F> {
    pub async fn new(mut config: config::Manager<F, Vec<Process>>) {
        let proc_configs = config.read().await.unwrap();

        let mut processes = HashMap::new();

        for config in proc_configs.iter() {
            let runtime = Self::run_process(&config.config).unwrap();
            let daemon_proc = DaemonProcess {
                runtime,
                state: ProcessState {
                    status: Status::Starting,
                    circuit_breaker_state: None,
                },
            };

            processes.insert(config.id, daemon_proc);
        }
    }

    fn run_process(config: &ProcessConfig) -> Result<ProcessRuntime, std::io::Error> {
        let parent_dir = {
            let mut tmp = config.path.clone();
            tmp.pop();
            tmp
        };
        let mut cmd = Command::new(config.path.clone())
            .current_dir(parent_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .args(config.args.clone())
            .spawn()?;

        let entry = ProcessRuntime {
            stdout: cmd.stdout.take().unwrap(),
            stderr: cmd.stderr.take().unwrap(),
            child: cmd,
        };

        Ok(entry)
    }
}

fn get_config_dir() -> PathBuf {
    let mut userhome = homedir::get_my_home().unwrap().unwrap();
    userhome.push("/.rm2/");
    userhome
}

fn get_default_config_format() -> impl BytesFormat {
    Toml
}

#[tokio::main]
async fn main() {
    const PROCS_CONFIG_FILENAME: &str = "processes.toml";
    let daemon_dir = get_config_dir();
    // temporary ignored
    let _ = tokio::fs::create_dir_all(&daemon_dir).await;

    let processes = config::Manager::new(
        daemon_dir.join(PROCS_CONFIG_FILENAME),
        get_default_config_format(),
    )
    .await
    .unwrap();

    let daemon = Daemon::new(processes).await;
}
