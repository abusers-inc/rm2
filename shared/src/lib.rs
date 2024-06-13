use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const DAEMON_SOCKET_NAME: &str = "rm2daemon.sock";

pub type ProcID = u32;

#[derive(Serialize, Deserialize, Clone)]
pub struct CircuitBreakerOptions {}
#[derive(Serialize, Deserialize, Clone)]
pub struct ProcessOptions {
    pub save_logs: bool,

    // if this is None, then circuit breaker is not used
    pub circuit_breaker: Option<CircuitBreakerOptions>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProcessConfig {
    pub path: std::path::PathBuf,
    pub args: Vec<String>,

    pub groups: Vec<String>,

    pub options: ProcessOptions,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Process {
    pub id: ProcID,
    #[serde(flatten)]
    pub config: ProcessConfig,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum Status {
    Starting,
    Running { pid: u32 },
    Stopped,
    Crashed { code: Option<i32> },
}

#[derive(Serialize, Deserialize, Clone)]
pub enum CircuitBreakerState {
    Open,
    HalfOpen,
    Closed,
}

/// Defines how to select process(-es) from pool
#[derive(Serialize, Deserialize, Clone)]
pub enum Selector {
    All,
    Group(String),
    Particular { id: ProcID },
}

/// Runtime state of process
#[derive(Serialize, Deserialize, Clone)]
pub struct ProcessState {
    pub status: Status,

    // Option because process might opted-out of circuit breaker
    pub circuit_breaker_state: Option<CircuitBreakerState>,
}
pub trait BytesFormat: std::fmt::Debug + Send + 'static {
    type SerError: serde::ser::Error + std::fmt::Debug + Send;
    type DeError: serde::de::Error + std::fmt::Debug + Send;

    fn serialize_bytes(input: &impl Serialize) -> Result<Vec<u8>, Self::SerError>;
    fn deserialize_bytes<T: DeserializeOwned>(input: &[u8]) -> Result<T, Self::DeError>;
}

pub mod proto {
    //! Protocol of daemon-client communications
    pub const LOG_BUFFER_SIZE: usize = 128;

    #[derive(Debug, Serialize, Deserialize, Clone)]
    pub enum LogKind {
        Stdout,
        Stderr,
    }
    #[derive(Debug, Serialize, Deserialize, Clone)]

    pub struct Log {
        pub process: ProcID,
        pub kind: LogKind,
        pub data: Vec<u8>,
    }

    use strum_macros::EnumIs;
    #[derive(Serialize, Deserialize, EnumIs)]
    pub enum SubscriptionKind {
        Log,
        Status,
    }

    #[derive(Serialize, Deserialize, Clone, EnumIs)]
    pub enum DaemonNotification {
        StateUpdate { id: ProcID, state: ProcessState },
        Log(Log),
    }

    use super::*;
    #[derive(Serialize, Deserialize, Clone)]
    pub enum DaemonMessage {
        Pong,
        AllProcesses(Vec<(Process, ProcessState)>),
        Notification(DaemonNotification),
    }

    #[derive(Serialize, Deserialize)]
    pub struct Subscription {
        pub kind: SubscriptionKind,
        pub selector: Selector,
    }

    #[derive(Serialize, Deserialize)]
    pub enum ProcessAction {
        Start,
        Restart,
        Stop,
        OpenCircuitBreaker,
    }
    #[derive(Serialize, Deserialize)]
    pub enum ClientMessage {
        Ping,
        RequestEntries,

        Subscribe(Subscription),

        Unsubscribe(Subscription),

        CreateProcess(ProcessConfig),

        UpdateProcessConfig(Process),

        Act {
            action: ProcessAction,
            selector: Selector,
        },
    }

    pub mod transport {
        use std::pin;

        use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
        use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

        use crate::BytesFormat;

        #[derive(Serialize, Deserialize)]
        pub struct Header<T> {
            len: u32,
            data: T,
        }

        #[derive(thiserror::Error)]
        pub enum Error<Format: BytesFormat> {
            Ser(Format::SerError),
            De(Format::DeError),
            IO(#[from] std::io::Error),
        }
        pub struct TransportSocket<Pipe: AsyncRead + AsyncWrite + Unpin + Send, Format: BytesFormat> {
            pub pipe: Pipe,
            pub _f: std::marker::PhantomData<Format>,
        }

        impl<Pipe: AsyncRead + AsyncWrite + Unpin + Send, Format: BytesFormat>
            TransportSocket<Pipe, Format>
        {
            pub fn new(pipe: Pipe) -> Self {
                Self {
                    pipe,
                    _f: std::marker::PhantomData,
                }
            }
            pub async fn send<T: Serialize + for<'a> Deserialize<'a>>(
                &mut self,
                msg: &T,
            ) -> Result<(), Error<Format>> {
                let bytes = Format::serialize_bytes(&msg).map_err(|x| Error::Ser(x))?;
                self.pipe.write_u32(bytes.len() as u32).await?;

                self.pipe.write(bytes.as_ref()).await?;

                Ok(())
            }

            pub async fn recv<E: Serialize + for<'a> Deserialize<'a>>(
                &mut self,
            ) -> Result<E, Error<Format>> {
                let read_size = self.pipe.read_u32().await?;

                let mut buffer = Vec::with_capacity(read_size as usize);

                self.pipe.read_exact(&mut buffer).await?;

                let deserialized =
                    Format::deserialize_bytes(buffer.as_slice()).map_err(|x| Error::De(x))?;

                Ok(deserialized)
            }
        }
    }
}

pub mod formats {
    use serde::de::DeserializeOwned;

    use crate::BytesFormat;

    #[derive(Debug)]
    pub struct MessagePack;
    impl crate::BytesFormat for MessagePack {
        type SerError = rmp_serde::encode::Error;

        type DeError = rmp_serde::decode::Error;

        fn serialize_bytes(input: &impl serde::Serialize) -> Result<Vec<u8>, Self::SerError> {
            rmp_serde::encode::to_vec(input)
        }

        fn deserialize_bytes<T: DeserializeOwned>(input: &[u8]) -> Result<T, Self::DeError> {
            rmp_serde::decode::from_slice(input)
        }
    }

    pub fn get_current_default_network_format() -> impl BytesFormat {
        MessagePack
    }

    // #[derive(thiserror::Error, Debug)]
    // pub enum TomlDeError {
    //     #[error("Toml format failure during deserialization")]
    //     De(#[from] toml::de::Error),
    //     #[error("Toml format failure during conversion to string")]
    //     Utf(#[from] std::str::Utf8Error),
    // }
    // impl serde::de::Error for TomlDeError {
    //     fn custom<T>(msg: T) -> Self
    //     where
    //         T: Display,
    //     {
    //         todo!()
    //     }
    // }
    #[derive(Debug)]

    pub struct Toml;

    impl crate::BytesFormat for Toml {
        type SerError = toml::ser::Error;
        type DeError = toml::de::Error;

        fn serialize_bytes(input: &impl serde::Serialize) -> Result<Vec<u8>, Self::SerError> {
            let as_str = toml::ser::to_string(input)?;
            let as_bytes = as_str.into_bytes();
            Ok(as_bytes)
        }

        fn deserialize_bytes<T: serde::de::DeserializeOwned>(
            input: &[u8],
        ) -> Result<T, Self::DeError> {
            // fix unwrap here
            let as_string = std::str::from_utf8(input).unwrap();
            let output = toml::de::from_str(as_string)?;
            Ok(output)
        }
    }
}

use interprocess::local_socket::{
    tokio::prelude::*, traits::tokio::Listener, GenericNamespaced, ListenerOptions, Name, ToNsName,
};
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Stream: AsyncRead + AsyncWrite + Sized + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Sized + Unpin + Send + 'static> Stream for T {}
pub trait Listenerlike {
    type Stream: Stream;
    async fn accept(&self) -> Result<Self::Stream, std::io::Error>;
}

impl Listenerlike for LocalSocketListener {
    type Stream = <Self as Listener>::Stream;

    async fn accept(&self) -> Result<Self::Stream, std::io::Error> {
        <Self as Listener>::accept(self).await
    }
}

fn get_name() -> Name<'static> {
    DAEMON_SOCKET_NAME
        .to_ns_name::<GenericNamespaced>()
        .unwrap()
}
pub async fn client_connect() -> Result<impl Stream, std::io::Error> {
    interprocess::local_socket::tokio::Stream::connect(get_name()).await
}

pub async fn server_host() -> Result<impl Listenerlike, std::io::Error> {
    ListenerOptions::new().name(get_name()).create_tokio()
}
