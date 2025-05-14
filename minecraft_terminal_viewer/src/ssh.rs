// filepath: /home/mike/source/docker-minecraft-rtsp/minecraft_terminal_viewer/src/ssh.rs
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use rand_core::OsRng;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use russh::keys::ssh_key;
use russh::server::*;
use russh::{Channel, ChannelId, Pty};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex as TokioMutex;

use crate::config::{InputEvent, TerminalSize};
use crate::render;

// Type alias for our SSH terminal
type SshTerminal = ratatui::Terminal<CrosstermBackend<TerminalHandle>>;

// Structure to hold client-specific data
struct ClientData {
    pub terminal: SshTerminal,
    pub term_size: Arc<Mutex<TerminalSize>>,
}

// Handle for the SSH terminal to write to
struct TerminalHandle {
    sender: UnboundedSender<Vec<u8>>,
    sink: Vec<u8>,
}

impl TerminalHandle {
    async fn start(handle: Handle, channel_id: ChannelId) -> Self {
        let (sender, mut receiver) = unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                let result = handle.data(channel_id, data.into()).await;
                if result.is_err() {
                    eprintln!("Failed to send data: {:?}", result);
                }
            }
        });
        Self {
            sender,
            sink: Vec::new(),
        }
    }
}

// Implement Write for TerminalHandle to work with CrosstermBackend
impl io::Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.sender.send(self.sink.clone());
        if result.is_err() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                result.unwrap_err(),
            ));
        }

        self.sink.clear();
        Ok(())
    }
}

// Main SSH server implementation
#[derive(Clone)]
pub struct MinecraftSshServer {
    clients: Arc<TokioMutex<HashMap<usize, ClientData>>>,
    id: usize,
}

impl MinecraftSshServer {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(TokioMutex::new(HashMap::new())),
            id: 0,
        }
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let config = Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            auth_rejection_time: std::time::Duration::from_secs(3),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![
                russh::keys::PrivateKey::random(&mut OsRng, ssh_key::Algorithm::Ed25519).unwrap(),
            ],
            nodelay: true,
            ..Default::default()
        };

        // Listen on all interfaces on port 2222
        println!("Starting SSH server on 0.0.0.0:2222");
        println!("Use 'ssh localhost -p 2222' to connect");
        self.run_on_address(Arc::new(config), ("0.0.0.0", 2222))
            .await?;
        
        Ok(())
    }
}

impl Server for MinecraftSshServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        let s = self.clone();
        self.id += 1;
        s
    }
}

impl Handler for MinecraftSshServer {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let terminal_handle = TerminalHandle::start(session.handle(), channel.id()).await;
        
        let backend = CrosstermBackend::new(terminal_handle);
        
        // The correct viewport area will be set when the client requests a pty
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::default()),
        };
        
        let terminal = ratatui::Terminal::with_options(backend, options)?;
        
        // Create initial terminal size (will be updated on pty_request)
        let term_size = Arc::new(Mutex::new(TerminalSize {
            width: 80,
            height: 24,
            target_width: 80,
            target_height: 48, // 16:9 aspect ratio for 80 width
        }));
        
        let mut clients = self.clients.lock().await;
        clients.insert(self.id, ClientData { 
            terminal,
            term_size,
        });
        
        Ok(true)
    }

    async fn auth_publickey(&mut self, _: &str, _: &ssh_key::PublicKey) -> Result<Auth, Self::Error> {
        // Accept any public key for demo purposes
        // In production, you should validate the public key
        Ok(Auth::Accept)
    }
    
    async fn auth_password(&mut self, _: &str, _: &str) -> Result<Auth, Self::Error> {
        // Accept any password for demo purposes
        // In production, you should validate the password
        Ok(Auth::Accept)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Process key input
        match data {
            // Pressing 'q' closes the connection.
            b"q" => {
                self.clients.lock().await.remove(&self.id);
                session.close(channel)?;
            }
            // Pass other keys to the input handler - but not fully implemented here
            _ => {}
        }

        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _: ChannelId,
        col_width: u32,
        row_height: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        let rect = Rect {
            x: 0,
            y: 0,
            width: col_width as u16,
            height: row_height as u16,
        };

        let mut clients = self.clients.lock().await;
        let client_data = clients.get_mut(&self.id).unwrap();
        
        // Update terminal size
        {
            let mut size = client_data.term_size.lock().unwrap();
            size.width = col_width as u16;
            size.height = row_height as u16;
            
            // Calculate target dimensions (must be even height for the block character approach)
            size.target_width = col_width as usize;
            // For proper aspect ratio and block character rendering
            size.target_height = ((size.target_width * 9 / 16 + 1) / 2) * 2;
        }
        
        client_data.terminal.resize(rect)?;

        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        col_width: u32,
        row_height: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let rect = Rect {
            x: 0,
            y: 0,
            width: col_width as u16,
            height: row_height as u16,
        };

        // Update client data
        let mut clients = self.clients.lock().await;
        let client_data = clients.get_mut(&self.id).unwrap();
        
        // Update terminal size
        {
            let mut size = client_data.term_size.lock().unwrap();
            size.width = col_width as u16;
            size.height = row_height as u16;
            
            // Calculate target dimensions
            size.target_width = col_width as usize;
            size.target_height = ((size.target_width * 9 / 16 + 1) / 2) * 2;
        }
        
        // Resize the terminal
        client_data.terminal.resize(rect)?;
        
        // If this is the first client, we need to start the Minecraft rendering
        if clients.len() == 1 {
            let client_id = self.id;
            let clients_ref = self.clients.clone();
            
            // Start the Minecraft rendering in a separate task
            tokio::spawn(async move {
                if let Err(e) = start_minecraft_rendering(client_id, clients_ref).await {
                    eprintln!("Error starting Minecraft rendering: {}", e);
                }
            });
        }

        session.channel_success(channel)?;
        
        Ok(())
    }
}

// Function to start the Minecraft rendering for an SSH client
async fn start_minecraft_rendering(
    client_id: usize,
    clients: Arc<TokioMutex<HashMap<usize, ClientData>>>,
) -> Result<(), anyhow::Error> {
    // We'll use a separate thread for the rendering since it's blocking
    let running = Arc::new(AtomicBool::new(true));
    
    // Get the terminal size for this client
    let mut clients_lock = clients.lock().await;
    let client_data = match clients_lock.get_mut(&client_id) {
        Some(data) => data,
        None => return Ok(()),
    };
    
    let term_size_clone = Arc::clone(&client_data.term_size);
    
    // Create channels for communication
    let (render_tx, render_rx) = mpsc::channel();
    let (_input_tx, _input_rx) = mpsc::channel::<InputEvent>();
    let (_resize_tx, resize_rx) = mpsc::channel::<()>();
    
    // Clone for different threads
    let running_render = Arc::clone(&running);
    let running_display = Arc::clone(&running);
    let term_size_render = Arc::clone(&term_size_clone);
    
    // Start the rendering thread
    let _render_handle = std::thread::spawn(move || {
        if let Err(e) = render::render_minecraft_directly(render_tx, resize_rx, term_size_render, running_render) {
            eprintln!("Render error: {}", e);
        }
    });
    
    // Create a rendering task that updates the terminal with frames
    let clients_display = clients.clone();
    let render_thread = tokio::spawn(async move {
        let mut last_frame: Option<String> = None;
        
        while running_display.load(Ordering::SeqCst) {
            // Try to get the latest frame
            match render_rx.try_recv() {
                Ok(frame) => {
                    // Got a frame, now drain any newer ones that might be waiting
                    let mut latest = frame;
                    while let Ok(newer) = render_rx.try_recv() {
                        latest = newer; // Keep only the newest frame
                    }
                    last_frame = Some(latest);
                },
                Err(mpsc::TryRecvError::Empty) => {
                    // No new frames, use the last one or wait
                    if last_frame.is_none() {
                        match render_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(frame) => last_frame = Some(frame),
                            Err(_) => {
                                if !running_display.load(Ordering::SeqCst) {
                                    break;
                                }
                                continue;
                            }
                        }
                    }
                },
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Channel closed, exit
                    break;
                }
            }
            
            // If we have a frame to display, do it
            if let Some(frame) = &last_frame {
                // Get a lock on the clients map
                let mut clients = clients_display.lock().await;
                
                // Find this client's terminal
                if let Some(client_data) = clients.get_mut(&client_id) {
                    // Convert the frame to bytes once before the terminal draw call
                    let frame_bytes = frame.clone().into_bytes();
                    
                    // Draw the frame to the terminal using a write-only approach
                    let backend = client_data.terminal.backend_mut();
                    let _ = backend.write(&frame_bytes);
                    let _ = backend.flush();
                } else {
                    // Client not found, exit
                    break;
                }
            }
            
            // Small sleep to avoid consuming too much CPU
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    });
    
    // Release the lock so other tasks can use it
    drop(clients_lock);
    
    // Wait for the client to disconnect
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // Check if client still exists
        let client_exists = {
            let clients = clients.lock().await;
            clients.contains_key(&client_id)
        };
        
        if !client_exists {
            // Client disconnected, stop rendering
            running.store(false, Ordering::SeqCst);
            break;
        }
    }
    
    // Wait for rendering thread to finish
    let _ = render_thread.await;
    
    Ok(())
}

// Clean up when the server is dropped
impl Drop for MinecraftSshServer {
    fn drop(&mut self) {
        let id = self.id;
        let clients = self.clients.clone();
        tokio::spawn(async move {
            let mut clients = clients.lock().await;
            clients.remove(&id);
        });
    }
}

// Function to check if the program is running in interactive mode
pub fn is_interactive() -> bool {
    atty::is(atty::Stream::Stdout)
}

// Main function to run the SSH server
pub async fn run_ssh_server() -> Result<(), anyhow::Error> {
    let mut server = MinecraftSshServer::new();
    server.run().await
}
