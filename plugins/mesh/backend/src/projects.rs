//! Every project of the network, worked on at its own computer (`docs/MESH-IMPLEMENTATION-PLAN.md`
//! §12). Each card carries its computer's projects; `mesh.projects` joins them. The window reaches
//! the computer of the project it shows through a link: a local port whose every connection is
//! carried over the mesh to that computer's frame (`mesh.window` on the member's side), with the
//! token that frame takes (`mesh.window.ticket`). Sessions, files, commands and memory stay at the
//! project's computer; the window only shows them.

use crate::service::{Mesh, lock};
use cc_protocol::codec::write_message;
use cc_protocol::contracts::mesh::{Link, NetworkProject, ProjectsResult};
use cc_protocol::{Message, RpcError, codes};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// node -> the local port that leads to its frame.
#[derive(Default)]
pub struct Links {
    ports: Mutex<HashMap<String, u16>>,
}

impl Mesh {
    /// This computer's projects and every member's, a member's greyed out while it is away.
    pub async fn projects(&self) -> ProjectsResult {
        let mine = self.my_card().await;
        let me = self.me.hex();
        let mut projects: Vec<NetworkProject> = mine.projects.into_iter().map(|project| NetworkProject { project, node: String::new(), computer: self.name(), reachable: true }).collect();
        let cards = lock(&self.cards).clone();
        let conns = self.connections();
        for member in self.members().into_values().filter(|member| member.node != me) {
            let Some(card) = cards.get(&member.node) else { continue };
            for project in &card.projects {
                projects.push(NetworkProject { project: project.clone(), node: member.node.clone(), computer: member.name.clone(), reachable: conns.contains_key(&member.node) });
            }
        }
        ProjectsResult { projects }
    }

    /// A project was added, removed or described here: the card is built again and sent now,
    /// so every computer's sidebar has it within seconds.
    pub async fn projects_changed(self: &Arc<Self>) {
        self.forget_card();
        self.notify("projects");
        self.push_card().await;
    }

    /// The local address of a member's frame and the token it takes.
    pub async fn link(self: &Arc<Self>, node: &str) -> Result<Link, RpcError> {
        if self.connection(node).is_none() {
            let name = self.member(node).map(|member| member.name).unwrap_or_else(|| "The computer".to_owned());
            return Err(RpcError::new(codes::UNAVAILABLE, format!("{name} is not reachable now")).retryable());
        }
        // The token is asked every time: the member's frame takes a new one when it restarts.
        let ticket = self.call_member(node, "mesh.window.ticket", json!({}), None, CancellationToken::new()).await?;
        let token = ticket.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
        let known = lock(&self.links.ports).get(node).copied();
        let port = match known {
            Some(port) => port,
            None => {
                let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| RpcError::internal(format!("cannot open a local port for the link: {e}")))?;
                let port = listener.local_addr().map_err(|e| RpcError::internal(e.to_string()))?.port();
                lock(&self.links.ports).insert(node.to_owned(), port);
                let mesh = self.clone();
                let node = node.to_owned();
                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let mesh = mesh.clone();
                        let node = node.clone();
                        tokio::spawn(async move { mesh.carry(&node, stream).await });
                    }
                });
                port
            }
        };
        Ok(Link { url: format!("http://127.0.0.1:{port}"), token })
    }

    /// One connection of the window to a member's frame: a stream that starts with `mesh.window`
    /// and then carries the bytes both ways until either side closes.
    async fn carry(&self, node: &str, stream: TcpStream) {
        let Some(connection) = self.connection(node) else { return };
        let Ok((mut send, mut recv)) = connection.open_bi().await else { return };
        if write_message(&mut send, &Message::Req { id: 1, method: "mesh.window".to_owned(), params: json!({}) }).await.is_err() {
            return;
        }
        let (mut read, mut write) = stream.into_split();
        let up = async {
            let _ = tokio::io::copy(&mut read, &mut send).await;
            let _ = send.finish();
        };
        let down = async {
            let _ = tokio::io::copy(&mut recv, &mut write).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
        };
        tokio::join!(up, down);
    }
}

/// The member's side of `mesh.window`: the stream's bytes to and from this computer's own frame.
pub async fn serve_window(home: &std::path::Path, send: &mut iroh::endpoint::SendStream, recv: &mut iroh::endpoint::RecvStream) -> Result<(), String> {
    let frame = crate::borrow::frame_info(home)?;
    let stream = TcpStream::connect(("127.0.0.1", frame.port)).await.map_err(|e| format!("cannot reach this computer's frame: {e}"))?;
    let (mut read, mut write) = stream.into_split();
    let up = async {
        let _ = tokio::io::copy(recv, &mut write).await;
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut read, send).await;
        let _ = send.finish();
    };
    tokio::join!(up, down);
    Ok(())
}

/// The token of this computer's frame, for a member's window.
pub fn ticket(home: &std::path::Path) -> Result<Value, RpcError> {
    let frame = crate::borrow::frame_info(home).map_err(|e| RpcError::new(codes::UNAVAILABLE, e))?;
    Ok(json!({"token": frame.token}))
}
