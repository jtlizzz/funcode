use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "funcode")]
#[command(about = "A powerful AI agent framework", long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long)]
    name: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(name) = args.name {
        println!("Hello, {}!", name);
    }

    // TODO: Implement CLI logic
    Ok(())
}
