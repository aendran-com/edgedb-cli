use std::process::exit;

use crate::server::options::Install;
use crate::server::detect::{self, VersionQuery};
use crate::server::methods::InstallMethod;

pub mod operation;
pub mod exit_codes;
pub mod settings;


pub(in crate::server) use operation::{Operation, Command};
pub(in crate::server) use settings::{Settings, SettingsBuilder};

pub const KEY_FILE_URL: &str = "https://packages.edgedb.com/keys/edgedb.asc";



pub fn install(options: &Install) -> Result<(), anyhow::Error> {
    let current_os = detect::current_os()?;
    let avail_methods = current_os.get_available_methods()?;
    if options.method.is_none() && !options.interactive &&
        !avail_methods.package.supported
    {
        anyhow::bail!(avail_methods.format_error());
    }
    let methods = avail_methods.instantiate_all(&*current_os, false)?;
    let effective_method = options.method.clone()
        .unwrap_or(InstallMethod::Package);
    let version = VersionQuery::new(options.nightly, options.version.as_ref());
    for (meth_kind, meth) in &methods {
        for old_ver in meth.installed_versions()? {
            if version.installed_matches(&old_ver) {
                if &effective_method == meth_kind {
                    eprintln!("EdgeDB {} ({}-{}) is already installed. \
                        Use `edgedb server upgrade` for upgrade.",
                        old_ver.major_version,
                        old_ver.version, old_ver.revision);
                } else {
                    eprintln!("EdgeDB {} is already installed via {}. \
                        Please deinstall before installing via {}.",
                        old_ver.major_version, meth_kind.option(),
                        effective_method.option());
                }
                exit(exit_codes::ALREADY_INSTALLED);
            }
        }
    }
    let mut settings_builder = SettingsBuilder::new(
        &*current_os, options, methods)?;
    settings_builder.auto_version()?;
    let (settings, method) = settings_builder.build()?;
    settings.print();
    method.install(&settings)?;
    println!("\nEdgedb server is installed now. Great!\n\
        Initialize and start a new database instance with:\n  \
          edgedb server init{arg}",
          arg=if options.nightly { " --nightly" } else { "" });
    Ok(())
}
