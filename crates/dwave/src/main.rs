mod args;
mod render;

use anyhow::{anyhow, Context, Result};
use args::{
    CacheCommand, Cli, Command, ConfigCommand, FilterArgs, IssueCommand, IssuesArgs, PresetCommand,
    ProgramCommand, SyncArgs, WatchCommand,
};
use chrono::{DateTime, TimeDelta, Utc};
use clap::Parser;
use render::{Report, View};
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::BTreeSet, io, process::ExitCode, sync::Arc, time::Duration};
use wave_core::{
    config::{
        default_config_path, default_db_path, parse_duration, validate_slug, Config, ConfigFile,
    },
    engine::{Engine, EngineConfig, Freshness, IssueReference, ListingOptions, Refresh},
    filter::Filter,
    gateway::{Gateway, HostPolicy},
    github::GitHubClient,
    source::HttpWaveSource,
    store::Store,
    watch::{WatchEntry, Watcher},
};

#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(error: anyhow::Error) -> anyhow::Error {
    UsageError(format!("{error:#}")).into()
}

fn main() -> ExitCode {
    match run() {
        Ok(exit) => exit,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {}", render::clean(&format!("{error:#}")));
            if error.is::<UsageError>() {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let now = Utc::now();
    let config_file = ConfigFile::new(cli.config.clone().map_or_else(default_config_path, Ok)?);

    let mut report = match &cli.command {
        Command::Program(ProgramCommand::Use { slug }) => {
            validate_slug(slug).map_err(usage)?;
            config_file.update(|config| {
                config.default_program.clone_from(slug);
                Ok(())
            })?;
            Report::new(View::Fields, json!({ "default_program": slug }), Some(now)).program(slug)
        }
        Command::Config(command) => config_command(&config_file, command, now)?,
        Command::Preset(command) => preset_command(&config_file, command, now)?,
        Command::Cache(command) => cache_command(command, cli.db.as_deref(), now)?,
        _ => {
            let config = config_file.load()?;
            let slug = cli.program.as_deref().unwrap_or(&config.default_program);
            let program =
                if let Command::Program(ProgramCommand::Show { slug: Some(slug) }) = &cli.command {
                    slug
                } else {
                    slug
                };
            validate_slug(program).map_err(usage)?;
            let db = cli.db.clone().map_or_else(default_db_path, Ok)?;
            let mut app = App::new(
                &config,
                Store::open(db)?,
                cli.max_requests,
                cli.freshness(),
                now,
            )?;
            let mut report = app.run(&cli.command, program)?;
            report.meta.metrics = app.gateway.metrics();
            report
        }
    };

    report.meta.warnings.sort();
    report.meta.warnings.dedup();
    render::write(&report, &cli)?;
    Ok(if report.changed {
        ExitCode::from(10)
    } else if report.failed_check {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn config_command(
    file: &ConfigFile,
    command: &ConfigCommand,
    now: DateTime<Utc>,
) -> Result<Report> {
    let (key, value) = match command {
        ConfigCommand::Get { key } => (key, file.load()?.get(key).map_err(usage)?),
        ConfigCommand::Set { key, value } => (
            key,
            file.update(|config| {
                config.set(key, value).map_err(usage)?;
                config.get(key)
            })?,
        ),
    };
    Ok(Report::new(
        View::Setting,
        json!({ "key": key, "value": value }),
        Some(now),
    ))
}

fn preset_command(
    file: &ConfigFile,
    command: &PresetCommand,
    now: DateTime<Utc>,
) -> Result<Report> {
    let (view, data) = match command {
        PresetCommand::List => (View::Presets, to_json(file.load()?.presets())?),
        PresetCommand::Save { name, filter } => {
            let filter = filter.apply(Filter::default()).map_err(usage)?;
            let preset = file.update(|config| {
                config.save_preset(name, filter).map_err(usage)?;
                config.preset(name)
            })?;
            (View::Presets, json!([preset]))
        }
        PresetCommand::Rm { name } => {
            file.update(|config| config.remove_preset(name).map_err(usage))?;
            (View::Fields, json!({ "removed_preset": name }))
        }
    };
    Ok(Report::new(view, data, Some(now)))
}

fn cache_command(
    command: &CacheCommand,
    db: Option<&std::path::Path>,
    now: DateTime<Utc>,
) -> Result<Report> {
    let mut store = Store::open(db.map_or_else(default_db_path, |db| Ok(db.to_owned()))?)?;
    let data = match command {
        CacheCommand::Stats => to_json(store.stats()?)?,
        CacheCommand::Prune { older_than } => to_json(store.prune(cutoff(now, *older_than)?)?)?,
    };
    Ok(Report::new(View::Fields, data, Some(now)))
}

struct App<'a> {
    config: &'a Config,
    store: Store,
    gateway: Arc<Gateway>,
    source: HttpWaveSource,
    engine_config: EngineConfig,
    freshness: Freshness,
    now: DateTime<Utc>,
}

impl<'a> App<'a> {
    fn new(
        config: &'a Config,
        store: Store,
        max_requests: Option<u32>,
        freshness: Freshness,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        let max_requests = max_requests.unwrap_or(config.max_requests);
        let gateway = Arc::new(Gateway::new(
            max_requests,
            HostPolicy {
                rate_per_second: config.wave_rate,
                burst: config.wave_burst,
                ..HostPolicy::default()
            },
        )?);
        let engine_config = EngineConfig {
            program_ttl: parse_duration(&config.program_ttl)?,
            waves_ttl: parse_duration(&config.waves_ttl)?,
            listing_ttl: parse_duration(&config.listing_ttl)?,
            issue_ttl: parse_duration(&config.issue_ttl)?,
            overlap: parse_duration(&config.overlap)?,
            full_interval: parse_duration(&config.full_interval)?,
            max_requests: u64::from(max_requests),
        };
        Ok(Self {
            config,
            store,
            source: HttpWaveSource::new(Arc::clone(&gateway)),
            gateway,
            engine_config,
            freshness,
            now,
        })
    }

    fn run(&mut self, command: &Command, program: &str) -> Result<Report> {
        match command {
            Command::Program(ProgramCommand::Show { .. }) => {
                let freshness = self.freshness;
                let refresh = self.engine().program(program, freshness)?;
                Ok(report_for(View::Fields, refresh)?.program(program))
            }
            Command::Waves { current } => self.waves(program, *current),
            Command::Issues(args) => self.issues(program, args),
            Command::Sync(args) => self.sync(program, args),
            Command::Issue(IssueCommand::Show { reference, github }) => {
                self.issue(program, reference, *github)
            }
            Command::Watch(command) => self.watch(program, command),
            _ => unreachable!("local commands are handled before opening the engine"),
        }
    }

    fn engine(&mut self) -> Engine<'_, HttpWaveSource> {
        Engine::new(&mut self.store, &self.source, self.engine_config.clone())
    }

    fn github(&self) -> Result<GitHubClient> {
        Ok(GitHubClient::new(
            Arc::clone(&self.gateway),
            parse_duration(&self.config.github_ttl)?,
        ))
    }

    fn waves(&mut self, program: &str, current: bool) -> Result<Report> {
        let freshness = self.freshness;
        let mut engine = self.engine();
        let resolved = engine.program(program, freshness)?;
        let id = issue_id(&resolved.data).context("program response missing its ID")?;
        let mut refresh = engine.waves(id, freshness)?;
        if current {
            let now = self.now;
            refresh.data.retain(|wave| active_wave(wave, now));
        }
        refresh.warnings.extend(resolved.warnings);
        refresh.partial |= resolved.partial;
        Ok(report_for(View::Waves, refresh)?.program(program))
    }

    fn issues(&mut self, program: &str, args: &IssuesArgs) -> Result<Report> {
        let filter = self.filter(args.preset.as_deref(), &args.filter)?;
        let options = ListingOptions {
            freshness: self.freshness,
            sort: args.sort.into(),
            limit: args.limit,
            page: args.page,
            new_since_last: args.new_since_last,
            full: args.full,
            sync: false,
        };
        if args.explain {
            return self.explain(program, &filter, options);
        }
        let listing = self.engine().list(program, &filter, options)?;
        let changed = args.new_since_last && !listing.data.is_empty();
        let mut report =
            Report::new(View::Issues, Value::Array(listing.data), listing.as_of).program(program);
        report.page = Some(render::Page {
            page: args.page,
            limit: args.limit,
            total: listing.total,
        });
        report.meta.partial = listing.partial;
        report.meta.warnings = listing.warnings;
        report.changed = changed;
        if let Some(issues) = report
            .data
            .as_array_mut()
            .filter(|issues| args.github && !issues.is_empty())
        {
            let client = self.github()?;
            for issue in issues {
                self.enrich(&client, issue, &mut report.meta)?;
            }
        }
        report.failed_check = args.new_since_last && report.meta.partial;
        Ok(report)
    }

    fn sync(&mut self, program: &str, args: &SyncArgs) -> Result<Report> {
        let filter = self.filter(args.scope.as_deref(), &args.filter)?;
        let options = ListingOptions {
            freshness: self.freshness,
            full: args.full,
            sync: true,
            limit: 1,
            ..ListingOptions::default()
        };
        if args.explain {
            return self.explain(program, &filter, options);
        }
        let listing = self.engine().list(program, &filter, options)?;
        let mut report = Report::new(
            View::Plan,
            json!({ "plan": listing.plan, "total": listing.total }),
            listing.as_of,
        )
        .program(program);
        report.meta.partial = listing.partial;
        report.meta.warnings = listing.warnings;
        Ok(report)
    }

    fn filter(&self, name: Option<&str>, args: &FilterArgs) -> Result<Filter> {
        let preset = name
            .map(|name| self.config.preset(name))
            .transpose()
            .map_err(usage)?;
        let requires_repo = preset.as_ref().is_some_and(|preset| preset.requires_repo);
        let filter = args
            .apply(preset.map_or_else(Filter::default, |preset| preset.filter))
            .map_err(usage)?;
        if requires_repo && filter.repos.is_empty() {
            return Err(usage(anyhow!(
                "this preset requires at least one --repo OWNER/REPO"
            )));
        }
        Ok(filter)
    }

    fn explain(
        &mut self,
        program: &str,
        filter: &Filter,
        options: ListingOptions,
    ) -> Result<Report> {
        let plan = self.engine().explain(program, filter, options)?;
        Ok(Report::new(View::Plan, to_json(plan)?, Some(self.now)).program(program))
    }

    fn issue(&mut self, program: &str, reference: &str, github: bool) -> Result<Report> {
        let freshness = self.freshness;
        let refresh = self.resolve_issue(program, reference, freshness)?;
        let mut report = report_for(View::Issue, refresh)?.program(program);
        if github {
            let client = self.github()?;
            self.enrich(&client, &mut report.data, &mut report.meta)?;
        }
        Ok(report)
    }

    fn enrich(
        &self,
        client: &GitHubClient,
        issue: &mut Value,
        meta: &mut render::Meta,
    ) -> Result<()> {
        let enrichment = client.enrich_issue(&self.store, issue, self.freshness, self.now)?;
        if let Some(fields) = issue.as_object_mut() {
            fields.insert("github".into(), to_json(enrichment.status)?);
            fields.insert("github_as_of".into(), to_json(enrichment.as_of)?);
        }
        meta.partial |= enrichment.partial;
        meta.warnings.extend(enrichment.warnings);
        Ok(())
    }

    fn watch(&mut self, program: &str, command: &WatchCommand) -> Result<Report> {
        let now = self.now;
        let mut warnings = Vec::new();
        let mut partial = false;
        let (view, data) = match command {
            WatchCommand::Add { refs, note } => {
                let mut ids = BTreeSet::new();
                for reference in refs {
                    let refresh = self.resolve_issue(program, reference, self.freshness)?;
                    ids.insert(issue_id(&refresh.data)?.to_owned());
                    warnings.extend(refresh.warnings);
                    partial |= refresh.partial;
                }
                for id in &ids {
                    self.store.watch_add(id, note.as_deref(), now)?;
                }
                let entries = self
                    .store
                    .watchlist()?
                    .into_iter()
                    .filter(|entry: &WatchEntry| ids.contains(&entry.issue_id))
                    .collect::<Vec<_>>();
                (View::WatchList, to_json(entries)?)
            }
            WatchCommand::Rm { refs } => {
                let mut ids = BTreeSet::new();
                for reference in refs {
                    ids.insert(self.history_id(program, reference, self.freshness)?);
                }
                let mut removed = Vec::new();
                for id in ids {
                    removed
                        .push(json!({ "issue_id": id, "removed": self.store.watch_remove(&id)? }));
                }
                (View::WatchRemoved, Value::Array(removed))
            }
            WatchCommand::List => (View::WatchList, to_json(self.store.watchlist()?)?),
            WatchCommand::Check { github } => {
                let client = github.then(|| self.github()).transpose()?;
                let mut watcher = Watcher::new(
                    &mut self.store,
                    &self.source,
                    now,
                    self.engine_config.issue_ttl,
                    self.freshness,
                );
                let check = watcher.check(client.as_ref())?;
                let changed = !check.changes.is_empty();
                let mut report = Report::new(View::Changes, to_json(check.changes)?, check.as_of);
                report.meta.warnings = check.warnings;
                report.meta.partial = check.partial;
                report.changed = changed;
                report.failed_check = check.partial;
                return Ok(report);
            }
            WatchCommand::Log { reference, since } => {
                let id = reference
                    .as_deref()
                    .map(|reference| self.history_id(program, reference, Freshness::Offline))
                    .transpose()?;
                (
                    View::History,
                    to_json(
                        self.store
                            .watch_log(id.as_deref(), Some(cutoff(now, *since)?))?,
                    )?,
                )
            }
        };
        let mut report = Report::new(view, data, Some(now));
        report.meta.warnings = warnings;
        report.meta.partial = partial;
        Ok(report)
    }

    fn history_id(
        &mut self,
        program: &str,
        reference: &str,
        freshness: Freshness,
    ) -> Result<String> {
        if let IssueReference::Id(id) = IssueReference::parse(reference).map_err(usage)? {
            return Ok(id);
        }
        let refresh = self.resolve_issue(program, reference, freshness)?;
        Ok(issue_id(&refresh.data)?.into())
    }

    fn resolve_issue(
        &mut self,
        program: &str,
        reference: &str,
        freshness: Freshness,
    ) -> Result<Refresh<Value>> {
        IssueReference::parse(reference).map_err(usage)?;
        let refresh = self.engine().issue(reference, freshness, program)?;
        let data = refresh
            .data
            .ok_or_else(|| missing_issue(reference, &refresh.warnings))?;
        Ok(Refresh {
            data,
            as_of: refresh.as_of,
            warnings: refresh.warnings,
            partial: refresh.partial,
        })
    }
}

fn issue_id(value: &Value) -> Result<&str> {
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("Wave response is missing an ID"))
}

fn missing_issue(reference: &str, warnings: &[String]) -> anyhow::Error {
    let hint = warnings.join("; ");
    if hint.is_empty() {
        anyhow!("issue {reference:?} not found")
    } else {
        anyhow!("issue {reference:?} not found ({hint})")
    }
}

fn cutoff(now: DateTime<Utc>, duration: Duration) -> Result<DateTime<Utc>> {
    TimeDelta::from_std(duration)
        .ok()
        .and_then(|delta| now.checked_sub_signed(delta))
        .ok_or_else(|| usage(anyhow!("duration is outside the supported calendar")))
}

fn active_wave(wave: &Value, now: DateTime<Utc>) -> bool {
    let timestamp = |field| {
        DateTime::parse_from_rfc3339(wave.get(field)?.as_str()?)
            .ok()
            .map(|date| date.with_timezone(&Utc))
    };
    let start = timestamp("startDate");
    let end = timestamp("endDate");
    if start.is_some_and(|start| now < start) || end.is_some_and(|end| now >= end) {
        return false;
    }
    let status = wave
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match status.as_str() {
        "active" | "current" | "ongoing" | "in_progress" | "in-progress" => true,
        "closed" | "ended" | "completed" | "cancelled" | "canceled" => false,
        _ => start.is_some() && end.is_some(),
    }
}

fn report_for<T: Serialize>(view: View, refresh: Refresh<T>) -> Result<Report> {
    let mut report = Report::new(view, to_json(refresh.data)?, refresh.as_of);
    report.meta.partial = refresh.partial;
    report.meta.warnings = refresh.warnings;
    Ok(report)
}

fn to_json(value: impl Serialize) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}
