// Launcher-owned UDP bridge. Credentials enter through stdin, never argv.
use super::wire::{random, Cipher, ACK, DATA, HEADER, MAX_FRAME, MAX_GAME_PACKET, PROBE, REGISTER};
use super::{ClientMessage, Id, ServerMessage, PATH};
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    net::SocketAddr,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

#[derive(Serialize, Deserialize)]
struct Launch {
    url: String,
    homedir: String,
}

static LAUNCHER_PROCESS: AtomicBool = AtomicBool::new(false);
pub fn enable_in_launcher() {
    LAUNCHER_PROCESS.store(true, Ordering::Relaxed);
}

pub struct Process {
    child: Option<Child>,
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Process {
    pub fn detach(mut self) {
        self.child.take();
    }
}

/// An authenticated game launch starts the bridge before the retail client
/// consumes its single-use launch ticket. Other native tools are unaffected.
pub fn start(url: &str, homedir: &str) -> Result<Option<Process>> {
    let parsed = url::Url::parse(url)?;
    if !LAUNCHER_PROCESS.load(Ordering::Relaxed)
        || !parsed.path().starts_with(env!("KNOCKOUT_LAUNCH_PREFIX"))
    {
        return Ok(None);
    }
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("__p2p-bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().context("start player network transport")?;
    let mut input = child.stdin.take().context("open player transport input")?;
    serde_json::to_writer(
        &mut input,
        &Launch {
            url: url.to_owned(),
            homedir: homedir.to_owned(),
        },
    )?;
    input.write_all(b"\n")?;
    drop(input);
    let output = child
        .stdout
        .take()
        .context("open player transport readiness")?;
    let process = Process { child: Some(child) };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(output)
            .take(1024)
            .read_line(&mut line);
        let _ = tx.send(result.is_ok() && line.trim() == "ready");
    });
    if !rx.recv_timeout(Duration::from_secs(25)).unwrap_or(false) {
        bail!("Player network transport could not connect. Check the server connection and launcher version.");
    }
    Ok(Some(process))
}

pub fn run_from_stdin() -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin().take(16 * 1024).read_to_end(&mut bytes)?;
    let launch: Launch =
        serde_json::from_slice(&bytes).context("decode player transport launch")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run(launch, Lifecycle::Game))
}

enum Lifecycle {
    Game,
}

enum Event {
    Connected(mpsc::Sender<Message>),
    Disconnected,
    Server(Message),
    Network(Vec<u8>, SocketAddr),
    Local(Option<Id>, Vec<u8>, SocketAddr),
    Exit,
}

fn game_process_matches(pid: u32, homedir: &str) -> bool {
    // Discovery selects the newest matching process once. A second launch
    // with the same profile must not make the original helper abandon a
    // still-running game (including a player acting as the listen host).
    crate::process::is_game_pid(pid)
        && crate::process::has_launch_argument(pid, &format!("-homedir={homedir}"))
}

struct Peer {
    cipher: Cipher,
    host: bool,
    allocation: String,
    socket: Arc<UdpSocket>,
    reader: Option<JoinHandle<()>>,
    game_address: Option<SocketAddr>,
    candidate: Option<SocketAddr>,
    direct: Option<(SocketAddr, Instant)>,
    probe: [u8; 16],
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

fn read_udp(
    socket: Arc<UdpSocket>,
    events: mpsc::Sender<Event>,
    local: bool,
    id: Option<Id>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0; MAX_FRAME];
        loop {
            let (size, address) = match socket.recv_from(&mut buffer).await {
                Ok(value) => value,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                Err(_) => break,
            };
            let data = buffer[..size].to_vec();
            let event = if local {
                Event::Local(id, data, address)
            } else {
                Event::Network(data, address)
            };
            if events.send(event).await.is_err() {
                break;
            }
        }
    })
}

fn send_control(tx: &Option<mpsc::Sender<Message>>, message: ClientMessage) {
    if let Some(tx) = tx {
        if let Ok(text) = serde_json::to_string(&message) {
            let _ = tx.try_send(Message::Text(text));
        }
    }
}

fn connect(
    url: String,
    credential: String,
    ports: (u16, u16),
    events: mpsc::Sender<Event>,
    delay: bool,
) {
    tokio::spawn(async move {
        if delay {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let result: Result<()> = async {
            let mut request = url.into_client_request()?;
            request.headers_mut().insert("authorization", format!("Bearer {credential}").parse()?);
            let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                max_message_size: Some(MAX_FRAME), max_frame_size: Some(MAX_FRAME), ..Default::default()
            };
            let (socket, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async_with_config(request, Some(config), true)).await??;
            let (mut writer, mut reader) = socket.split();
            writer.send(Message::Text(serde_json::to_string(&ClientMessage::Register { proxy_port: ports.0, host_port: ports.1 })?)).await?;
            let (tx, mut rx) = mpsc::channel(256);
            events.send(Event::Connected(tx)).await?;
            loop {
                tokio::select! {
                    outbound = rx.recv() => match outbound {
                        Some(message) => { tokio::time::timeout(Duration::from_secs(5), writer.send(message)).await??; }
                        None => break,
                    },
                    inbound = tokio::time::timeout(Duration::from_secs(45), reader.next()) => match inbound? {
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(message)) => { events.send(Event::Server(message)).await?; }
                        Some(Err(_)) => break,
                    }
                }
            }
            Ok(())
        }.await;
        // Errors can include a request URL or authorization header. Do not log
        // the transport library's error text into launcher/player logs.
        let _ = result;
        let _ = events.send(Event::Disconnected).await;
    });
}

async fn run(launch: Launch, lifecycle: Lifecycle) -> Result<()> {
    let mut lifecycle = Some(lifecycle);
    let mut url = url::Url::parse(&launch.url)?;
    let ticket = url
        .path()
        .strip_prefix(env!("KNOCKOUT_LAUNCH_PREFIX"))
        .and_then(|s| s.split('/').next())
        .filter(|s| !s.is_empty())
        .context("transport launch has no ticket")?
        .to_owned();
    let scheme = match url.scheme() {
        "https" => "wss",
        "http"
            if url.host_str().is_some_and(|h| {
                h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }) =>
        {
            "ws"
        }
        _ => bail!("public player transport requires HTTPS"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid transport scheme"))?;
    url.set_path(PATH);
    url.set_query(None);
    url.set_fragment(None);
    let proxy = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let mut host_reservation = Some(UdpSocket::bind("127.0.0.1:0").await?);
    let host_port = host_reservation.as_ref().unwrap().local_addr()?.port();
    let ports = (proxy.local_addr()?.port(), host_port);
    let network = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let (events, mut rx) = mpsc::channel(512);
    let _network_reader = AbortTask(read_udp(network.clone(), events.clone(), false, None));
    let mut outgoing = None;
    let mut credential = ticket;
    let mut discovery: Option<Cipher> = None;
    let mut rendezvous = None;
    let mut peers: BTreeMap<Id, Peer> = BTreeMap::new();
    // Retain closed bindings briefly so the OS cannot immediately reuse an
    // old match's port while the previous NetDriver is still tearing down.
    let mut retired_sockets: Vec<(Instant, Arc<UdpSocket>)> = Vec::new();
    let mut initialized = false;
    let mut last_server_received = Instant::now();
    let mut last_udp_ack: Option<Instant> = None;
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    connect(
        url.to_string(),
        credential.clone(),
        ports,
        events.clone(),
        false,
    );
    loop {
        tokio::select! {
            _ = tick.tick() => {
                retired_sockets.retain(|(retired, _)| retired.elapsed() < Duration::from_secs(30));
                if !initialized && started.elapsed() > Duration::from_secs(20) { bail!("player transport initialization timed out"); }
                if outgoing.is_some() && last_server_received.elapsed() > Duration::from_secs(45) { outgoing = None; }
                send_control(&outgoing, ClientMessage::Ping);
                if let (Some(cipher), Some(address)) = (discovery.as_mut(), rendezvous) {
                    if let Some(frame) = cipher.seal(REGISTER, &[]) { let _ = network.send_to(&frame, address).await; }
                }
                for peer in peers.values_mut() {
                    if let Some(address) = peer.candidate {
                        peer.probe = random();
                        if let Some(frame) = peer.cipher.seal(PROBE, &peer.probe) { let _ = network.send_to(&frame, address).await; }
                    }
                }
            }
            event = rx.recv() => match event.context("transport event loop closed")? {
                Event::Exit => return Ok(()),
                Event::Connected(tx) => { outgoing = Some(tx); last_server_received = Instant::now(); }
                Event::Disconnected => { outgoing = None; connect(url.to_string(), credential.clone(), ports, events.clone(), true); }
                Event::Server(Message::Text(text)) => { last_server_received = Instant::now(); match serde_json::from_str::<ServerMessage>(&text)? {
                    ServerMessage::Welcome { id, key, resume, udp } => {
                        credential = resume;
                        if discovery.is_none() { discovery = Some(Cipher::new(id, key, 0)); }
                        rendezvous = match udp { Some(address) => tokio::net::lookup_host(address).await.ok().and_then(|mut a| a.find(|a| a.is_ipv4())), None => None };
                        if !initialized {
                            initialized = true;
                            match lifecycle.take().context("transport lifecycle initialized twice")? {
                                Lifecycle::Game => {
                                    println!("ready"); std::io::stdout().flush()?;
                                    let events = events.clone(); let homedir = launch.homedir.clone();
                                    tokio::task::spawn_blocking(move || {
                                        if let Ok(pid) = crate::process::wait_for_game_pid_with_homedir(&homedir, Duration::from_secs(180)) {
                                            while game_process_matches(pid, &homedir) { std::thread::sleep(Duration::from_secs(2)); }
                                        }
                                        let _ = events.blocking_send(Event::Exit);
                                    });
                                }
                            }
                        }
                    }
                    ServerMessage::Pair { id, key, host, host_port, allocation, candidate } => {
                        let obsolete: Vec<_> = peers.iter().filter_map(|(id, peer)| (peer.allocation != allocation).then_some(*id)).collect();
                        for id in obsolete { if let Some(peer) = peers.remove(&id) { retired_sockets.push((Instant::now(), peer.socket.clone())); } }
                        if !peers.contains_key(&id) {
                            if !host && peers.values().any(|p| !p.host) { bail!("overlapping player transport allocations"); }
                            let socket = if host {
                                let mut socket = UdpSocket::bind("127.0.0.1:0").await?;
                                if socket.local_addr()?.port() == host_port {
                                    // Keep the conflicting binding alive while
                                    // choosing its replacement, so it cannot
                                    // be selected a second time.
                                    socket = UdpSocket::bind("127.0.0.1:0").await?;
                                }
                                socket.connect((std::net::Ipv4Addr::LOCALHOST, host_port)).await?;
                                host_reservation.take();
                                Arc::new(socket)
                            } else { Arc::new(UdpSocket::bind("127.0.0.1:0").await?) };
                            let reader = Some(read_udp(socket.clone(), events.clone(), true, Some(id)));
                            peers.insert(id, Peer { cipher: Cipher::new(id, key, u8::from(!host)), host, allocation, socket, reader, game_address: None, candidate, direct: None, probe: random() });
                        }
                        send_control(&outgoing, ClientMessage::Ready { id, proxy_port: peers[&id].socket.local_addr()?.port() });
                    }
                    ServerMessage::Candidate { id, address } => { if let Some(peer) = peers.get_mut(&id) { peer.candidate = Some(address); } }
                    ServerMessage::Close { id } => { if let Some(peer) = peers.remove(&id) { retired_sockets.push((Instant::now(), peer.socket.clone())); } }
                    ServerMessage::Pong => {}
                } },
                Event::Server(Message::Binary(frame)) => { receive(&mut peers, &network, &frame, None).await; }
                Event::Server(_) => {},
                Event::Network(frame, address) => {
                    if Some(address) == rendezvous {
                        if let Some((ACK, challenge)) = discovery.as_mut().and_then(|cipher| cipher.open(&frame, 1)) {
                            if let Ok(challenge) = challenge.try_into() {
                                last_udp_ack = Some(Instant::now());
                                send_control(&outgoing, ClientMessage::UdpReady { challenge });
                            }
                            continue;
                        }
                        // Receiving a relayed probe never nominates the relay
                        // address as a direct peer candidate.
                        receive(&mut peers, &network, &frame, None).await;
                        continue;
                    }
                    receive(&mut peers, &network, &frame, Some(address)).await;
                }
                Event::Local(id, data, address) => {
                    if data.len() > MAX_GAME_PACKET || !address.ip().is_loopback() { continue; }
                    let peer = match id { Some(id) => peers.get_mut(&id), None => peers.values_mut().find(|p| !p.host) };
                    let Some(peer) = peer else { continue; };
                    if !peer.host {
                        if peer.game_address.is_some_and(|previous| previous != address) { continue; }
                        peer.game_address = Some(address);
                    }
                    let Some(frame) = peer.cipher.seal(DATA, &data) else { continue; };
                    if let Some((target, seen)) = peer.direct.filter(|(_, seen)| seen.elapsed() < Duration::from_secs(3)) {
                        let _ = seen;
                        if frame.len() <= 1232 && network.send_to(&frame, target).await.is_ok() { continue; }
                    }
                    if let Some(target) = rendezvous.filter(|_| last_udp_ack.is_some_and(|seen| seen.elapsed() < Duration::from_secs(3))) {
                        if frame.len() <= 1232 && network.send_to(&frame, target).await.is_ok() { continue; }
                    }
                    if let Some(tx) = &outgoing { let _ = tx.try_send(Message::Binary(frame)); }
                }
            }
        }
    }
}

struct AbortTask(JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn receive(
    peers: &mut BTreeMap<Id, Peer>,
    network: &UdpSocket,
    frame: &[u8],
    source: Option<SocketAddr>,
) {
    if frame.len() < HEADER + 17 {
        return;
    }
    let id: Id = frame[..16].try_into().unwrap();
    let Some(peer) = peers.get_mut(&id) else {
        return;
    };
    let Some((kind, data)) = peer.cipher.open(frame, u8::from(peer.host)) else {
        return;
    };
    match (kind, source) {
        (DATA, _) => {
            if peer.host {
                let _ = peer.socket.send(&data).await;
            } else if let Some(address) = peer.game_address {
                let _ = peer.socket.send_to(&data, address).await;
            }
        }
        (PROBE, Some(address)) if data.len() == 16 => {
            if let Some(ack) = peer.cipher.seal(ACK, &data) {
                let _ = network.send_to(&ack, address).await;
            }
            // An authenticated peer-reflexive candidate may differ from the
            // rendezvous mapping. Probe it before selecting it for gameplay.
            if peer.candidate != Some(address) {
                peer.candidate = Some(address);
            }
        }
        (ACK, Some(address)) if data.as_slice() == peer.probe => {
            peer.direct = Some((address, Instant::now()))
        }
        _ => {}
    }
}
