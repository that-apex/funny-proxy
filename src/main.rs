use tokio::net::TcpListener;

mod connection;
mod packet;

#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:25565").await.unwrap();

    loop {
        let (socket, _) = listener.accept().await.unwrap();

        tokio::spawn(async move {
            connection::Connection::create(socket).process().await;
        });
    }
}
