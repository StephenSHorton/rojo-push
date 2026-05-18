use std::{
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use clap::Parser;
use memofs::{InMemoryFs, Vfs, VfsSnapshot};
use roblox_install::RobloxStudio;
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

use crate::serve_session::ServeSession;

static PLUGIN_BINCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugin.bincode"));
static PLUGIN_FILE_NAME: &str = "RojoManagedPlugin.rbxm";
static DISABLED_SUFFIX: &str = ".disabled";

/// Install Rojo's plugin.
#[derive(Debug, Parser)]
pub struct PluginCommand {
    #[clap(subcommand)]
    subcommand: PluginSubcommand,
}

/// Manages Rojo's Roblox Studio plugin.
#[derive(Debug, Parser)]
pub enum PluginSubcommand {
    /// Install the plugin in Roblox Studio's plugins folder. If the plugin is
    /// already installed, installing it again will overwrite the current plugin
    /// file.
    ///
    /// Scans for other Rojo plugin files in the plugins folder and warns about
    /// conflicts. Pass `--disable-conflicts` to rename conflicting plugin files
    /// to `<name>.disabled` automatically.
    Install {
        /// If other Rojo plugin files are detected in the plugins folder,
        /// rename them to `<name>.disabled` instead of just warning. Safe to
        /// undo: rename `.disabled` back to restore.
        #[clap(long)]
        disable_conflicts: bool,
    },

    /// Removes the plugin if it is installed.
    Uninstall,
}

impl PluginCommand {
    pub fn run(self) -> anyhow::Result<()> {
        self.subcommand.run()
    }
}

impl PluginSubcommand {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            PluginSubcommand::Install { disable_conflicts } => install_plugin(disable_conflicts),
            PluginSubcommand::Uninstall => uninstall_plugin(),
        }
    }
}

fn initialize_plugin() -> anyhow::Result<ServeSession> {
    let plugin_snapshot: VfsSnapshot = bincode::deserialize(PLUGIN_BINCODE)
        .expect("Rojo's plugin was not properly packed into Rojo's binary");

    let mut in_memory_fs = InMemoryFs::new();
    in_memory_fs.load_snapshot("/plugin", plugin_snapshot)?;

    let vfs = Vfs::new(in_memory_fs);
    Ok(ServeSession::new(vfs, "/plugin")?)
}

fn install_plugin(disable_conflicts: bool) -> anyhow::Result<()> {
    let studio = RobloxStudio::locate()?;

    let plugins_folder_path = studio.plugins_path();

    if !plugins_folder_path.exists() {
        log::debug!("Creating Roblox Studio plugins folder");
        fs::create_dir(plugins_folder_path)?;
    }

    let conflicts = find_conflicting_plugins(plugins_folder_path)?;
    if !conflicts.is_empty() {
        if disable_conflicts {
            disable_conflicting_plugins(&conflicts)?;
        } else {
            warn_conflicting_plugins(&conflicts)?;
        }
    }

    let plugin_path = plugins_folder_path.join(PLUGIN_FILE_NAME);
    log::debug!("Writing plugin to {}", plugin_path.display());

    let mut file = BufWriter::new(File::create(&plugin_path)?);

    let session = initialize_plugin()?;
    let tree = session.tree();
    let root_id = tree.get_root_id();

    rbx_binary::to_writer(&mut file, tree.inner(), &[root_id])?;

    println!("Installed plugin to {}", plugin_path.display());

    Ok(())
}

/// Scans the plugins folder for other plugin files whose name looks like a
/// Rojo plugin but isn't the one we install (`RojoManagedPlugin.rbxm`).
///
/// Matches anything starting with "Rojo" (case-insensitive) that ends in
/// `.rbxm` or `.rbxmx`. The standalone Marketplace plugin commonly lands as
/// `Rojo.rbxm` or `Rojo.<version>.rbxm`; either form is a real conflict
/// because Studio loads every `*.rbxm` in the folder and two Rojo plugins
/// will subscribe to the same serve session.
fn find_conflicting_plugins(plugins_folder: &Path) -> io::Result<Vec<PathBuf>> {
    let mut conflicts = Vec::new();
    let entries = match fs::read_dir(plugins_folder) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(conflicts),
        Err(err) => return Err(err),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if file_name == PLUGIN_FILE_NAME {
            continue;
        }

        let lower = file_name.to_lowercase();
        if !lower.starts_with("rojo") {
            continue;
        }
        if !(lower.ends_with(".rbxm") || lower.ends_with(".rbxmx")) {
            continue;
        }

        conflicts.push(path);
    }

    Ok(conflicts)
}

fn warn_conflicting_plugins(conflicts: &[PathBuf]) -> io::Result<()> {
    let writer = BufferWriter::stderr(ColorChoice::Auto);
    let mut buffer = writer.buffer();

    let mut yellow = ColorSpec::new();
    yellow.set_fg(Some(Color::Yellow)).set_bold(true);
    buffer.set_color(&yellow)?;
    writeln!(&mut buffer, "warning: other Rojo plugin files detected")?;
    buffer.set_color(&ColorSpec::new())?;

    for path in conflicts {
        writeln!(&mut buffer, "  - {}", path.display())?;
    }

    writeln!(&mut buffer)?;
    writeln!(
        &mut buffer,
        "Studio loads every *.rbxm in this folder. With two Rojo plugins active,"
    )?;
    writeln!(
        &mut buffer,
        "one of them will likely fail to talk to the serve session (commonly with a"
    )?;
    writeln!(
        &mut buffer,
        "\"Can't parse JSON\" error from older plugin versions calling the msgpack API)."
    )?;
    writeln!(&mut buffer)?;
    writeln!(&mut buffer, "Fixes:")?;
    writeln!(
        &mut buffer,
        "  * Re-run with `--disable-conflicts` to rename the files above to `.disabled`,"
    )?;
    writeln!(
        &mut buffer,
        "    or uninstall them via Plugins → Manage Plugins in Studio."
    )?;
    writeln!(
        &mut buffer,
        "  * After resolving, restart Studio so the change takes effect."
    )?;

    writer.print(&buffer)?;
    Ok(())
}

fn disable_conflicting_plugins(conflicts: &[PathBuf]) -> io::Result<()> {
    for path in conflicts {
        let new_path = append_disabled_suffix(path);
        log::debug!(
            "Renaming conflicting plugin {} -> {}",
            path.display(),
            new_path.display()
        );
        fs::rename(path, &new_path)?;
        println!(
            "Disabled conflicting plugin: {} -> {}",
            path.display(),
            new_path.display()
        );
    }
    println!("Restart Roblox Studio for the change to take effect.");
    Ok(())
}

fn append_disabled_suffix(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .expect("conflict path always has a file name")
        .to_owned();
    name.push(DISABLED_SUFFIX);
    path.with_file_name(name)
}

fn uninstall_plugin() -> anyhow::Result<()> {
    let studio = RobloxStudio::locate()?;

    let plugin_path = studio.plugins_path().join(PLUGIN_FILE_NAME);

    if plugin_path.exists() {
        log::debug!("Removing existing plugin from {}", plugin_path.display());
        fs::remove_file(plugin_path)?;
    } else {
        log::debug!("Plugin not installed at {}", plugin_path.display());
    }

    Ok(())
}

#[test]
fn plugin_initialize() {
    let _ = initialize_plugin().unwrap();
}
