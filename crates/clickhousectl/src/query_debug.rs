//! `clickhousectl debug`: run one query under lldb and stop at query execution.
//!
//! The process being debugged is `clickhouse local`, which executes the SQL.
//! On Linux the matching `clickhouse-common-static-dbg` package is downloaded
//! when it exists and stored as `clickhouse.debug` next to the binary so lldb
//! can load it through the GNU debug link.

use crate::cli::{Breakpoint, DebugArgs};
use crate::error::{Error, NetworkCategory, Result};
use crate::paths;
use crate::version_manager::VersionSpec;
use crate::version_manager::download;
use crate::version_manager::install;
use crate::version_manager::list::{Channel, compare_versions};
use crate::version_manager::platform::Platform;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use tar::Archive;

const REPEATED_QUERY_MIN_VERSION: &str = "23.9.1.1854";
const QUERY_ENTRY_SYMBOL: &str = "DB::executeQuery";

pub async fn run(args: DebugArgs) -> Result<()> {
    ensure_lldb()?;
    let platform = Platform::detect()?;
    let version = install::ensure_installed_local_first(&args.version, &platform, false).await?;
    ensure_repeated_query_supported(&version, args.query.len())?;
    let binary = paths::binary_path(&version)?;
    if !binary.is_file() {
        return Err(Error::VersionNotFound(version));
    }

    if let Some(arch) = platform.packages_arch() {
        ensure_debug_symbols(&args.version, &version, arch, &binary).await?;
    } else {
        eprintln!(
            "No debug symbol package is published for this platform; lldb uses the binary's own symbols"
        );
    }

    let breakpoints = effective_breakpoints(&args.breakpoint);
    eprintln!(
        "Debugging ClickHouse {version} under lldb; stopping at {}",
        describe_breakpoints(&breakpoints)
    );
    let local_args = clickhouse_local_args(&args.query, &args.queries_file, &args.args);
    exec_lldb(&binary, &breakpoints, &local_args)
}

/// Package channels to try, preferred first. `stable` and anything that is
/// not explicitly `lts` look at the stable packages first.
pub(crate) fn symbol_package_channels(spec: &VersionSpec) -> [&'static str; 2] {
    match spec {
        VersionSpec::Channel(Channel::Lts) => ["lts", "stable"],
        _ => ["stable", "lts"],
    }
}

pub(crate) fn debug_symbol_url(channel: &str, version: &str, arch: &str) -> String {
    format!(
        "https://packages.clickhouse.com/tgz/{channel}/clickhouse-common-static-dbg-{version}-{arch}.tgz"
    )
}

/// Arguments passed to `clickhouse local`.
pub(crate) fn clickhouse_local_args(
    query: &[String],
    queries_file: &[String],
    extra: &[String],
) -> Vec<String> {
    let mut args = Vec::new();
    for statement in query {
        args.extend(["--query".to_string(), statement.clone()]);
    }
    for file in queries_file {
        args.extend(["--queries-file".to_string(), file.clone()]);
    }
    args.extend(extra.iter().cloned());
    args
}

/// Breakpoints to install. An empty list keeps the query-entry default.
pub(crate) fn effective_breakpoints(breakpoints: &[Breakpoint]) -> Vec<Breakpoint> {
    if breakpoints.is_empty() {
        vec![Breakpoint::Function(QUERY_ENTRY_SYMBOL.to_string())]
    } else {
        breakpoints.to_vec()
    }
}

fn describe_breakpoints(breakpoints: &[Breakpoint]) -> String {
    breakpoints
        .iter()
        .map(|breakpoint| match breakpoint {
            Breakpoint::Function(name) => name.clone(),
            Breakpoint::FileLine { file, line } => format!("{file}:{line}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn lldb_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn breakpoint_command(breakpoint: &Breakpoint) -> String {
    match breakpoint {
        Breakpoint::Function(name) => format!("breakpoint set --name {}", lldb_quote(name)),
        Breakpoint::FileLine { file, line } => {
            format!("breakpoint set --file {} --line {line}", lldb_quote(file))
        }
    }
}

/// lldb argv: create the target, set each breakpoint, then run.
pub(crate) fn lldb_arguments(
    binary: &Path,
    breakpoints: &[Breakpoint],
    local_args: &[String],
) -> Vec<String> {
    let mut args = Vec::new();
    for breakpoint in effective_breakpoints(breakpoints) {
        args.push("-o".to_string());
        args.push(breakpoint_command(&breakpoint));
    }
    args.extend([
        "-o".to_string(),
        "run".to_string(),
        "--".to_string(),
        binary.display().to_string(),
        "local".to_string(),
    ]);
    args.extend(local_args.iter().cloned());
    args
}

async fn ensure_debug_symbols(
    spec: &VersionSpec,
    version: &str,
    arch: &str,
    binary: &Path,
) -> Result<()> {
    let symbol = binary.with_file_name("clickhouse.debug");
    if symbol.is_file() && symbol.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        eprintln!("Using installed debug symbols for ClickHouse {version}");
        return Ok(());
    }

    let urls: Vec<String> = symbol_package_channels(spec)
        .into_iter()
        .map(|channel| debug_symbol_url(channel, version, arch))
        .collect();
    let download_path = binary.with_file_name("clickhouse-common-static-dbg.tgz");
    eprintln!("Downloading debug symbols for ClickHouse {version}");
    let found = download_first_available(&urls, &download_path).await?;
    if !found {
        let _ = std::fs::remove_file(&download_path);
        eprintln!(
            "No debug symbol package for ClickHouse {version}; lldb uses the binary's own symbols"
        );
        return Ok(());
    }

    let extracted = extract_clickhouse_debug(&download_path, &symbol);
    let _ = std::fs::remove_file(&download_path);
    extracted?;
    eprintln!("Installed debug symbols for ClickHouse {version}");
    Ok(())
}

async fn download_first_available(urls: &[String], dest: &Path) -> Result<bool> {
    for url in urls {
        match download::download_url(url, dest, false).await {
            Ok(_) => return Ok(true),
            Err(Error::Network(failure)) if failure.category == NetworkCategory::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn extract_clickhouse_debug(tarball: &Path, dest: &Path) -> Result<()> {
    let partial = dest.with_file_name(".clickhouse.debug.extracting");
    let extraction_error = |source| Error::ExtractArchive {
        archive: tarball.to_path_buf(),
        destination: partial.clone(),
        source,
    };
    let archive_file = File::open(tarball).map_err(extraction_error)?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = Archive::new(decoder);

    let extraction_result = (|| -> Result<()> {
        let entries = archive.entries().map_err(extraction_error)?;
        let mut found = false;
        for entry in entries {
            let mut entry = entry.map_err(extraction_error)?;
            let entry_path = entry.path().map_err(extraction_error)?.into_owned();
            let is_symbol = entry_path.file_name().and_then(|name| name.to_str())
                == Some("clickhouse.debug")
                && entry.header().entry_type().is_file();
            if !is_symbol || found {
                continue;
            }
            let mut output = File::create(&partial).map_err(extraction_error)?;
            io::copy(&mut entry, &mut output)
                .and_then(|_| output.flush())
                .map_err(extraction_error)?;
            found = true;
        }
        if !found {
            return Err(Error::Extract(format!(
                "Archive '{}' does not contain clickhouse.debug",
                tarball.display()
            )));
        }
        let mut decoder = archive.into_inner();
        io::copy(&mut decoder, &mut io::sink()).map_err(extraction_error)?;
        Ok(())
    })();

    if let Err(error) = extraction_result {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    if let Err(source) = std::fs::rename(&partial, dest) {
        let _ = std::fs::remove_file(&partial);
        return Err(extraction_error(source));
    }
    Ok(())
}

fn ensure_repeated_query_supported(version: &str, query_count: usize) -> Result<()> {
    if query_count > 1
        && compare_versions(version, REPEATED_QUERY_MIN_VERSION) == std::cmp::Ordering::Less
    {
        return Err(Error::RepeatedClientQueryUnsupported {
            version: version.to_string(),
            minimum: REPEATED_QUERY_MIN_VERSION,
        });
    }
    Ok(())
}

fn ensure_lldb() -> Result<()> {
    match Command::new("lldb").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(Error::Exec(format!(
            "lldb --version exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::Exec(
            "lldb was not found on PATH. Install lldb and retry (macOS: Xcode command line tools; Linux: the lldb package)."
                .to_string(),
        )),
        Err(error) => Err(Error::Exec(format!("Failed to run lldb: {error}"))),
    }
}

fn exec_lldb(binary: &Path, breakpoints: &[Breakpoint], local_args: &[String]) -> Result<()> {
    let mut cmd = Command::new("lldb");
    cmd.args(lldb_arguments(binary, breakpoints, local_args));
    #[cfg(feature = "telemetry")]
    crate::telemetry::finalize_before_exec();
    #[cfg(feature = "debug-log")]
    crate::debug_log::finish_before_exec();
    let error = std::os::unix::process::CommandExt::exec(&mut cmd);
    Err(Error::Exec(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::path::PathBuf;
    use tar::{Builder, Header};

    #[test]
    fn symbol_urls_prefer_the_requested_channel_and_name_the_exact_build() {
        assert_eq!(
            symbol_package_channels(&VersionSpec::Channel(Channel::Stable)),
            ["stable", "lts"]
        );
        assert_eq!(
            symbol_package_channels(&VersionSpec::Channel(Channel::Lts)),
            ["lts", "stable"]
        );
        assert_eq!(
            symbol_package_channels(&VersionSpec::Latest),
            ["stable", "lts"]
        );
        assert_eq!(
            debug_symbol_url("stable", "26.10.11.12", "amd64"),
            "https://packages.clickhouse.com/tgz/stable/clickhouse-common-static-dbg-26.10.11.12-amd64.tgz"
        );
    }

    #[test]
    fn lldb_stops_at_query_execution_and_forwards_sql() {
        let binary = PathBuf::from("/home/user/.clickhouse/versions/26.10.11.12/clickhouse");
        let local_args = clickhouse_local_args(
            &["SELECT 1".to_string()],
            &[],
            &["--format".to_string(), "TSV".to_string()],
        );
        assert_eq!(
            lldb_arguments(&binary, &[], &local_args),
            [
                "-o",
                "breakpoint set --name \"DB::executeQuery\"",
                "-o",
                "run",
                "--",
                "/home/user/.clickhouse/versions/26.10.11.12/clickhouse",
                "local",
                "--query",
                "SELECT 1",
                "--format",
                "TSV",
            ]
        );
        let chosen = [
            Breakpoint::Function("DB::InterpreterSelectQuery::execute".to_string()),
            Breakpoint::FileLine {
                file: "src/Interpreters/executeQuery.cpp".to_string(),
                line: 410,
            },
        ];
        assert_eq!(
            lldb_arguments(&binary, &chosen, &[]),
            [
                "-o",
                "breakpoint set --name \"DB::InterpreterSelectQuery::execute\"",
                "-o",
                "breakpoint set --file \"src/Interpreters/executeQuery.cpp\" --line 410",
                "-o",
                "run",
                "--",
                "/home/user/.clickhouse/versions/26.10.11.12/clickhouse",
                "local",
            ]
        );
    }

    #[test]
    fn extract_clickhouse_debug_keeps_the_symbol_file_from_a_package_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tarball = dir.path().join("symbols.tgz");
        write_debug_tarball(&tarball, b"dwarf-bytes");
        let dest = dir.path().join("clickhouse.debug");
        extract_clickhouse_debug(&tarball, &dest).expect("extract");
        assert_eq!(std::fs::read(&dest).expect("read"), b"dwarf-bytes");
        assert!(!dir.path().join(".clickhouse.debug.extracting").exists());
    }

    fn write_debug_tarball(path: &Path, contents: &[u8]) {
        let file = File::create(path).expect("create archive");
        let encoder = GzEncoder::new(file, Compression::fast());
        let mut builder = Builder::new(encoder);
        let mut header = Header::new_gnu();
        header
            .set_path("clickhouse-common-static-dbg/usr/lib/debug/usr/bin/clickhouse.debug")
            .expect("path");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        builder.append(&header, contents).expect("append");
        let encoder = builder.into_inner().expect("archive");
        encoder.finish().expect("gzip");
    }
}
