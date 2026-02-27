mod client;
mod protocol;
mod server;
mod storage;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "collab-cli", version, about = "Barebones collaborative text backend (TCP)")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the collaboration server
    Server {
        /// Address to bind (e.g. 0.0.0.0:4000)
        #[arg(long, default_value = "0.0.0.0:4000")]
        addr: String,
        /// Directory to store document snapshots
        #[arg(long, default_value = "data")]
        data_dir: String,
        /// Address for HTTP health checks (GET /health)
        #[arg(long, default_value = "0.0.0.0:8080")]
        health_addr: String,
    },
    /// Run an interactive client
    Client {
        /// Server address (e.g. 127.0.0.1:4000)
        #[arg(long, default_value = "127.0.0.1:4000")]
        addr: String,
        /// User display name
        #[arg(long)]
        user: String,
        /// Room name
        #[arg(long, default_value = "default-room")]
        room: String,
        /// Document name
        #[arg(long, default_value = "shared.txt")]
        doc: String,
    },
    /// Run a minimal TUI frontend
    Tui {
        /// Server address (e.g. 127.0.0.1:4000 or ngrok host:port)
        #[arg(long, default_value = "127.0.0.1:4000")]
        addr: String,
        /// User display name
        #[arg(long)]
        user: String,
        /// Room name
        #[arg(long, default_value = "default-room")]
        room: String,
        /// Document name
        #[arg(long, default_value = "shared.txt")]
        doc: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    match args.command {
        Command::Server {
            addr,
            data_dir,
            health_addr,
        } => server::run(&addr, &data_dir, &health_addr).await?,
        Command::Client {
            addr,
            user,
            room,
            doc,
        } => client::run(&addr, &user, &room, &doc).await?,
        Command::Tui {
            addr,
            user,
            room,
            doc,
        } => tui::run(&addr, &user, &room, &doc).await?,
    }

    Ok(())
}
