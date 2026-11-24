use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::{path::PathBuf, time::Duration};
use wave_core::{
    config::{parse_duration, validate_filter},
    engine::Freshness,
    filter::{Bounds, Filter, Sort},
};

#[derive(Parser)]
#[command(version, about = "Find and track issues on Drips Wave")]
pub struct Cli {
    /// Wave program slug. Defaults to default_program in config.
    #[arg(global = true, long, value_name = "SLUG")]
    pub program: Option<String>,

    /// Write a dwave/v1 JSON envelope.
    #[arg(global = true, long, conflicts_with = "ndjson")]
    pub json: bool,

    /// Write one dwave/v1 JSON envelope per output record.
    #[arg(global = true, long)]
    pub ndjson: bool,

    /// Override the SQLite cache path.
    #[arg(global = true, long, env = "DWAVE_DB", value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Override the TOML configuration path.
    #[arg(global = true, long, env = "DWAVE_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Show request metrics on stderr.
    #[arg(global = true, short = 'v', long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Disable ANSI color in human output.
    #[arg(global = true, long)]
    pub no_color: bool,

    /// Refresh the requested data regardless of the TTL.
    #[arg(global = true, long, conflicts_with = "offline")]
    fresh: bool,

    /// Read from the local cache without making requests.
    #[arg(global = true, long)]
    offline: bool,

    /// Cap requests, including retries and GitHub requests.
    #[arg(global = true, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_requests: Option<u32>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn freshness(&self) -> Freshness {
        if self.offline {
            Freshness::Offline
        } else if self.fresh {
            Freshness::Fresh
        } else {
            Freshness::Default
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Show or change the selected Wave program.
    #[command(subcommand)]
    Program(ProgramCommand),
    /// List waves in the selected program.
    Waves {
        /// Show only the current wave.
        #[arg(long)]
        current: bool,
    },
    /// Refresh the cache for a query scope.
    Sync(SyncArgs),
    /// List issues in the selected program.
    Issues(IssuesArgs),
    /// Inspect an issue by reference.
    #[command(subcommand)]
    Issue(IssueCommand),
    /// Manage the watchlist and its change history.
    #[command(subcommand)]
    Watch(WatchCommand),
    /// List, save, or hide named filters.
    #[command(subcommand)]
    Preset(PresetCommand),
    /// Inspect or prune the local cache.
    #[command(subcommand)]
    Cache(CacheCommand),
    /// Read or update configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Subcommand)]
pub enum ProgramCommand {
    /// Show a program and its details.
    Show {
        /// Program slug.
        slug: Option<String>,
    },
    /// Set the default Wave program.
    Use { slug: String },
}

#[derive(Args)]
pub struct IssuesArgs {
    /// Start from a named filter. Explicit flags override its fields.
    #[arg(long, value_name = "NAME")]
    pub preset: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    /// Sort by updated time, creation time, or points, descending.
    #[arg(long, value_enum, default_value = "updated")]
    pub sort: SortArg,

    /// Number of issues per page.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: u32,

    /// One-based result page.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    pub page: u32,

    /// Show only matches discovered since this query's last completed run.
    #[arg(long)]
    pub new_since_last: bool,

    /// Cross-check each displayed issue with GitHub.
    #[arg(long)]
    pub github: bool,

    /// Start a full scoped sync instead of an incremental refresh.
    #[arg(long, conflicts_with = "offline")]
    pub full: bool,

    /// Show the query plan using cached estimates.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Args)]
pub struct SyncArgs {
    /// Select a named filter. Explicit flags override its fields.
    #[arg(long, value_name = "PRESET")]
    pub scope: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    /// Start a full sync instead of an incremental refresh.
    #[arg(long, conflicts_with = "offline")]
    pub full: bool,

    /// Show the query plan using cached estimates.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Subcommand)]
pub enum IssueCommand {
    /// Show one issue.
    Show {
        /// Wave UUID, OWNER/REPO#N, or GitHub issue URL.
        reference: String,

        /// Cross-check the issue with GitHub.
        #[arg(long)]
        github: bool,
    },
}

#[derive(Subcommand)]
pub enum WatchCommand {
    /// Add one or more issues to the watchlist.
    Add {
        /// Wave UUIDs, OWNER/REPO#N references, or GitHub issue URLs.
        #[arg(required = true, value_name = "REF")]
        refs: Vec<String>,

        /// Attach a note to the watch entries.
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove one or more issues from the watchlist.
    Rm {
        /// Wave UUIDs, OWNER/REPO#N references, or GitHub issue URLs.
        #[arg(required = true, value_name = "REF")]
        refs: Vec<String>,
    },
    /// List watched issues.
    List,
    /// Refresh and compare watched issues. Exit 10 when changes are found.
    Check {
        /// Cross-check watched issues with GitHub.
        #[arg(long)]
        github: bool,
    },
    /// Show the snapshot history for one or all watched issues.
    Log {
        /// Wave UUID, OWNER/REPO#N, or GitHub issue URL.
        reference: Option<String>,

        /// Age of the oldest snapshot to show.
        #[arg(long, default_value = "7d", value_parser = duration_arg)]
        since: Duration,
    },
}

#[derive(Subcommand)]
pub enum PresetCommand {
    /// List available filter presets.
    List,
    /// Save a named filter.
    Save {
        name: String,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Remove a custom preset or hide a built-in preset.
    Rm { name: String },
}

#[derive(Subcommand)]
pub enum CacheCommand {
    /// Show counts for the local cache.
    Stats,
    /// Prune cached data older than a cutoff.
    Prune {
        #[arg(long, default_value = "30d", value_parser = duration_arg)]
        older_than: Duration,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Read a setting.
    Get { key: String },
    /// Write a setting.
    Set { key: String, value: String },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SortArg {
    Points,
    Updated,
    Created,
}

impl From<SortArg> for Sort {
    fn from(sort: SortArg) -> Self {
        match sort {
            SortArg::Points => Self::Points,
            SortArg::Updated => Self::Updated,
            SortArg::Created => Self::Created,
        }
    }
}

#[derive(Args)]
pub struct FilterArgs {
    /// Issue state; use all to match any state.
    #[arg(long, value_name = "STATE")]
    state: Option<String>,

    /// Pending applications: N, N..=M, ..=M, or N...
    #[arg(long, value_name = "COUNT_OR_RANGE", value_parser = pending_arg)]
    pending: Option<Bounds>,

    /// Match issues that have never had an application.
    #[arg(long, conflicts_with = "has_applications")]
    never_applied: bool,

    /// Match issues that have had at least one application.
    #[arg(long)]
    has_applications: bool,

    /// Match assigned issues.
    #[arg(long, conflicts_with = "unassigned")]
    assigned: bool,

    /// Match unassigned issues.
    #[arg(long)]
    unassigned: bool,

    /// Match issues with a linked PR in Wave.
    #[arg(long, conflicts_with = "no_pr")]
    has_pr: bool,

    /// Match issues with no linked PR in Wave.
    #[arg(long)]
    no_pr: bool,

    /// Minimum issue points, inclusive.
    #[arg(long)]
    min_points: Option<u32>,

    /// Maximum issue points, inclusive.
    #[arg(long)]
    max_points: Option<u32>,

    /// Limit to repositories. Repeat to match any supplied repository.
    #[arg(long = "repo", value_name = "OWNER/REPO")]
    repos: Vec<String>,

    /// Require this label. Repeat to require additional labels.
    #[arg(long = "label", value_name = "NAME")]
    labels: Vec<String>,

    /// Search issue titles.
    #[arg(long)]
    search: Option<String>,

    /// Match issues updated within this duration (for example, 7d).
    #[arg(long, value_parser = duration_arg)]
    updated_since: Option<Duration>,
}

impl FilterArgs {
    pub fn apply(&self, mut filter: Filter) -> anyhow::Result<Filter> {
        if let Some(state) = &self.state {
            let state = state.to_ascii_lowercase();
            filter.state = (state != "all").then_some(state);
        }
        if let Some(bounds) = self.pending {
            filter.pending = Some(bounds);
        }
        if let Some(value) = switch_pair(self.never_applied, self.has_applications) {
            filter.never_applied = Some(value);
        }
        if let Some(value) = switch_pair(self.assigned, self.unassigned) {
            filter.assigned = Some(value);
        }
        if let Some(value) = switch_pair(self.has_pr, self.no_pr) {
            filter.has_pr = Some(value);
        }
        if self.min_points.is_some() || self.max_points.is_some() {
            let points = filter.points.get_or_insert(Bounds {
                min: None,
                max: None,
            });
            points.min = self.min_points.or(points.min);
            points.max = self.max_points.or(points.max);
        }
        if !self.repos.is_empty() {
            filter.repos.clone_from(&self.repos);
        }
        if !self.labels.is_empty() {
            filter.labels.clone_from(&self.labels);
        }
        if let Some(search) = &self.search {
            filter.search = Some(search.clone());
        }
        if let Some(duration) = self.updated_since {
            filter.updated_since = Some(duration.as_secs());
        }
        validate_filter(&filter)?;
        Ok(filter)
    }
}

fn switch_pair(positive: bool, negative: bool) -> Option<bool> {
    (positive || negative).then_some(positive)
}

fn pending_arg(raw: &str) -> Result<Bounds, String> {
    let error =
        || format!("invalid pending applications range {raw:?}; use N, N..=M, ..=M, or N..");
    let number = |part: &str| part.parse::<u32>().map_err(|_| error());
    if let Some((min, max)) = raw.split_once("..=") {
        let min = (!min.is_empty()).then(|| number(min)).transpose()?;
        let max = number(max)?;
        if min.is_some_and(|min| min > max) {
            return Err(error());
        }
        return Ok(Bounds {
            min,
            max: Some(max),
        });
    }
    if let Some(min) = raw.strip_suffix("..") {
        return Ok(Bounds {
            min: Some(number(min)?),
            max: None,
        });
    }
    let value = number(raw)?;
    Ok(Bounds {
        min: Some(value),
        max: Some(value),
    })
}

fn duration_arg(raw: &str) -> Result<Duration, String> {
    parse_duration(raw).map_err(|error| error.to_string())
}
