use std::io;
use std::net::{TcpListener, TcpStream};

/// A status server for the configured service.
pub struct Server {
    listener: TcpListener,
}

/// Bind the status server.
pub fn start(addr: &str) -> io::Result<Server> {
    Ok(Server { listener: TcpListener::bind(addr)? })
}

impl Server {
    pub fn local_port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Wait until the first client connects.
    pub fn wait_ready(&self) -> io::Result<TcpStream> {
        let (stream, _) = self.listener.accept()?;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_start() {
        let server = start("127.0.0.1:0").expect("bind");
        assert!(server.local_port() > 0);
        server.wait_ready().expect("ready");
    }
}
