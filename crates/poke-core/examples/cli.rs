//! Headless test client.
//!
//!   cargo run -p poke-core --example cli -- gen-code
//!   cargo run -p poke-core --example cli -- --relay localhost:8787 --code <CODE> --name Bob --auto-reply coming
//!   cargo run -p poke-core --example cli -- --relay localhost:8787 --code <CODE> --name Alice --poke Bob --msg "Lunch?"
//!
//! Interactive commands on stdin: `poke <name|id> [message]`, `reply <kind>`,
//! `dnd on|off`, `quit`. Events are printed as JSON lines.

use std::collections::HashMap;
use std::time::Duration;

use poke_core::{Client, ClientConfig, Event, Presence, Profile, ReplyKind};
use tokio::io::{AsyncBufReadExt, BufReader};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn parse_kind(s: &str) -> Option<ReplyKind> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("gen-code") {
        println!("{}", poke_core::generate_team_code());
        return;
    }
    let relay = arg(&args, "--relay").unwrap_or_else(|| "localhost:8787".into());
    let code = arg(&args, "--code").expect("--code required");
    let name = arg(&args, "--name").expect("--name required");
    let id = arg(&args, "--id").unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let poke_target = arg(&args, "--poke");
    let poke_msg = arg(&args, "--msg");
    let auto_reply = arg(&args, "--auto-reply").map(|k| parse_kind(&k).expect("unknown reply kind"));
    let timeout: u64 = arg(&args, "--timeout").and_then(|t| t.parse().ok()).unwrap_or(0);

    let cfg = ClientConfig {
        relay_url: relay,
        team_code: code,
        client_id: id,
        profile: Profile { name, presence: Presence::Available },
        proxy: arg(&args, "--proxy"),
    };
    let (client, mut events) = Client::start(cfg).expect("start client");
    eprintln!("team id: {}", client.team_id());

    let mut peers: HashMap<String, String> = HashMap::new(); // name(lower) -> id
    let mut pending_poke = poke_target.clone();
    let mut last_poke: Option<(String, String)> = None;
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let deadline = async {
        if timeout == 0 {
            std::future::pending::<()>().await
        } else {
            tokio::time::sleep(Duration::from_secs(timeout)).await
        }
    };
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => { eprintln!("timeout"); std::process::exit(2); }
            ev = events.recv() => {
                let Some(ev) = ev else { break };
                println!("{}", serde_json::to_string(&ev).unwrap());
                match &ev {
                    Event::Peers { peers: list } => {
                        peers = list.iter().map(|p| (p.name.to_lowercase(), p.id.clone())).collect();
                        if let Some(target) = pending_poke.clone() {
                            if let Some(to) = peers.get(&target.to_lowercase()) {
                                client.poke(to, poke_msg.as_deref());
                                pending_poke = None;
                            }
                        }
                    }
                    Event::Poke { from, pid, .. } => {
                        last_poke = Some((from.clone(), pid.clone()));
                        if let Some(kind) = auto_reply {
                            client.reply(from, pid, ReplyKind::Seen);
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            client.reply(from, pid, kind);
                        }
                    }
                    Event::Reply { kind, .. } if poke_target.is_some() && *kind != ReplyKind::Seen => {
                        // One-shot poke mode: done once we got a real answer.
                        client.shutdown();
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        return;
                    }
                    _ => {}
                }
            }
            line = stdin.next_line() => {
                let line: String = match line { Ok(Some(l)) => l, _ => continue };
                let mut parts = line.trim().splitn(3, ' ');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("poke"), Some(who), msg) => {
                        let to = peers.get(&who.to_lowercase()).cloned().unwrap_or_else(|| who.to_string());
                        let pid = client.poke(&to, msg);
                        eprintln!("sent poke {pid}");
                    }
                    (Some("reply"), Some(kind), _) => match (last_poke.clone(), parse_kind(kind)) {
                        (Some((from, pid)), Some(k)) => client.reply(&from, &pid, k),
                        _ => eprintln!("usage: reply coming|five_min|busy (answers the last poke)"),
                    },
                    (Some("dnd"), Some(v), _) => client.set_profile(Profile {
                        name: arg(&args, "--name").unwrap(),
                        presence: if v == "on" { Presence::Dnd } else { Presence::Available },
                    }),
                    (Some("quit"), _, _) => { client.shutdown(); tokio::time::sleep(Duration::from_millis(200)).await; return; }
                    _ => eprintln!("commands: poke <name|id> [msg] | reply <kind> | dnd on|off | quit"),
                }
            }
        }
    }
}
