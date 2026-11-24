use crate::filter::{Bounds, Filter};
use anyhow::{anyhow, bail, ensure, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_program: String,
    pub program_ttl: String,
    pub waves_ttl: String,
    pub listing_ttl: String,
    pub issue_ttl: String,
    pub github_ttl: String,
    pub overlap: String,
    pub full_interval: String,
    pub max_requests: u32,
    pub wave_rate: f64,
    pub wave_burst: u32,
    pub high_value_min_points: u32,
    pub hidden_presets: Vec<String>,
    pub presets: BTreeMap<String, Filter>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_program: "stellar".into(),
            program_ttl: "7d".into(),
            waves_ttl: "6h".into(),
            listing_ttl: "10m".into(),
            issue_ttl: "5m".into(),
            github_ttl: "30m".into(),
            overlap: "10m".into(),
            full_interval: "24h".into(),
            max_requests: 50,
            wave_rate: 2.0,
            wave_burst: 4,
            high_value_min_points: 1000,
            hidden_presets: Vec::new(),
            presets: BTreeMap::new(),
        }
    }
}

#[derive(Serialize)]
pub struct Preset {
    pub name: String,
    pub filter: Filter,
    pub estimated_cost: &'static str,
    pub requires_repo: bool,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        validate_slug(&self.default_program)?;
        for (key, value) in [
            ("program_ttl", &self.program_ttl),
            ("waves_ttl", &self.waves_ttl),
            ("listing_ttl", &self.listing_ttl),
            ("issue_ttl", &self.issue_ttl),
            ("github_ttl", &self.github_ttl),
            ("overlap", &self.overlap),
            ("full_interval", &self.full_interval),
        ] {
            parse_duration(value).with_context(|| format!("invalid {key}"))?;
        }
        ensure!(self.max_requests > 0, "max_requests must be at least 1");
        ensure!(
            self.wave_rate.is_finite() && self.wave_rate > 0.0,
            "wave_rate must be a positive finite number"
        );
        ensure!(self.wave_burst > 0, "wave_burst must be at least 1");
        for (name, filter) in &self.presets {
            validate_preset_name(name)?;
            validate_filter(filter).with_context(|| format!("invalid preset {name}"))?;
        }
        for name in &self.hidden_presets {
            validate_preset_name(name)?;
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<toml::Value> {
        let values = toml::Table::try_from(self)?;
        values
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("unknown config key {key:?}"))
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let mut table = toml::Table::try_from(&*self)?;
        let current = table
            .get(key)
            .ok_or_else(|| anyhow!("unknown config key {key:?}"))?;
        ensure!(key != "presets", "use 'dwave preset save' to set presets");
        let replacement = if current.is_str() {
            toml::Value::String(value.to_owned())
        } else {
            let mut parsed: toml::Table =
                toml::from_str(&format!("value = {value}")).context("invalid TOML value")?;
            ensure!(parsed.len() == 1, "expected a single TOML value");
            parsed
                .remove("value")
                .ok_or_else(|| anyhow!("expected a TOML value"))?
        };
        table.insert(key.to_owned(), replacement);
        let next: Self = toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("invalid value for {key}"))?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn preset(&self, name: &str) -> Result<Preset> {
        self.presets()
            .into_iter()
            .find(|preset| preset.name == name)
            .ok_or_else(|| anyhow!("unknown preset {name:?}; run 'dwave preset list'"))
    }

    pub fn presets(&self) -> Vec<Preset> {
        let mut presets = self.builtins();
        presets.retain(|name, _| !self.hidden_presets.contains(name));
        for (name, filter) in &self.presets {
            let mut filter = filter.clone();
            if filter.state.as_deref() == Some("all") {
                filter.state = None;
            }
            let cost = if filter.pending.is_some()
                || filter.assigned.is_some()
                || filter.points.is_some()
                || !filter.labels.is_empty()
                || filter.updated_since.is_some()
            {
                "scoped"
            } else if !filter.repos.is_empty() {
                "repository-dependent"
            } else {
                "cheap"
            };
            presets.insert(name.clone(), make_preset(name, filter, cost, false));
        }
        presets.into_values().collect()
    }

    pub fn save_preset(&mut self, name: &str, mut filter: Filter) -> Result<()> {
        validate_preset_name(name)?;
        validate_filter(&filter)?;
        // An omitted state in a hand-written preset inherits open; persist all explicitly.
        if filter.state.is_none() {
            filter.state = Some("all".into());
        }
        self.hidden_presets.retain(|hidden| hidden != name);
        self.presets.insert(name.into(), filter);
        Ok(())
    }

    pub fn remove_preset(&mut self, name: &str) -> Result<()> {
        self.preset(name)?;
        self.presets.remove(name);
        if self.builtins().contains_key(name) && !self.hidden_presets.iter().any(|n| n == name) {
            self.hidden_presets.push(name.into());
        }
        Ok(())
    }

    fn builtins(&self) -> BTreeMap<String, Preset> {
        let open = Filter::default();
        [
            make_preset(
                "untouched",
                Filter {
                    never_applied: Some(true),
                    has_pr: Some(false),
                    ..open.clone()
                },
                "cheap",
                false,
            ),
            make_preset(
                "open-pending-0",
                Filter {
                    pending: Some(Bounds {
                        min: Some(0),
                        max: Some(0),
                    }),
                    assigned: Some(false),
                    has_pr: Some(false),
                    ..open.clone()
                },
                "broad",
                false,
            ),
            make_preset(
                "unassigned",
                Filter {
                    assigned: Some(false),
                    has_pr: Some(false),
                    ..open.clone()
                },
                "broad",
                false,
            ),
            make_preset(
                "high-value",
                Filter {
                    assigned: Some(false),
                    points: Some(Bounds {
                        min: Some(self.high_value_min_points),
                        max: None,
                    }),
                    ..open.clone()
                },
                "broad",
                false,
            ),
            make_preset("repo", open, "repository-dependent", true),
        ]
        .into_iter()
        .map(|preset| (preset.name.clone(), preset))
        .collect()
    }
}

fn make_preset(
    name: &str,
    filter: Filter,
    estimated_cost: &'static str,
    requires_repo: bool,
) -> Preset {
    Preset {
        name: name.to_owned(),
        filter,
        estimated_cost,
        requires_repo,
    }
}

pub fn validate_slug(slug: &str) -> Result<()> {
    ensure!(
        !slug.is_empty()
            && slug
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
        "invalid program slug {slug:?}"
    );
    Ok(())
}

pub fn validate_preset_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
        "invalid preset name {name:?}; use letters, numbers, hyphens, or underscores"
    );
    Ok(())
}

pub fn validate_filter(filter: &Filter) -> Result<()> {
    if let Some(state) = &filter.state {
        ensure!(
            !state.is_empty() && !state.chars().any(|c| c.is_whitespace() || c.is_control()),
            "invalid issue state {state:?}"
        );
    }
    for (name, bounds) in [("pending", &filter.pending), ("points", &filter.points)] {
        if let Some(Bounds {
            min: Some(min),
            max: Some(max),
        }) = bounds
        {
            ensure!(min <= max, "{name} minimum must not exceed its maximum");
        }
    }
    for repo in &filter.repos {
        let part_ok = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        };
        ensure!(
            repo.split_once('/')
                .is_some_and(|(owner, name)| part_ok(owner) && part_ok(name)),
            "invalid repository {repo:?}; expected OWNER/REPO"
        );
    }
    Ok(())
}

pub fn parse_duration(value: &str) -> Result<Duration> {
    let Some((quantity, units)) = value.split_at_checked(value.len().saturating_sub(1)) else {
        bail!("invalid duration {value:?}; expected a number followed by s, m, h, d, or w");
    };
    let multiplier = match units {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => bail!("invalid duration {value:?}; expected a number followed by s, m, h, d, or w"),
    };
    let seconds = quantity
        .parse::<u64>()
        .ok()
        .and_then(|quantity| quantity.checked_mul(multiplier))
        .and_then(|seconds| chrono::TimeDelta::try_seconds(seconds.try_into().ok()?))
        .ok_or_else(|| anyhow!("invalid duration {value:?}"))?;
    Ok(Duration::from_secs(seconds.num_seconds().try_into()?))
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

pub fn default_db_path() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().join("dwave.db"))
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "dwave")
        .ok_or_else(|| anyhow!("cannot locate the user data directory"))
}

pub struct ConfigFile {
    path: PathBuf,
}

impl ConfigFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Config> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()))
            }
        };
        let config: Config =
            toml::from_str(&contents).with_context(|| format!("parse {}", self.path.display()))?;
        config
            .validate()
            .with_context(|| format!("validate {}", self.path.display()))?;
        Ok(config)
    }

    /// Reload under the same lock used for writes to preserve simultaneous edits.
    pub fn update<T>(&self, edit: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let lock_path = sibling_with_suffix(&self.path, ".lock")?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("lock {}", lock_path.display()))?;

        let mut config = self.load()?;
        let output = edit(&mut config)?;
        config.validate()?;
        let encoded = toml::to_string_pretty(&config)?;
        let staging = sibling_with_suffix(&self.path, ".next")?;
        {
            let mut file =
                File::create(&staging).with_context(|| format!("open {}", staging.display()))?;
            file.write_all(encoded.as_bytes())
                .with_context(|| format!("write {}", staging.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", staging.display()))?;
        }
        fs::rename(&staging, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))?;
        Ok(output)
    }
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| anyhow!("invalid config path {}", path.display()))?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}
