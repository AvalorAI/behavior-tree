use anyhow::Result;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::Receiver;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::connect_async;
use url::Url;

use super::socket_reader::SocketReader;
use super::socket_writer::SocketWriter;
use crate::bt::listener::Update;

const CONNECTION_TIME_OUT: u64 = 3; // [s]

#[derive(Debug)]
pub struct SocketConnector {
    socket_url: Url,
    rx: Receiver<Update>,
    bt_export: Value,
}

impl SocketConnector {
    pub fn spawn(socket_url: String, rx: Receiver<Update>, bt_export: Value) -> Result<()> {
        let socket_url = Url::parse(&socket_url)?;
        let connector = SocketConnector::new(socket_url, rx, bt_export);
        tokio::spawn(SocketConnector::serve(connector)); // Start serving itself
        Ok(())
    }

    fn new(socket_url: Url, rx: Receiver<Update>, bt_export: Value) -> Self {
        Self {
            socket_url,
            rx,
            bt_export,
        }
    }

    async fn serve(mut connector: SocketConnector) {
        let (mut writer, mut _reader) = connector.wait_for_connect(&connector.socket_url).await;
        writer.run(&mut connector.rx, connector.bt_export.clone()).await;
    }

    async fn wait_for_connect(&self, socket_url: &Url) -> (SocketWriter, SocketReader) {
        log::info!("Connecting to socket on {}", socket_url.clone());
        loop {
            if let Ok((socket, _response)) = connect_async(socket_url.clone()).await {
                let (writer, reader) = socket.split();
                return (SocketWriter { writer }, SocketReader { reader });
            }
            log::warn!("Trying to connect websocket again in {:?}", CONNECTION_TIME_OUT);
            sleep(Duration::from_secs(CONNECTION_TIME_OUT)).await
        }
    }
}
