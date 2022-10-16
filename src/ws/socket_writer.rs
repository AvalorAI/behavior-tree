use futures::stream::SplitSink;
use futures_util::SinkExt;
use serde_json::{json, Value};
use tokio::{net::TcpStream, sync::mpsc::Receiver};
use tokio_tungstenite::{tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream};

use crate::Update;

type SocketWriterObj = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

pub struct SocketWriter {
    pub writer: SocketWriterObj,
}

impl SocketWriter {
    async fn request(&mut self, msg: String) {
        let result = &self.writer.send(Message::Text(msg)).await;
        match result {
            Ok(_) => {}
            Err(e) => log::warn!("Socket request failed with error {:?}", e),
        }
    }

    pub async fn run(&mut self, rx: &mut Receiver<Update>, bt_export: Value) {
        log::debug!("Running websocket writer");
        let bt_msg = json!({"type": "tree", "value": bt_export}).to_string();
        self.request(bt_msg).await;
        log::debug!("Sent behavior tree");
        while let Some(update) = rx.recv().await {
            let update_msg = json!({"type": "update", "value": json!(update)}).to_string();
            self.request(update_msg).await;
            log::debug!("Sent node update");
        }

        log::info!("Update receiver became inactive: closing websocket writer");
    }
}
