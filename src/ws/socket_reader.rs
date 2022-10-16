use futures::stream::SplitStream;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type SocketReaderObj = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

pub struct SocketReader {
    pub reader: SocketReaderObj,
}

impl SocketReader {
    // TODO
}
