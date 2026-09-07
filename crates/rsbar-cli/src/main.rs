//! `rsbar` — the command line client.

use async_mach_ports::{SendPort, Sender};
use clap::{Args, Parser, Subcommand};
use rsbar_protocol::style::Color;
use rsbar_protocol::{
    BarPatch, Edge, ItemName, ItemPatch, Json, Kind, Position, Query, Request, Response,
    service_name,
};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "rsbar", version, about = "Control the rsbar status bar.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Doc comments here are clap's `--help` text, not rustdoc, so backticks would
// show up verbatim in the terminal.
#[allow(clippy::doc_markdown)]
#[derive(Subcommand)]
enum Command {
    /// Change the bar itself.
    Bar {
        #[command(subcommand)]
        action: BarAction,
    },
    /// Add, change or remove an item.
    Item {
        #[command(subcommand)]
        action: ItemAction,
    },
    /// Read the daemon's current state.
    Query {
        #[command(subcommand)]
        what: QueryWhat,
    },
    /// Fire an event now, as a source would.
    Trigger {
        #[arg(value_parser = parse_event)]
        event: Kind,
        /// Passed to scripts as RSBAR_INFO. JSON if it parses as JSON,
        /// otherwise a plain string.
        #[arg(long)]
        info: Option<String>,
    },
    /// Run every item's script immediately, ignoring update frequency.
    Update,
    /// Tear the bar down and re-run the config.
    Reload,
    /// Ask the daemon to exit.
    Shutdown,
}

#[derive(Subcommand)]
enum BarAction {
    Set(BarOptions),
}

#[derive(Args)]
struct BarOptions {
    #[arg(long)]
    height: Option<f64>,
    #[arg(long, value_parser = parse_edge)]
    edge: Option<Edge>,
    #[arg(long, value_parser = parse_color)]
    color: Option<u32>,
    #[arg(long)]
    margin: Option<f64>,
    #[arg(long, allow_negative_numbers = true)]
    y_offset: Option<f64>,
    #[arg(long)]
    corner_radius: Option<f64>,
    #[arg(long)]
    blur_radius: Option<i32>,
    #[arg(long)]
    hidden: Option<bool>,
    /// Draw over the system menu bar instead of underneath it.
    #[arg(long)]
    topmost: Option<bool>,
}

#[derive(Subcommand)]
enum ItemAction {
    /// Create an item, or move an existing one.
    Add {
        name: ItemName,
        #[arg(long, default_value = "left", value_parser = parse_position)]
        position: Position,
    },
    Set {
        name: ItemName,
        #[command(flatten)]
        options: Box<ItemOptions>,
    },
    Remove {
        name: ItemName,
    },
    /// Replace which events run this item's script.
    Subscribe {
        name: ItemName,
        #[arg(required = true, value_parser = parse_event)]
        events: Vec<Kind>,
    },
}

// Doc comments on these fields are clap's `--help` text, not rustdoc, so
// backticks would show up verbatim in the terminal.
#[allow(clippy::doc_markdown)]
#[derive(Args)]
struct ItemOptions {
    #[arg(long)]
    icon: Option<String>,
    #[arg(long)]
    label: Option<String>,
    /// `Family:Style:Size`, e.g. `Menlo:Bold:14`.
    #[arg(long)]
    icon_font: Option<String>,
    #[arg(long)]
    label_font: Option<String>,
    #[arg(long, value_parser = parse_color)]
    icon_color: Option<u32>,
    #[arg(long, value_parser = parse_color)]
    label_color: Option<u32>,
    #[arg(long, value_parser = parse_color)]
    background_color: Option<u32>,
    #[arg(long)]
    corner_radius: Option<f64>,
    #[arg(long)]
    padding_left: Option<f64>,
    #[arg(long)]
    padding_right: Option<f64>,
    #[arg(long, allow_negative_numbers = true)]
    y_offset: Option<f64>,
    #[arg(long, value_parser = parse_position)]
    position: Option<Position>,
    #[arg(long)]
    drawing: Option<bool>,
    /// Shell command run on every update, with RSBAR_NAME, RSBAR_SENDER and
    /// RSBAR_INFO in its environment. Pass an empty string to clear it.
    #[arg(long)]
    script: Option<String>,
    /// Shell command run when the item is clicked, with RSBAR_BUTTON and
    /// RSBAR_MODIFIERS in its environment. Pass an empty string to clear it.
    #[arg(long)]
    click_script: Option<String>,
    /// Mirror a menu bar item, as `Owner,Name`. Empty stops mirroring.
    #[arg(long)]
    alias: Option<String>,
    /// Seconds between routine updates; 0 means event-driven only.
    #[arg(long)]
    update_freq: Option<u32>,
}

#[derive(Subcommand)]
enum QueryWhat {
    Bar,
    Items,
    Item { name: ItemName },
}

fn parse_color(s: &str) -> Result<u32, String> {
    s.parse::<Color>().map(|c| c.0).map_err(|e| e.to_string())
}

fn parse_position(s: &str) -> Result<Position, String> {
    s.parse()
        .map_err(|e: rsbar_protocol::InvalidPosition| e.to_string())
}

fn parse_event(s: &str) -> Result<Kind, String> {
    s.parse()
        .map_err(|e: rsbar_protocol::event::InvalidEvent| e.to_string())
}

fn parse_edge(s: &str) -> Result<Edge, String> {
    match s.to_ascii_lowercase().as_str() {
        "top" => Ok(Edge::Top),
        "bottom" => Ok(Edge::Bottom),
        other => Err(format!("`{other}` is not an edge: expected top or bottom")),
    }
}

impl From<BarOptions> for BarPatch {
    fn from(o: BarOptions) -> Self {
        Self {
            height: o.height,
            edge: o.edge,
            color: o.color,
            margin: o.margin,
            y_offset: o.y_offset,
            corner_radius: o.corner_radius,
            blur_radius: o.blur_radius,
            topmost: o.topmost,
            hidden: o.hidden,
        }
    }
}

impl From<ItemOptions> for ItemPatch {
    fn from(o: ItemOptions) -> Self {
        Self {
            icon: o.icon,
            label: o.label,
            icon_font: o.icon_font,
            label_font: o.label_font,
            icon_color: o.icon_color,
            label_color: o.label_color,
            background_color: o.background_color,
            corner_radius: o.corner_radius,
            padding_left: o.padding_left,
            padding_right: o.padding_right,
            y_offset: o.y_offset,
            position: o.position,
            drawing: o.drawing,
            script: o.script,
            click_script: o.click_script,
            alias: o.alias,
            update_freq: o.update_freq,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let request = match cli.command {
        Command::Bar {
            action: BarAction::Set(options),
        } => Request::SetBar(options.into()),
        Command::Item { action } => match action {
            ItemAction::Add { name, position } => Request::AddItem { name, position },
            ItemAction::Set { name, options } => Request::SetItem {
                name,
                patch: Box::new((*options).into()),
            },
            ItemAction::Remove { name } => Request::RemoveItem(name),
            ItemAction::Subscribe { name, events } => Request::Subscribe { name, events },
        },
        Command::Query { what } => Request::Query(match what {
            QueryWhat::Bar => Query::Bar,
            QueryWhat::Items => Query::Items,
            QueryWhat::Item { name } => Query::Item(name),
        }),
        Command::Trigger { event, info } => {
            let mut event = event.into_event();
            // Only a custom event has somewhere to put free-form data; a
            // built-in's payload is the source's to fill in.
            if let (rsbar_protocol::Event::Custom(custom), Some(text)) = (&mut event, info) {
                custom.data = Json::parse_or_string(&text);
            }
            Request::Trigger(event)
        }
        Command::Update => Request::UpdateAll,
        Command::Reload => Request::Reload,
        Command::Shutdown => Request::Shutdown,
    };

    let service = service_name();
    let sender = match Sender::<Request>::connect(&service) {
        Ok(sender) => sender,
        Err(err) => {
            eprintln!("rsbar is not running ({err})");
            return ExitCode::FAILURE;
        }
    };

    // Blocking on purpose: a one-shot CLI has no executor to yield to.
    match sender.call_blocking::<Response>(&request) {
        Ok(Response::Ok) => ExitCode::SUCCESS,
        Ok(Response::Error(message)) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        Ok(Response::Bar(state)) => {
            println!("{state:#?}");
            ExitCode::SUCCESS
        }
        Ok(Response::Items(items)) => {
            for item in items {
                println!("{item:?}");
            }
            ExitCode::SUCCESS
        }
        Ok(Response::Item(item)) => {
            println!("{item:#?}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
