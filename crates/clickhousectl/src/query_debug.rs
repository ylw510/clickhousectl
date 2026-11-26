//! `clickhousectl debug`: run one query under lldb.
//!
//! With no `--break`, a launched process stops at entry and an attached
//! server stays stopped where lldb attached. Each `--break` is installed and
//! the process runs until one of them hits.
//!
//! The process being debugged is `clickhouse local`, which executes the SQL.
//! On Linux the matching `clickhouse-common-static-dbg` package is downloaded
//! when it exists and stored as `clickhouse.debug` next to the binary so lldb
//! can load it through the GNU debug link.

use crate::cli::{Breakpoint, DebugArgs};
use crate::error::{Error, NetworkCategory, Result};
use crate::local::discovery;
use crate::local::server::{self, Engine};
use crate::paths;
use crate::version_manager::VersionSpec;
use crate::version_manager::download;
use crate::version_manager::install;
use crate::version_manager::list::{Channel, compare_versions};
use crate::version_manager::platform::Platform;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::Archive;

const REPEATED_QUERY_MIN_VERSION: &str = "23.9.1.1854";

pub async fn run(args: DebugArgs) -> Result<()> {
    ensure_lldb()?;
    if let Some(name) = args.name {
        return attach_to_server(&name, &args.breakpoint, &args.query, &args.queries_file).await;
    }
    let platform = Platform::detect()?;
    let spec = args.version.unwrap_or(VersionSpec::Latest);
    let version = install::ensure_installed_local_first(&spec, &platform, false).await?;
    ensure_repeated_query_supported(&version, args.query.len())?;
    let binary = paths::binary_path(&version)?;
    if !binary.is_file() {
        return Err(Error::VersionNotFound(version));
    }

    if let Some(arch) = platform.packages_arch() {
        ensure_debug_symbols(&spec, &version, arch, &binary).await?;
    } else {
        eprintln!(
            "No debug symbol package is published for this platform; lldb uses the binary's own symbols"
        );
    }

    if args.breakpoint.is_empty() {
        eprintln!("Debugging ClickHouse {version} under lldb; stopped at process entry");
    } else {
        eprintln!(
            "Debugging ClickHouse {version} under lldb; stopping at {}",
            describe_breakpoints(&args.breakpoint)
        );
    }
    let local_args = clickhouse_local_args(&args.query, &args.queries_file, &args.args);
    exec_lldb(&lldb_arguments(&binary, &args.breakpoint, &local_args))
}

async fn attach_to_server(
    name: &str,
    breakpoints: &[Breakpoint],
    query: &[String],
    queries_file: &[String],
) -> Result<()> {
    let metadata_lock = server::lock_recovered_engine_metadata(Some(Engine::Clickhouse))?;
    let missing = || Error::ServerNotFound(name.to_string());
    let metadata_lock = metadata_lock.ok_or_else(missing)?;
    let entry = server::server_entry_locked(name, &metadata_lock)?.ok_or_else(missing)?;
    drop(metadata_lock);
    if !entry.running || entry.info.as_ref().is_none_or(|info| info.pid == 0) {
        return Err(Error::ServerNotRunning(name.to_string()));
    }
    let info = entry
        .info
        .ok_or_else(|| Error::ServerNotRunning(name.to_string()))?;
    if info.engine != Engine::Clickhouse {
        return Err(Error::ServerNotFound(name.to_string()));
    }

    let platform = Platform::detect()?;
    if let (Some(arch), Ok(VersionSpec::Exact(version))) = (
        platform.packages_arch(),
        crate::version_manager::parse_version_spec(&info.version),
    ) {
        let binary = paths::binary_path(&version)?;
        if binary.is_file() {
            ensure_debug_symbols(
                &VersionSpec::Exact(version.clone()),
                &version,
                arch,
                &binary,
            )
            .await?;
        }
    }

    // Metadata records the watchdog. Queries run in the server it supervises.
    let pid = discovery::query_process_pid(info.pid).map_err(Error::Io)?;
    let query_shell = server_query_shell(name, pid, &info, query, queries_file)?;
    if breakpoints.is_empty() {
        eprintln!("Debugging ClickHouse server {name} (pid {pid}) under lldb; stopped at attach");
    } else {
        eprintln!(
            "Debugging ClickHouse server {name} (pid {pid}) under lldb; stopping at {}",
            describe_breakpoints(breakpoints)
        );
    }
    if query_shell.is_some() && breakpoints.is_empty() {
        eprintln!("The query waits until you continue");
    }
    exec_lldb(&lldb_attach_arguments(
        pid,
        breakpoints,
        query_shell.as_deref(),
    ))
}

fn server_query_shell(
    name: &str,
    pid: u32,
    info: &server::ServerInfo,
    query: &[String],
    queries_file: &[String],
) -> Result<Option<String>> {
    if query.is_empty() && queries_file.is_empty() {
        return Ok(None);
    }
    ensure_repeated_query_supported(&info.version, query.len())?;
    if info.tcp_port == 0 {
        return Err(Error::ServerNotRunning(name.to_string()));
    }
    let binary = paths::binary_path(&info.version)?;
    if !binary.is_file() {
        return Err(Error::VersionNotFound(info.version.clone()));
    }

    let directory = std::env::temp_dir();
    let mut files = Vec::new();
    if !query.is_empty() {
        let path = directory.join(format!("clickhousectl-debug-{name}-{pid}.sql"));
        std::fs::write(&path, query.join("\n")).map_err(Error::Io)?;
        files.push(path);
    }
    files.extend(queries_file.iter().map(PathBuf::from));
    let output = directory.join(format!("clickhousectl-debug-{name}-{pid}.out"));
    eprintln!("Query output: {}", output.display());
    Ok(Some(client_query_shell(
        &binary,
        info.tcp_port,
        &files,
        &output,
    )))
}

/// `platform shell` execs its arguments, so the query command has to go
/// through `/bin/sh -c` for the redirect and the background `&`.
fn lldb_platform_shell(script: &str) -> String {
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    format!("platform shell /bin/sh -c \"{escaped}\"")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Shell command that runs the query against the attached server in the
/// background, so lldb can continue without waiting for the client.
pub(crate) fn client_query_shell(
    binary: &Path,
    port: u16,
    query_files: &[PathBuf],
    output: &Path,
) -> String {
    let mut command = format!(
        "{} client --host 127.0.0.1 --port {port}",
        shell_quote(&binary.display().to_string())
    );
    for file in query_files {
        command.push_str(" --queries-file ");
        command.push_str(&shell_quote(&file.display().to_string()));
    }
    // The client must not inherit lldb's stdin: it would consume the commands
    // typed at the debugger.
    command.push_str(" < /dev/null > ");
    command.push_str(&shell_quote(&output.display().to_string()));
    command.push_str(" 2>&1 &");
    command
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

/// lldb argv for a server that is already running. With no breakpoint, attach
/// and stay stopped. Otherwise set each breakpoint and continue.
pub(crate) fn lldb_attach_arguments(
    pid: u32,
    breakpoints: &[Breakpoint],
    query_shell: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["-p".to_string(), pid.to_string()];
    for breakpoint in breakpoints {
        args.push("-o".to_string());
        args.push(breakpoint_command(breakpoint));
    }
    if let Some(query_shell) = query_shell {
        args.push("-o".to_string());
        args.push(lldb_platform_shell(query_shell));
    }
    if !breakpoints.is_empty() {
        args.extend(["-o".to_string(), "continue".to_string()]);
    }
    args
}

/// lldb argv. With no breakpoint, launch and stop at process entry. Otherwise
/// set each breakpoint and run.
pub(crate) fn lldb_arguments(
    binary: &Path,
    breakpoints: &[Breakpoint],
    local_args: &[String],
) -> Vec<String> {
    let mut args = Vec::new();
    if breakpoints.is_empty() {
        args.extend([
            "-o".to_string(),
            "process launch --stop-at-entry".to_string(),
        ]);
    } else {
        for breakpoint in breakpoints {
            args.push("-o".to_string());
            args.push(breakpoint_command(breakpoint));
        }
        args.extend(["-o".to_string(), "run".to_string()]);
    }
    args.extend([
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

fn exec_lldb(arguments: &[String]) -> Result<()> {
    let mut cmd = Command::new("lldb");
    cmd.args(arguments);
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
                "process launch --stop-at-entry",
                "--",
                "/home/user/.clickhouse/versions/26.10.11.12/clickhouse",
                "local",
                "--query",
                "SELECT 1",
                "--format",
                "TSV",
            ]
        );
        assert_eq!(lldb_attach_arguments(4242, &[], None), ["-p", "4242"]);
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
        assert_eq!(
            lldb_attach_arguments(4242, &chosen, None),
            [
                "-p",
                "4242",
                "-o",
                "breakpoint set --name \"DB::InterpreterSelectQuery::execute\"",
                "-o",
                "breakpoint set --file \"src/Interpreters/executeQuery.cpp\" --line 410",
                "-o",
                "continue",
            ]
        );
        let shell = client_query_shell(
            &binary,
            9000,
            &[PathBuf::from("/tmp/q.sql")],
            Path::new("/tmp/q.out"),
        );
        assert_eq!(
            lldb_attach_arguments(4242, &chosen, Some(&shell)),
            vec![
                "-p".to_string(),
                "4242".to_string(),
                "-o".to_string(),
                "breakpoint set --name \"DB::InterpreterSelectQuery::execute\"".to_string(),
                "-o".to_string(),
                "breakpoint set --file \"src/Interpreters/executeQuery.cpp\" --line 410"
                    .to_string(),
                "-o".to_string(),
                lldb_platform_shell(&shell),
                "-o".to_string(),
                "continue".to_string(),
            ]
        );
        assert!(shell.contains("client --host 127.0.0.1 --port 9000"));
        assert!(shell.contains("--queries-file '/tmp/q.sql'"));
        assert!(shell.ends_with("< /dev/null > '/tmp/q.out' 2>&1 &"));
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
