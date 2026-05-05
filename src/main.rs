pub mod avahi;
pub mod uefi;
pub mod upload;

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use zbus::Connection;

use crate::avahi::Avahi;
use crate::upload::upload_folder;

#[derive(Parser)]
#[command(name = std::env!("CARGO_PKG_NAME"))]
#[command(about = std::env!("CARGO_PKG_DESCRIPTION"))]
struct Cli {
    /// Direct URL to dispatch server (bypasses service discovery)
    #[arg(short, long, global = true)]
    url: Option<String>,

    /// Show all available dispatch services and exit
    #[arg(long = "show-dispatch", default_value = "false")]
    show_dispatch: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Alerts dispatch that a workload is booting.
    Boot,

    /// Asks dispatch to create a GitHub issue with test results.
    Report {
        /// The title of the GitHub issue
        #[arg(short, long)]
        title: String,

        /// Text file to use as the GitHub issue body
        ///
        /// If not specified, the body will be read from stdin.
        #[arg(short, long, value_name = "FILE")]
        body: Option<std::path::PathBuf>,

        /// Labels for the GitHub issue (can be specified multiple times)
        #[arg(short, long, action = clap::ArgAction::Append)]
        label: Vec<String>,

        /// Assignees for the GitHub issue (can be specified multiple times)
        #[arg(short, long, action = clap::ArgAction::Append)]
        assignee: Vec<String>,

        /// Milestone for the GitHub issue
        #[arg(short, long)]
        milestone: Option<String>,
    },

    /// Uploads a folder as a zip archive to dispatch.
    Upload {
        /// Path to the folder to upload
        #[arg(value_name = "FOLDER")]
        folder: PathBuf,

        /// Name for the uploaded archive (defaults to folder name + .zip)
        #[arg(short, long)]
        name: Option<String>,
    },
}

#[derive(Debug, Clone)]
enum Action {
    Boot,
    Report(Report),
    Upload {
        folder: PathBuf,
        name: Option<String>,
    },
}

impl TryFrom<Commands> for Action {
    type Error = Box<dyn std::error::Error>;

    fn try_from(value: Commands) -> Result<Self, Self::Error> {
        match value {
            Commands::Boot => Ok(Action::Boot),
            Commands::Report {
                title,
                body,
                label,
                assignee,
                milestone,
            } => Ok(Action::Report(Report {
                title,
                body: body
                    .map(std::fs::read_to_string)
                    .unwrap_or_else(|| std::io::read_to_string(std::io::stdin().lock()))?,
                labels: label,
                assignees: assignee,
                milestone,
            })),
            Commands::Upload { folder, name } => Ok(Action::Upload { folder, name }),
        }
    }
}

impl Action {
    async fn perform(&self, url: &str) -> Result<bool, Box<dyn std::error::Error>> {
        // Handle upload separately as it has its own response handling
        if let Action::Upload { folder, name } = self {
            return upload_folder(url, folder, name.as_deref()).await;
        }

        // Send the appropriate request based on action
        let response = match self {
            Action::Boot => Client::new().post(url).send().await?,
            Action::Report(report) => Client::new().put(url).json(report).send().await?,
            Action::Upload { .. } => unreachable!(),
        };

        // Handle the response
        match response.status() {
            // This is the normal error when the service worked,
            // but no task was found for our IP address. This either
            // means that there is no job or that we need to contact
            // the server on a different address. Skip.
            StatusCode::EXPECTATION_FAILED => Ok(false),
            StatusCode::OK => Ok(true),
            status => {
                eprintln!("warning: {status}");
                Ok(false)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    title: String,

    #[serde(skip_serializing_if = "String::is_empty")]
    body: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    assignees: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    milestone: Option<String>,
}

const RESOLVER_TIMEOUT: Duration = Duration::from_secs(5);
const BROWSER_TIMEOUT: Duration = Duration::from_secs(10);

// Avahi D-Bus proxy interfaces
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Handle --show-dispatch mode
    if cli.show_dispatch {
        return show_dispatch_services().await;
    }

    let command = cli
        .command
        .ok_or("No command specified. Use --help for usage information.")?;
    let action: Action = command.try_into()?;

    // If URL is provided directly, use it and skip service discovery
    if let Some(url) = cli.url {
        match action.perform(&url).await {
            Ok(true) => return Ok(()),
            Ok(false) => return Err("Operation failed".into()),
            Err(e) => return Err(e),
        }
    }

    for url in uefi::find_urls().await? {
        match action.perform(&url).await {
            Ok(true) => return Ok(()),
            Ok(false) => continue,
            Err(e) => eprintln!("error: {}: {}", url, e),
        }
    }

    let connection = Connection::system().await?;
    let avahi = Avahi::new(&connection).await?;

    let mut browsing = avahi.browse(-1, -1, "_dispatch._tcp", "local", 0).await?;
    while let Ok(Some(item)) = timeout(BROWSER_TIMEOUT, browsing.next()).await {
        let resolved = timeout(RESOLVER_TIMEOUT, avahi.resolve(item)).await?;

        match resolved {
            Ok(resolved) => {
                match resolved.address.ip() {
                    addr if addr.is_loopback() => continue,
                    IpAddr::V4(ipv4) if ipv4.is_link_local() => continue,
                    IpAddr::V6(ipv6) if ipv6.is_unicast_link_local() => continue,
                    _ => {}
                }

                // Construct the URL
                let url = match resolved.txt.get("path") {
                    Some(path) => format!("http://{}{}", resolved.address, path),
                    None => continue,
                };
                match action.perform(&url).await {
                    Ok(true) => std::process::exit(0),
                    Ok(false) => continue,

                    Err(e) => eprintln!("error: {}: {}", url, e),
                }
            }
            Err(e) => eprintln!("Warning: Resolve failed: {e}"),
        }
    }

    Err("no dispatch services found".into())
}

async fn show_dispatch_services() -> Result<(), Box<dyn std::error::Error>> {
    let mut found_any = false;

    // First, show UEFI-discovered URLs
    let uefi_urls = uefi::find_urls().await?;
    if !uefi_urls.is_empty() {
        println!("UEFI Boot Services:");
        for url in &uefi_urls {
            println!("  {}", url);
        }
        found_any = true;
    }

    // Then, show mDNS/Avahi discovered services
    let connection = Connection::system().await?;
    let avahi = Avahi::new(&connection).await?;

    let mut browsing = avahi.browse(-1, -1, "_dispatch._tcp", "local", 0).await?;
    let mut mdns_services = Vec::new();

    while let Ok(Some(item)) = timeout(BROWSER_TIMEOUT, browsing.next()).await {
        let resolved = timeout(RESOLVER_TIMEOUT, avahi.resolve(item)).await;

        match resolved {
            Ok(Ok(resolved)) => {
                // Skip loopback and link-local addresses
                match resolved.address.ip() {
                    addr if addr.is_loopback() => continue,
                    IpAddr::V4(ipv4) if ipv4.is_link_local() => continue,
                    IpAddr::V6(ipv6) if ipv6.is_unicast_link_local() => continue,
                    _ => {}
                }

                let url = match resolved.txt.get("path") {
                    Some(path) => format!("http://{}{}", resolved.address, path),
                    None => format!("http://{}", resolved.address),
                };

                mdns_services.push((resolved.service.name, resolved.host, url));
            }
            Ok(Err(e)) => eprintln!("Warning: Resolve failed: {e}"),
            Err(_) => {} // Timeout, skip
        }
    }

    if !mdns_services.is_empty() {
        if found_any {
            println!();
        }
        println!("mDNS/Avahi Dispatch Services:");
        for (name, host, url) in &mdns_services {
            println!("  {} ({})", name, host);
            println!("    URL: {}", url);
        }
        found_any = true;
    }

    if !found_any {
        println!("No dispatch services found.");
    }

    Ok(())
}
