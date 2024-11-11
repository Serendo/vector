use std::{path::PathBuf, pin::Pin, time::Duration};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{stream::BoxStream, SinkExt, StreamExt};
use snafu::{ResultExt, Snafu};
use tokio::{
    net::{UnixDatagram, UnixStream},
    time::sleep,
};
use tokio_util::codec::Encoder;
use vector_lib::event::EventStatus;
use vector_lib::json_size::JsonSize;
use vector_lib::{
    configurable::configurable_component,
    internal_event::{ByteSize, BytesSent, InternalEventHandle, Protocol},
};
use vector_lib::{ByteSizeOf, EstimatedJsonEncodedSizeOf};

use crate::{
    codecs::Transformer,
    event::{Event, Finalizable},
    internal_events::{
        ConnectionOpen, OpenGauge, SocketEventsSent, SocketMode, UnixSendIncompleteError,
        UnixSocketConnectionEstablished, UnixSocketOutgoingConnectionError, UnixSocketSendError,
    },
    sink_ext::VecSinkExt,
    sinks::{
        util::{
            retries::ExponentialBackoff,
            service::net::UnixMode,
            socket_bytes_sink::{BytesSink, ShutdownCheck},
            EncodedEvent, StreamSink,
        },
        Healthcheck, VectorSink,
    },
};

#[derive(Debug, Snafu)]
pub enum UnixError {
    #[snafu(display("Failed connecting to socket at path {}: {}", path.display(), source))]
    ConnectionError {
        source: tokio::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Failed to bind socket: {}.", source))]
    FailedToBind { source: std::io::Error },
}

/// A Unix Domain Socket sink.
#[configurable_component]
#[derive(Clone, Debug)]
pub struct UnixSinkConfig {
    /// The Unix socket path.
    ///
    /// This should be an absolute path.
    #[configurable(metadata(docs::examples = "/path/to/socket"))]
    pub path: PathBuf,

    /// The Unix socket mode to use.
    #[serde(default = "default_unix_mode")]
    unix_mode: UnixMode,
}

const fn default_unix_mode() -> UnixMode {
    UnixMode::Stream
}

impl UnixSinkConfig {
    pub const fn new(path: PathBuf, unix_mode: UnixMode) -> Self {
        Self { path, unix_mode }
    }

    pub fn build(
        &self,
        transformer: Transformer,
        encoder: impl Encoder<Event, Error = vector_lib::codecs::encoding::Error>
            + Clone
            + Send
            + Sync
            + 'static,
    ) -> crate::Result<(VectorSink, Healthcheck)> {
        let connector = UnixConnector::new(self.path.clone(), self.unix_mode);
        let sink = UnixSink::new(connector.clone(), transformer, encoder);
        Ok((
            VectorSink::from_event_streamsink(sink),
            Box::pin(async move { connector.healthcheck().await }),
        ))
    }
}

enum UnixEither {
    Datagram(UnixDatagram),
    Stream(UnixStream),
}

#[derive(Debug, Clone)]
struct UnixConnector {
    pub path: PathBuf,
    mode: UnixMode,
}

impl UnixConnector {
    const fn new(path: PathBuf, mode: UnixMode) -> Self {
        Self { path, mode }
    }

    const fn fresh_backoff() -> ExponentialBackoff {
        // TODO: make configurable
        ExponentialBackoff::from_millis(2)
            .factor(250)
            .max_delay(Duration::from_secs(60))
    }

    async fn connect(&self) -> Result<UnixEither, UnixError> {
        match self.mode {
            UnixMode::Stream => UnixStream::connect(&self.path)
                .await
                .context(ConnectionSnafu {
                    path: self.path.clone(),
                })
                .map(UnixEither::Stream),
            UnixMode::Datagram => {
                UnixDatagram::unbound()
                    .context(FailedToBindSnafu)
                    .and_then(|datagram| {
                        datagram
                            .connect(&self.path)
                            .context(ConnectionSnafu {
                                path: self.path.clone(),
                            })
                            .map(|_| UnixEither::Datagram(datagram))
                    })
            }
        }
    }

    async fn connect_backoff(&self) -> UnixEither {
        let mut backoff = Self::fresh_backoff();
        loop {
            match self.connect().await {
                Ok(stream) => {
                    emit!(UnixSocketConnectionEstablished { path: &self.path });
                    return stream;
                }
                Err(error) => {
                    emit!(UnixSocketOutgoingConnectionError { error });
                    sleep(backoff.next().unwrap()).await;
                }
            }
        }
    }

    async fn healthcheck(&self) -> crate::Result<()> {
        self.connect().await.map(|_| ()).map_err(Into::into)
    }
}

struct UnixSink<E>
where
    E: Encoder<Event, Error = vector_lib::codecs::encoding::Error> + Clone + Send + Sync,
{
    connector: UnixConnector,
    transformer: Transformer,
    encoder: E,
}

impl<E> UnixSink<E>
where
    E: Encoder<Event, Error = vector_lib::codecs::encoding::Error> + Clone + Send + Sync,
{
    pub const fn new(connector: UnixConnector, transformer: Transformer, encoder: E) -> Self {
        Self {
            connector,
            transformer,
            encoder,
        }
    }

    async fn connect(&mut self) -> BytesSink<UnixStream> {
        let stream = match self.connector.connect_backoff().await {
            UnixEither::Stream(stream) => stream,
            UnixEither::Datagram(_) => unreachable!("connect is only called with Stream mode"),
        };
        BytesSink::new(stream, |_| ShutdownCheck::Alive, SocketMode::Unix)
    }

    async fn run_internal(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        match self.connector.mode {
            UnixMode::Stream => self.run_stream(input).await,
            UnixMode::Datagram => self.run_datagram(input).await,
        }
    }

    // Same as TcpSink, more details there.
    async fn run_stream(mut self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        let mut encoder = self.encoder.clone();
        let transformer = self.transformer.clone();
        let mut input = input
            .map(|mut event| {
                let byte_size = event.size_of();
                let json_byte_size = event.estimated_json_encoded_size_of();

                transformer.transform(&mut event);

                let finalizers = event.take_finalizers();
                let mut bytes = BytesMut::new();

                // Errors are handled by `Encoder`.
                if encoder.encode(event, &mut bytes).is_ok() {
                    let item = bytes.freeze();
                    EncodedEvent {
                        item,
                        finalizers,
                        byte_size,
                        json_byte_size,
                    }
                } else {
                    EncodedEvent::new(Bytes::new(), 0, JsonSize::zero())
                }
            })
            .peekable();

        while Pin::new(&mut input).peek().await.is_some() {
            let mut sink = self.connect().await;
            let _open_token = OpenGauge::new().open(|count| emit!(ConnectionOpen { count }));

            let result = match sink.send_all_peekable(&mut (&mut input).peekable()).await {
                Ok(()) => sink.close().await,
                Err(error) => Err(error),
            };

            if let Err(error) = result {
                emit!(UnixSocketSendError {
                    error: &error,
                    path: &self.connector.path
                });
            }
        }

        Ok(())
    }

    // Same as UdpSink
    async fn run_datagram(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        let bytes_sent = register!(BytesSent::from(Protocol::UNIX));
        let mut input = input.peekable();

        let mut encoder = self.encoder.clone();
        while Pin::new(&mut input).peek().await.is_some() {
            let mut socket = match self.connector.connect_backoff().await {
                UnixEither::Datagram(datagram) => datagram,
                UnixEither::Stream(_) => {
                    unreachable!("run_datagram is only called with Datagram mode")
                }
            };

            while let Some(mut event) = input.next().await {
                let byte_size = event.estimated_json_encoded_size_of();

                self.transformer.transform(&mut event);

                let finalizers = event.take_finalizers();
                let mut bytes = BytesMut::new();

                // Errors are handled by `Encoder`.
                if encoder.encode(event, &mut bytes).is_err() {
                    continue;
                }

                match udp_send(&mut socket, &bytes).await {
                    Ok(()) => {
                        emit!(SocketEventsSent {
                            mode: SocketMode::Unix,
                            count: 1,
                            byte_size,
                        });

                        bytes_sent.emit(ByteSize(bytes.len()));
                        finalizers.update_status(EventStatus::Delivered);
                    }
                    Err(error) => {
                        emit!(UnixSocketSendError {
                            path: &self.connector.path,
                            error: &error
                        });
                        finalizers.update_status(EventStatus::Errored);
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

async fn udp_send(socket: &mut UnixDatagram, buf: &[u8]) -> tokio::io::Result<()> {
    let sent = socket.send(buf).await?;
    if sent != buf.len() {
        emit!(UnixSendIncompleteError {
            data_size: buf.len(),
            sent,
        });
    }
    Ok(())
}

#[async_trait]
impl<E> StreamSink<Event> for UnixSink<E>
where
    E: Encoder<Event, Error = vector_lib::codecs::encoding::Error> + Clone + Send + Sync,
{
    async fn run(mut self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
        self.run_internal(input).await
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;
    use vector_lib::codecs::{
        encoding::Framer, BytesEncoder, NewlineDelimitedEncoder, TextSerializerConfig,
    };

    use super::*;
    use crate::{
        codecs::Encoder,
        test_util::{
            components::{assert_sink_compliance, SINK_TAGS},
            random_lines_with_stream, CountReceiver,
        },
    };

    fn temp_uds_path(name: &str) -> PathBuf {
        tempfile::tempdir().unwrap().into_path().join(name)
    }

    #[tokio::test]
    async fn unix_sink_healthcheck() {
        let good_path = temp_uds_path("valid_stream_uds");
        let _listener = UnixListener::bind(&good_path).unwrap();
        assert!(UnixSinkConfig::new(good_path.clone(), UnixMode::Stream)
            .build(
                Default::default(),
                Encoder::<()>::new(TextSerializerConfig::default().build().into())
            )
            .unwrap()
            .1
            .await
            .is_ok());
        assert!(
            UnixSinkConfig::new(good_path.clone(), UnixMode::Datagram)
                .build(
                    Default::default(),
                    Encoder::<()>::new(TextSerializerConfig::default().build().into())
                )
                .unwrap()
                .1
                .await
                .is_err(),
            "datagram mode should fail when attempting to send into a stream mode UDS"
        );

        let bad_path = temp_uds_path("no_one_listening");
        assert!(UnixSinkConfig::new(bad_path.clone(), UnixMode::Stream)
            .build(
                Default::default(),
                Encoder::<()>::new(TextSerializerConfig::default().build().into())
            )
            .unwrap()
            .1
            .await
            .is_err());
        assert!(UnixSinkConfig::new(bad_path.clone(), UnixMode::Datagram)
            .build(
                Default::default(),
                Encoder::<()>::new(TextSerializerConfig::default().build().into())
            )
            .unwrap()
            .1
            .await
            .is_err());
    }

    #[tokio::test]
    async fn basic_unix_sink() {
        let num_lines = 1000;
        let out_path = temp_uds_path("unix_test");

        // Set up server to receive events from the Sink.
        let mut receiver = CountReceiver::receive_lines_unix(out_path.clone());

        // Set up Sink
        let config = UnixSinkConfig::new(out_path, UnixMode::Stream);
        let (sink, _healthcheck) = config
            .build(
                Default::default(),
                Encoder::<Framer>::new(
                    NewlineDelimitedEncoder::default().into(),
                    TextSerializerConfig::default().build().into(),
                ),
            )
            .unwrap();

        // Send the test data
        let (input_lines, events) = random_lines_with_stream(100, num_lines, None);

        assert_sink_compliance(&SINK_TAGS, async move { sink.run(events).await })
            .await
            .expect("Running sink failed");

        // Wait for output to connect
        receiver.connected().await;

        // Receive the data sent by the Sink to the receiver
        assert_eq!(input_lines, receiver.await);
    }

    #[tokio::test]
    async fn basic_unix_datagram_sink() {
        let num_lines = 1000;
        let out_path = temp_uds_path("unix_datagram_test");

        // Set up listener to receive events from the Sink.
        let receiver = UnixDatagram::bind(out_path.clone()).unwrap();

        // Listen in the background to avoid blocking
        let handle = tokio::spawn(async move {
            let mut output_lines = Vec::<String>::with_capacity(num_lines);

            for _ in 0..num_lines {
                let mut buf = [0; 256];
                let size = receiver
                    .recv(&mut buf)
                    .await
                    .expect("Did not receive message");
                let line = String::from_utf8_lossy(&buf[..size]).to_string();
                output_lines.push(line);
            }

            output_lines
        });

        // Set up Sink
        let config = UnixSinkConfig::new(out_path.clone(), UnixMode::Datagram);
        let (sink, _healthcheck) = config
            .build(
                Default::default(),
                Encoder::<Framer>::new(
                    BytesEncoder.into(),
                    TextSerializerConfig::default().build().into(),
                ),
            )
            .unwrap();

        // Send the test data
        let (input_lines, events) = random_lines_with_stream(100, num_lines, None);

        assert_sink_compliance(&SINK_TAGS, async move { sink.run(events).await })
            .await
            .expect("Running sink failed");

        // Receive the data sent by the Sink to the receiver
        let output_lines = handle.await.expect("UDS Datagram receiver failed");

        assert_eq!(input_lines, output_lines);
    }
}
