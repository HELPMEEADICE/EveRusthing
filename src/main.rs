use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use everusthing::efu;
#[cfg(windows)]
use everusthing::gui;
use everusthing::query::{Query, QueryOptions};
#[cfg(windows)]
use everusthing::service::{self, DEFAULT_PIPE_NAME};

const HELP: &str = r#"EveRusthing 0.1.0 - Everything 1.4.1.1032 compatible search

EveRusthing.exe <-filelist <filename> | -local> [-search <text>] [-options]

-?                    Show this help.
-case                 Enable case matching.
-filelist <filename>  Open the specified Everything file list.
-install-service      Install and start the EveRusthing service.
-local                Build and search the local NTFS index.
-matchpath            Enable full path matching.
-nocase               Disable case matching.
-nomatchpath          Disable full path matching.
-nodb                  Do not save to or load from EveRusthing.db.
-nowholeword          Disable whole word matching.
-reindex               Force rebuilding EveRusthing.db.
-s <text>             Set the search.
-search <text>        Set the search.
-service-pipe-name <name>  Connect to the specified service pipe.
-start-service        Start the EveRusthing service.
-stop-service         Stop the EveRusthing service.
-uninstall-service    Uninstall the EveRusthing service.
-wholeword            Enable whole word matching.
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceAction {
    Install,
    Start,
    Stop,
    Uninstall,
    Dispatch,
    Console,
    Ping,
}

fn main() -> ExitCode {
    match run(env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("EveRusthing: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut arguments = arguments.into_iter();
    let mut file_list: Option<PathBuf> = None;
    let mut local = false;
    let mut search = String::new();
    let mut options = QueryOptions::default();
    let mut service_action = None;
    let mut pipe_name = default_pipe_name();
    let mut service_once = false;
    let mut use_database = true;
    let mut force_reindex = false;

    while let Some(argument) = arguments.next() {
        match argument.to_ascii_lowercase().as_str() {
            "-?" | "-h" | "-help" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-filelist" => file_list = Some(required_value(&mut arguments, "-filelist")?.into()),
            "-install-service" => set_service_action(&mut service_action, ServiceAction::Install)?,
            "-l" | "-local" => local = true,
            "-s" | "-search" => search = required_value(&mut arguments, &argument)?,
            "-case" => options.match_case = true,
            "-nocase" => options.match_case = false,
            "-matchpath" => options.match_path = true,
            "-nomatchpath" => options.match_path = false,
            "-nodb" => use_database = false,
            "-wholeword" | "-ww" => options.match_whole_word = true,
            "-nowholeword" | "-noww" => options.match_whole_word = false,
            "-reindex" => force_reindex = true,
            "-service-pipe-name" | "-svc-pipe-name" => {
                pipe_name = required_value(&mut arguments, &argument)?
            }
            "-start-service" => set_service_action(&mut service_action, ServiceAction::Start)?,
            "-stop-service" => set_service_action(&mut service_action, ServiceAction::Stop)?,
            "-uninstall-service" => {
                set_service_action(&mut service_action, ServiceAction::Uninstall)?
            }
            "-svc" => set_service_action(&mut service_action, ServiceAction::Dispatch)?,
            "-svc-console" => set_service_action(&mut service_action, ServiceAction::Console)?,
            "-svc-once" => service_once = true,
            "-service-ping" => set_service_action(&mut service_action, ServiceAction::Ping)?,
            _ if !argument.starts_with('-') && file_list.is_none() => {
                file_list = Some(argument.into())
            }
            _ => return Err(format!("unknown command line switch {argument}")),
        }
    }

    if let Some(action) = service_action {
        return run_service_action(action, &pipe_name, service_once);
    }

    if file_list.is_some() && local {
        return Err("-filelist and -local cannot be used together".into());
    }
    let records = if let Some(file_list) = file_list {
        efu::read_file(&file_list).map_err(|error| error.to_string())?
    } else if local {
        load_local_index(&pipe_name, use_database, force_reindex)?
    } else {
        #[cfg(windows)]
        return gui::run(&pipe_name, &search, options, use_database, force_reindex);
        #[cfg(not(windows))]
        return Err("no index specified; use -filelist <filename> or -local".into());
    };
    let query = Query::parse(&search, options).map_err(|error| error.to_string())?;
    for record in query.filter(&records) {
        println!("{}", record.path);
    }
    Ok(())
}

#[cfg(windows)]
fn load_local_index(
    pipe_name: &str,
    use_database: bool,
    force_reindex: bool,
) -> Result<Vec<everusthing::FileRecord>, String> {
    let database_path = everusthing::database::default_path();
    everusthing::database::load_local(&database_path, pipe_name, use_database, force_reindex)
}

#[cfg(not(windows))]
fn load_local_index(
    _pipe_name: &str,
    _use_database: bool,
    _force_reindex: bool,
) -> Result<Vec<everusthing::FileRecord>, String> {
    Err("local NTFS indexing is only available on Windows".into())
}

#[cfg(windows)]
fn run_service_action(
    action: ServiceAction,
    pipe_name: &str,
    service_once: bool,
) -> Result<(), String> {
    let result = match action {
        ServiceAction::Install => {
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            service::install(&executable, pipe_name)
        }
        ServiceAction::Start => service::start(),
        ServiceAction::Stop => service::stop(),
        ServiceAction::Uninstall => service::uninstall(),
        ServiceAction::Dispatch => service::run_service_dispatcher(pipe_name.to_owned()),
        ServiceAction::Console => service::run_console_server(pipe_name, service_once),
        ServiceAction::Ping => service::ping(pipe_name),
    };
    result.map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn run_service_action(
    _action: ServiceAction,
    _pipe_name: &str,
    _service_once: bool,
) -> Result<(), String> {
    Err("the EveRusthing service is only available on Windows".into())
}

#[cfg(windows)]
fn default_pipe_name() -> String {
    DEFAULT_PIPE_NAME.into()
}

#[cfg(not(windows))]
fn default_pipe_name() -> String {
    "EveRusthing Service".into()
}

fn set_service_action(
    current: &mut Option<ServiceAction>,
    action: ServiceAction,
) -> Result<(), String> {
    if current.replace(action).is_some() {
        Err("only one service command can be used at a time".into())
    } else {
        Ok(())
    }
}

fn required_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("missing value after {option}"))
}
