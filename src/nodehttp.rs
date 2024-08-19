use chrono::Utc;
use std::error::Error;
use std::fmt::Write;
use std::future::Future;
use std::io;
use std::net::Ipv4Addr;
use std::pin::Pin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// Define a type alias for the request handler function
// FIXME: AsyncMut
type RequestHandler =
    fn(Request, Response) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send>>;

pub struct Request {
    pub method: String,
    pub path: String,
}

pub struct Response {
    stream: TcpStream,
}

impl Response {
    pub async fn write_head(
        &mut self,
        status_code: u16,
        headers: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
    ) -> io::Result<()> {
        let date = Utc::now().to_rfc2822();

        let mut response_header = format!(
            "HTTP/1.1 {status_code} OK\r\n\
            Date: {date}\r\n\
            Connection: keep-alive\r\n\
            Keep-Alive: timeout=5\r\n\
            Transfer-Encoding: chunked\r\n"
        );

        for (key, value) in headers {
            // FIXME: use .into_ok() later
            write!(
                &mut response_header,
                "{}: {}\r\n",
                key.as_ref(),
                value.as_ref()
            )
            .unwrap();
        }

        response_header.push_str("\r\n"); // End of headers

        self.stream.write_all(response_header.as_bytes()).await
    }

    pub async fn end(&mut self, body: &str) -> io::Result<()> {
        let body_len = body.len();
        let mut chunked_body = String::new();

        // Add chunked transfer encoding
        // FIXME: use .into_ok() later
        write!(
            &mut chunked_body,
            "{body_len:X}\r\n\
            {body}\r\n\
            0\r\n\r\n" // End of chunks
                       // what is the 0 here
        )
        .unwrap();

        self.stream.write_all(chunked_body.as_bytes()).await?;
        self.stream.flush().await
    }
}

pub fn create_server(handler: RequestHandler) -> Server {
    Server { handler }
}

pub struct Server {
    handler: RequestHandler,
}

impl Server {
    pub async fn listen(self, port: u16, on_listen: fn()) -> io::Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
        on_listen();

        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, self.handler).await {
                    todo!("{e}")
                }
            });
        }
    }
}

async fn handle_connection(stream: TcpStream, handler: RequestHandler) -> io::Result<()> {
    let mut buffer = [0; 512];
    let mut stream = Response { stream };
    stream.stream.read(&mut buffer).await?;

    let request_line = String::from_utf8_lossy(&buffer);

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let request = Request { method, path };
    if let Err(e) = handler(request, stream).await {
        todo!("{e}")
    }

    Ok(())
}
