use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::collections::BTreeMap;
use std::time::{SystemTime, Duration};

use anyhow::Context;
use async_std::task;
use fn_error_context::context;
use linked_hash_map::LinkedHashMap;
use serde::{Serialize, Deserialize};

use edgedb_client as client;
use crate::server::control;
use crate::server::detect::{self, VersionQuery};
use crate::server::init::{init, Metadata, data_path};
use crate::server::install;
use crate::server::options::{self, Upgrade};
use crate::server::os_trait::Method;
use crate::server::version::Version;
use crate::server::is_valid_name;
use crate::commands;
use crate::process::ProcessGuard;


#[derive(Serialize, Deserialize, Debug)]
pub struct UpgradeMeta {
    pub source: Version<String>,
    pub target: Version<String>,
    #[serde(with="humantime_serde")]
    pub started: SystemTime,
    pub pid: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BackupMeta {
    #[serde(with="humantime_serde")]
    pub timestamp: SystemTime,
}

struct Instance {
    name: String,
    meta: Metadata,
    system: bool,
    data_dir: PathBuf,
    source: Option<Version<String>>,
    version: Option<Version<String>>,
}

enum ToDo {
    MinorUpgrade,
    InstanceUpgrade(String, VersionQuery),
    NightlyUpgrade,
}

struct InstanceIterator {
    dir: fs::ReadDir,
    path: PathBuf,
}


fn interpret_options(options: &Upgrade) -> ToDo {
    if let Some(name) = &options.name {
        if options.nightly {
            eprintln!("Cannot upgrade specific nightly instance, \
                use `--to-nightly` to upgrade to nightly. \
                Use `--nightly` without instance name to upgrade all nightly \
                instances");
        }
        let nver = if options.to_nightly {
            VersionQuery::Nightly
        } else if let Some(ver) = &options.to_version {
            VersionQuery::Stable(Some(ver.clone()))
        } else {
            VersionQuery::Stable(None)
        };
        ToDo::InstanceUpgrade(name.into(), nver)
    } else if options.nightly {
        ToDo::NightlyUpgrade
    } else {
        ToDo::MinorUpgrade
    }
}

fn all_instances() -> anyhow::Result<Vec<Instance>> {
    let path = data_path(false)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    InstanceIterator {
        dir: fs::read_dir(&path)?,
        path: path.into(),
    }.collect::<Result<Vec<_>,_>>()
}

fn read_metadata(path: &Path) -> anyhow::Result<Metadata> {
    let file = fs::read(path)
        .with_context(|| format!("error reading {}", path.display()))?;
    let metadata = serde_json::from_slice(&file)
        .with_context(|| format!("error decoding json {}", path.display()))?;
    Ok(metadata)
}

impl Iterator for InstanceIterator {
    type Item = anyhow::Result<Instance>;
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(item) = self.dir.next() {
            match self.read_item(item).transpose() {
                None => continue,
                val => return val,
            }
        }
        return None;
    }
}

impl InstanceIterator {
    fn read_item(&self, item: Result<fs::DirEntry, io::Error>)
        -> anyhow::Result<Option<Instance>>
    {
        let item = item.with_context(
            || format!("error listing instances dir {}",
                       self.path.display()))?;
        if !item.file_type()
            .with_context(|| format!(
                "error listing {}: cannot determine entry type",
                self.path.display()))?
            .is_dir()
        {
            return Ok(None);
        }
        if let Some(name) = item.file_name().to_str() {
            if !is_valid_name(name) {
                return Ok(None);
            }
            let meta = match
                read_metadata(&item.path().join("metadata.json"))
            {
                Ok(metadata) => metadata,
                Err(e) => {
                    log::warn!(target: "edgedb::server::upgrade",
                        "Error reading metadata for \
                        instance {:?}: {:#}. Skipping...",
                        name, e);
                    return Ok(None);
                }
            };
            return Ok(Some(Instance {
                    name: name.into(),
                    meta,
                    system: false,
                    data_dir: item.path(),
                    source: None,
                    version: None,
            }));
        } else {
            return Ok(None);
        }
    }
}

fn get_instances(todo: &ToDo)
    -> anyhow::Result<Vec<Instance>>
{
    use ToDo::*;

    let instances = match todo {
        MinorUpgrade => all_instances()?.into_iter()
            .filter(|inst| !inst.meta.nightly)
            .collect(),
        NightlyUpgrade => all_instances()?.into_iter()
            .filter(|inst| inst.meta.nightly)
            .collect(),
        InstanceUpgrade(name, ..) => all_instances()?.into_iter()
            .filter(|inst| &inst.name == name)
            .collect(),
    };
    Ok(instances)
}

pub fn upgrade(options: &Upgrade) -> anyhow::Result<()> {
    use ToDo::*;

    let todo = interpret_options(&options);
    let instances = get_instances(&todo)?;
    if instances.is_empty() {
        if options.nightly {
            log::warn!(target: "edgedb::server::upgrade",
                "No instances found. Nothing to upgrade.");
        } else {
            log::warn!(target: "edgedb::server::upgrade",
                "No instances found. Nothing to upgrade \
                (Note: nightly instances are upgraded only if `--nightly` \
                is specified).");
        }
        return Ok(());
    }
    let mut by_method = BTreeMap::new();
    for instance in instances {
        by_method.entry(instance.meta.method.clone())
            .or_insert_with(Vec::new)
            .push(instance);
    }

    let os = detect::current_os()?;
    let avail = os.get_available_methods()?;
    for (meth_name, instances) in by_method {
        if !avail.is_supported(&meth_name) {
            log::warn!(target: "edgedb::server::upgrade",
                "method {} is not available. \
                Instances using it {}. Skipping...",
                meth_name.title(),
                instances
                    .iter()
                    .map(|inst| &inst.name[..])
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            continue;
        }
        let method = os.make_method(&meth_name, &avail)?;
        match todo {
            MinorUpgrade => {
                do_minor_upgrade(&*method, instances, options)?;
            }
            NightlyUpgrade => {
                do_nightly_upgrade(&*method, instances, options)?;
            }
            InstanceUpgrade(.., ref version) => {
                for inst in instances {
                    do_instance_upgrade(&*method, inst, version, options)?;
                }
            }
        }
    }
    Ok(())
}

fn do_minor_upgrade(method: &dyn Method,
    instances: Vec<Instance>, options: &Upgrade)
    -> anyhow::Result<()>
{
    let mut by_major = BTreeMap::new();
    for inst in instances {
        by_major.entry(inst.meta.version.clone())
            .or_insert_with(Vec::new)
            .push(inst);
    }
    for (version, mut instances) in by_major {
        let instances_str = instances
            .iter().map(|inst| &inst.name[..]).collect::<Vec<_>>().join(", ");

        let version_query = VersionQuery::Stable(Some(version.clone()));
        let new = method.get_version(&version_query)
            .context("Unable to determine version")?;
        let old = get_installed(&version_query, method)?;

        if !options.force {
            if let Some(old_ver) = &old {
                if old_ver >= &new.full_version() {
                    log::info!(target: "edgedb::server::upgrade",
                        "Version {} is up to date {}, skipping instances: {}",
                        version, old_ver, instances_str);
                    return Ok(());
                }
            }
        }

        println!("Upgrading version: {} to {}-{}, instances: {}",
            version, new.version, new.revision, instances_str);
        for inst in &mut instances {
            inst.source = old.clone();
            inst.version = Some(new.full_version());
        }

        // Stop instances first.
        //
        // This (launchctl unload) is required for MacOS to reinstall
        // the pacakge. On other systems, this is also useful as in-place
        // modifying the running package isn't very good idea.
        for inst in &mut instances {
            let mut ctl = inst.get_control()?;
            ctl.stop(&options::Stop { name: inst.name.clone() })
                .map_err(|e| {
                    log::warn!("Failed to stop instance {:?}: {:#}",
                        inst.name, e);
                })
                .ok();
        }

        log::info!(target: "edgedb::server::upgrade", "Upgrading the package");
        method.install(&install::Settings {
            method: method.name(),
            package_name: new.package_name,
            major_version: version,
            version: new.version,
            nightly: false,
            extra: LinkedHashMap::new(),
        })?;

        for inst in &instances {
            let mut ctl = inst.get_control()?;
            ctl.start(&options::Start {
                name: inst.name.clone(),
                foreground: false,
            })?;
        }
    }
    Ok(())
}

async fn dump_instance(inst: &Instance, socket: &Path)
    -> anyhow::Result<()>
{
    log::info!(target: "edgedb::server::upgrade",
        "Dumping instance {:?}", inst.name);
    let path = data_path(false)?.join(format!("{}.dump", inst.name));
    if path.exists() {
        log::info!(target: "edgedb::server::upgrade",
            "Removing old dump at {}", path.display());
        fs::remove_dir_all(&path)?;
    }
    let mut conn_params = client::Builder::new();
    conn_params.user("edgedb");
    conn_params.database("edgedb");
    conn_params.unix_addr(socket);
    conn_params.wait_until_available(Duration::from_secs(30));
    let mut cli = conn_params.connect().await?;
    let options = commands::Options {
        command_line: true,
        styler: None,
        conn_params,
    };
    commands::dump_all(&mut cli, &options, path.as_ref()).await?;
    Ok(())
}

async fn restore_instance(inst: &Instance, socket: &Path)
    -> anyhow::Result<()>
{
    use crate::commands::parser::Restore;

    log::info!(target: "edgedb::server::upgrade",
        "Restoring instance {:?}", inst.name);
    let path = inst.data_dir.with_file_name(format!("{}.dump", inst.name));
    let mut conn_params = client::Builder::new();
    conn_params.user("edgedb");
    conn_params.database("edgedb");
    conn_params.unix_addr(socket);
    conn_params.wait_until_available(Duration::from_secs(30));
    let mut cli = conn_params.connect().await?;
    let options = commands::Options {
        command_line: true,
        styler: None,
        conn_params,
    };
    commands::restore_all(&mut cli, &options, &Restore {
        path,
        all: true,
        allow_non_empty: false,
        verbose: false,
    }).await?;
    Ok(())
}

fn do_nightly_upgrade(method: &dyn Method,
    mut instances: Vec<Instance>, options: &Upgrade)
    -> anyhow::Result<()>
{
    let instances_str = instances
        .iter().map(|inst| &inst.name[..]).collect::<Vec<_>>().join(", ");

    let version_query = VersionQuery::Nightly;
    let new = method.get_version(&version_query)
        .context("Unable to determine version")?;
    let old = get_installed(&version_query, method)?;

    if !options.force {
        if let Some(old_ver) = &old {
            if old_ver >= &new.full_version() {
                log::info!(target: "edgedb::server::upgrade",
                    "Nightly is up to date {}, skipping instances: {}",
                    old_ver, instances_str);
                return Ok(());
            }
        }
    }
    for inst in &mut instances {
        inst.source = old.clone();
        inst.version = Some(new.full_version());
    }

    for inst in &instances {
        dump_and_stop(inst)?;
    }

    log::info!(target: "edgedb::server::upgrade", "Upgrading the package");
    method.install(&install::Settings {
        method: method.name(),
        package_name: new.package_name,
        major_version: new.major_version.clone(),
        version: new.version,
        nightly: true,
        extra: LinkedHashMap::new(),
    })?;

    for inst in instances {
        reinit_and_restore(&inst, &new.major_version, true, method)?;
    }
    Ok(())
}

#[context("failed to dump {:?}", inst.name)]
fn dump_and_stop(inst: &Instance) -> anyhow::Result<()> {
    let mut ctl = inst.get_control()?;
    // in case not started for now
    log::info!(target: "edgedb::server::upgrade",
        "Ensuring instance is started");
    ctl.start(&options::Start { name: inst.name.clone(), foreground: false })?;
    task::block_on(dump_instance(inst, &ctl.get_socket(true)?))?;
    log::info!(target: "edgedb::server::upgrade",
        "Stopping the instance before package upgrade");
    ctl.stop(&options::Stop { name: inst.name.clone() })?;
    Ok(())
}

#[context("failed to restore {:?}", inst.name)]
fn reinit_and_restore(inst: &Instance,
    version: &Version<String>, nightly: bool,
    method: &dyn Method)
    -> anyhow::Result<()>
{
    let base = inst.data_dir.parent().unwrap();
    let backup = base.join(&format!("{}.backup", &inst.name));
    fs::rename(&inst.data_dir, &backup)?;
    write_backup_meta(&backup.join("backup.json"), &BackupMeta {
        timestamp: SystemTime::now(),
    })?;

    let meta = inst.upgrade_meta();
    init(&options::Init {
        name: inst.name.clone(),
        system: inst.system,
        interactive: false,
        nightly,
        version: Some(version.clone()),
        method: Some(method.name()),
        port: Some(inst.meta.port),
        start_conf: inst.meta.start_conf,
        inhibit_user_creation: true,
        inhibit_start: true,
        upgrade_marker: Some(serde_json::to_string(&meta).unwrap()),
        overwrite: true,
        default_user: "edgedb".into(),
        default_database: "edgedb".into(),
    })?;

    let mut ctl = inst.get_control()?;
    let mut cmd = ctl.run_command()?;
    // temporarily patch the edgedb issue of 1-alpha.4
    cmd.arg("--default-database=edgedb");
    cmd.arg("--default-database-user=edgedb");
    log::debug!("Running server: {:?}", cmd);
    let child = ProcessGuard::run(&mut cmd)
        .with_context(|| format!("error running server {:?}", cmd))?;

    task::block_on(restore_instance(inst, &ctl.get_socket(true)?))?;
    log::info!(target: "edgedb::server::upgrade",
        "Restarting instance {:?} to apply changes from `restore --all`",
        &inst.name);
    drop(child);

    ctl.start(&options::Start { name: inst.name.clone(), foreground: false })?;
    Ok(())
}

fn get_installed(version: &VersionQuery, method: &dyn Method)
    -> anyhow::Result<Option<Version<String>>>
{
    for ver in method.installed_versions()? {
        if !version.installed_matches(ver) {
            continue
        }
        return Ok(Some(ver.full_version()));
    }
    return Ok(None);
}

fn do_instance_upgrade(method: &dyn Method,
    mut inst: Instance, version: &VersionQuery, options: &Upgrade)
    -> anyhow::Result<()>
{
    let new = method.get_version(&version)
        .context("Unable to determine version")?;
    let old = get_installed(version, method)?;

    if !options.force {
        if let Some(old_ver) = &old {
            if old_ver >= &new.full_version() {
                log::info!(target: "edgedb::server::upgrade",
                    "Version {} is up to date {}, skipping instance: {}",
                    version, old_ver, inst.name);
                return Ok(());
            }
        }
    }
    inst.source = old;
    inst.version = Some(new.full_version());

    dump_and_stop(&inst)?;

    log::info!(target: "edgedb::server::upgrade", "Installing the package");
    method.install(&install::Settings {
        method: method.name(),
        package_name: new.package_name,
        major_version: new.major_version,
        version: new.version.clone(),
        nightly: version.is_nightly(),
        extra: LinkedHashMap::new(),
    })?;

    reinit_and_restore(&inst, &new.version, version.is_nightly(), method)?;
    Ok(())
}

#[context("failed to write backup metadata file {}", path.display())]
fn write_backup_meta(path: &Path, metadata: &BackupMeta)
    -> anyhow::Result<()>
{
    fs::write(path, serde_json::to_vec(&metadata)?)?;
    Ok(())
}

impl Instance {
    fn get_control(&self) -> anyhow::Result<Box<dyn control::Instance>> {
        control::get_instance_from_metadata(
            &self.name, self.system, &self.meta)
    }
    fn upgrade_meta(&self) -> UpgradeMeta {
        UpgradeMeta {
            source: self.source.clone().unwrap_or(Version("unknown".into())),
            target: self.version.clone().unwrap_or(Version("unknown".into())),
            started: SystemTime::now(),
            pid: process::id(),
        }
    }
}
