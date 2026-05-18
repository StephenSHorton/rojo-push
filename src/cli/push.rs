use std::{
    io::{self, Write},
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use anyhow::Context;
use clap::Parser;
use termcolor::{BufferWriter, Color, ColorSpec, WriteColor};

use crate::web_api::RefreshResponse;

use super::GlobalOptions;

const DEFAULT_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const DEFAULT_PORT: u16 = 34872;

/// Trigger a manual sync on a running `rojo serve` instance.
///
/// Re-reads the project from disk, diffs it against the in-memory tree, and
/// pushes any changes to connected Roblox Studio plugins. Intended to be paired
/// with `rojo serve --no-watch`.
#[derive(Debug, Parser)]
pub struct PushCommand {
    /// Address of the running `rojo serve` instance. Defaults to `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// Port of the running `rojo serve` instance. Defaults to `34872`.
    #[clap(long)]
    pub port: Option<u16>,

    /// How long to wait for the server to respond, in seconds. Defaults to 30.
    #[clap(long, default_value_t = 30)]
    pub timeout: u64,
}

impl PushCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let address = self.address.unwrap_or(DEFAULT_ADDRESS.into());
        let port = self.port.unwrap_or(DEFAULT_PORT);
        let url = format!("http://{}:{}/api/refresh", address, port);

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(self.timeout))
            .build()
            .context("failed to construct HTTP client")?;

        let response = client
            .post(&url)
            .send()
            .with_context(|| format!("failed to POST {} (is `rojo serve` running?)", url))?;

        let status = response.status();
        let body_text = response.text().context("failed to read response body")?;

        if !status.is_success() && status.as_u16() != 207 {
            anyhow::bail!(
                "Server returned HTTP {}:\n{}",
                status,
                body_text
            );
        }

        let summary: RefreshResponse = serde_json::from_str(&body_text)
            .with_context(|| format!("server returned unexpected body:\n{}", body_text))?;

        show_summary(&summary, global.color.into())?;

        if !summary.errors.is_empty() {
            // Non-zero exit so scripts can detect partial-success runs.
            std::process::exit(1);
        }

        Ok(())
    }
}

fn show_summary(summary: &RefreshResponse, color: termcolor::ColorChoice) -> io::Result<()> {
    let writer = BufferWriter::stdout(color);
    let mut buffer = writer.buffer();

    let mut bold = ColorSpec::new();
    bold.set_bold(true);
    buffer.set_color(&bold)?;
    write!(&mut buffer, "Pushed")?;
    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, ": ")?;

    let mut green = ColorSpec::new();
    green.set_fg(Some(Color::Green));
    buffer.set_color(&green)?;
    write!(&mut buffer, "+{}", summary.instances_added)?;
    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, " ")?;

    let mut yellow = ColorSpec::new();
    yellow.set_fg(Some(Color::Yellow));
    buffer.set_color(&yellow)?;
    write!(&mut buffer, "~{}", summary.instances_updated)?;
    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, " ")?;

    let mut red = ColorSpec::new();
    red.set_fg(Some(Color::Red));
    buffer.set_color(&red)?;
    write!(&mut buffer, "-{}", summary.instances_removed)?;
    buffer.set_color(&ColorSpec::new())?;

    writeln!(&mut buffer, " ({} ms)", summary.duration_ms)?;

    if !summary.errors.is_empty() {
        let mut red_bold = ColorSpec::new();
        red_bold.set_fg(Some(Color::Red)).set_bold(true);
        buffer.set_color(&red_bold)?;
        writeln!(&mut buffer, "Errors ({}):", summary.errors.len())?;
        buffer.set_color(&ColorSpec::new())?;
        for err in &summary.errors {
            writeln!(&mut buffer, "  - {}", err)?;
        }
    }

    writer.print(&buffer)?;
    Ok(())
}
