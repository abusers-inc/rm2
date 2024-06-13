use std::{collections::HashMap, path::PathBuf, process::Stdio};

use interprocess::local_socket::tokio::prelude::*;
use shared::proto::transport::TransportSocket;
use shared::proto::{DaemonMessage, DaemonNotification, Log, Subscription};
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
        ) -> tokio::task::JoinHandle<(ProcID, std::io::Error)> {
            tokio::task::spawn(async move {
                return (
                    proc_id,
                    Self {
                        vent,
                        stream: streams,
                        proc_id,
                    }
                    .work()
                    .await
                    .unwrap_err(),
                );
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
                    _ => (),
                }
            }

            if let Some(end_index) = line_end_index {
                let to_send = buffer.drain(0..=end_index).collect();
                self.vent.send(Log {
                    process: self.proc_id,
                    kind,
                    data: to_send,
                });
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

pub mod connection {
    use std::{collections::HashMap, sync::Arc};
    use tokio::{sync::mpsc, task::JoinSet};

    use shared::{
        proto::{
            self,
            transport::{self, TransportSocket},
            ClientMessage, DaemonMessage, DaemonNotification, Subscription,
        },
        BytesFormat, Listenerlike, Process, Stream,
    };
    use tokio::sync::Mutex;

    use super::logger::Duplex;

    type ConnId = u64;

    fn assign_connection_id() -> ConnId {
        static ID_COUNTER: std::sync::Mutex<ConnId> = std::sync::Mutex::new(0);
        let mut lock = ID_COUNTER.lock().unwrap();
        let old = *lock;
        *lock += 1;
        old
    }

    const MANAGER_BUFFER: usize = 10_000;

    struct Connection<Format: BytesFormat> {
        id: ConnId,
        subscriptions: Vec<Subscription>,
        send_chan: mpsc::Sender<DaemonMessage>,

        __format: std::marker::PhantomData<Format>,
    }

    impl<F: BytesFormat + 'static> Connection<F> {
        pub fn new<S: Stream + 'static>(
            id: ConnId,
            socket_chan: mpsc::Sender<(ConnId, ClientMessage)>,
            conn: TransportSocket<S, F>,
        ) -> (
            Self,
            impl std::future::Future<Output = Result<ConnId, (ConnId, transport::Error<F>)>>,
        ) {
            let (tx, rx) = mpsc::channel(MANAGER_BUFFER);
            let task = async move { Self::work(id, socket_chan, rx, conn).await };
            (
                Self {
                    id,
                    subscriptions: Vec::new(),
                    send_chan: tx,
                    __format: std::marker::PhantomData,
                },
                task,
            )
        }

        async fn work<S: Stream>(
            id: ConnId,
            socket_chan: mpsc::Sender<(ConnId, ClientMessage)>,
            mut daemon_chan: mpsc::Receiver<DaemonMessage>,
            mut conn: TransportSocket<S, F>,
        ) -> Result<ConnId, (ConnId, transport::Error<F>)> {
            let coroutine = || async move {
                loop {
                    tokio::select! {
                        conn_msg = conn.recv::<ClientMessage>() => {
                            let conn_msg = conn_msg?;

                            // means network manager was closed
                            if let Err(_) = socket_chan.send((id, conn_msg)).await {
                                return Ok(id);
                            }
                        }

                        daemon_msg = daemon_chan.recv() => {
                            let daemon_msg = match daemon_msg {
                                Some(msg) => msg,
                                None => {
                                    return Ok(id);
                                }
                            };

                            if let Err(_) = conn.send(&daemon_msg).await {
                                return Ok(id);
                            }
                        }
                    }
                }
            };

            let result = coroutine().await.map_err(|x| (id, x));

            result
        }
    }

    pub struct ConnectionManager<L: Listenerlike, Format: BytesFormat> {
        listener: L,
        connections: HashMap<ConnId, Connection<Format>>,
        chan: mpsc::Receiver<(ConnId, ClientMessage)>,

        connections_workers: JoinSet<Result<ConnId, (ConnId, transport::Error<Format>)>>,

        // for clonning purposes
        _send: mpsc::Sender<(ConnId, ClientMessage)>,
        __format: std::marker::PhantomData<*const Format>,
    }

    impl<L: Listenerlike, Format: BytesFormat + 'static> ConnectionManager<L, Format> {
        pub fn new(listener: L) -> Self {
            let (tx, rx) = mpsc::channel(MANAGER_BUFFER);

            Self {
                listener,
                connections: HashMap::new(),
                connections_workers: JoinSet::new(),
                chan: rx,
                _send: tx,
                __format: std::marker::PhantomData,
            }
        }

        pub async fn listen(listener: &mut L) -> Result<L::Stream, std::io::Error> {
            let new_stream = listener.accept().await?;
            Ok(new_stream)
        }

        pub async fn dispatch(&mut self, notif: DaemonNotification, associated_process: &Process) {
            'conn_loop: for conn in self.connections.values_mut() {
                for subscription in conn.subscriptions.iter() {
                    let is_selected = match &subscription.selector {
                        shared::Selector::All => true,
                        shared::Selector::Group(group)
                            if associated_process.config.groups.contains(&group) =>
                        {
                            true
                        }
                        shared::Selector::Particular { id } if &associated_process.id == id => true,
                        _ => false,
                    };

                    if !is_selected {
                        continue;
                    }

                    let is_event_matched = cond::cond! {
                        notif.is_log() && subscription.kind.is_log() => true,
                        notif.is_state_update() && subscription.kind.is_status() => true,

                      _ => false
                    };

                    if !is_event_matched {
                        continue;
                    }

                    // TODO: Check if error and remove connection if so
                    conn.send_chan
                        .send(DaemonMessage::Notification(notif.clone()))
                        .await;

                    continue 'conn_loop;
                }
            }
        }

        pub async fn run(&mut self) -> (ConnId, ClientMessage) {
            loop {
                tokio::select! {
                   msg_from_conn = self.chan.recv() => {
                        let msg = msg_from_conn.expect("We keep a copy of sender, so it should not panic");

                        return msg;
                   }

                   new_conn = Self::listen(&mut self.listener) => {
                        let new_conn = match new_conn {
                            Ok(x) => x,
                            Err(_) => {continue;}
                        };
                        let new_conn_id = assign_connection_id();
                        let (conn, task) = Connection::new(new_conn_id, self._send.clone(), TransportSocket::<_, Format>::new(new_conn));

                        self.connections_workers.spawn(task);

                        self.connections.insert(new_conn_id, conn);
                   }

                   res = self.connections_workers.join_next() => {
                        if let Some(res) = res {
                            match res {
                                Ok(task_result) => {

                                    let id = match task_result {
                                        Ok(id) => id,
                                        Err((id, _)) => id
                                    };
                                    // remove connection from pool
                                    self.connections.remove(&id);
                                }
                                Err(err) => {
                                    if err.is_panic() {
                                        std::panic::resume_unwind(err.into_panic());
                                    }


                                },
                            }
                        }
                        }
                }
            }
        }
    }
}

use shared::{formats::Toml, *};
use tokio::sync::broadcast;
pub struct Daemon<F: BytesFormat> {
    proc_configs: config::Manager<F, Vec<Process>>,

    processes: HashMap<ProcID, DaemonProcess>,

    log_pipe: broadcast::Receiver<Log>,
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
            streams: ProcessStreams {
                stdout: cmd.stdout.take().unwrap(),
                stderr: cmd.stderr.take().unwrap(),
            },
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
