//! Installing the remote as a service: what keeps it running after the terminal
//! closes and the machine comes back up.
//!
//! Guided rather than printed, because a service definition is a machine's own
//! facts — where this binary is, which directories it reads, which user owns the
//! sandboxes — and someone who can already write one is not who this is for. It
//! says what it will do before doing it, never installs in a scope that was not
//! chosen, and is the only place in the serve role that asks for the machine's
//! owner.

use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    iter,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    cli::ServeArgs,
    config::{Config, ServeSettings},
};

use super::console::{ask, choose, field, has_terminal, paragraph};

/// The systemd unit name.
const UNIT: &str = "altitude.service";
/// The launchd job label, which is reverse-DNS because launchd expects one.
const LABEL: &str = "dev.saillard.altitude.remote";

/// Who starts it, and whose permissions it runs with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// This user, and nothing beyond what that user can already do.
    User,
    /// Without anyone logged in, which is the machine owner's business.
    Machine,
}

impl Scope {
    fn privileged(self) -> bool {
        self == Self::Machine
    }
}

pub async fn install(args: &ServeArgs, base: &Config) -> Result<(), Box<dyn Error>> {
    if !has_terminal() {
        return Err(io::Error::other(
            "run this from a terminal: it shows what it will write and asks before it writes it",
        )
        .into());
    }
    let service = Service::now(args, base)?;
    service.warn_about_inherited_secrets()?;
    let scope = service.choose_scope()?;
    let plan = service.plan(scope)?;
    plan.show(&service);
    if !ask("Write it and start it? [Y/n]", true)? {
        println!("\nNothing was written.");
        return Ok(());
    }
    plan.write()?;
    plan.run(&service)?;
    plan.verify();
    Ok(())
}

pub async fn uninstall(_args: &ServeArgs, _base: &Config) -> Result<(), Box<dyn Error>> {
    if !has_terminal() {
        return Err(io::Error::other(
            "run this from a terminal: it shows what it will stop and remove, and asks first",
        )
        .into());
    }
    let installations = Installation::find();
    if installations.is_empty() {
        println!("\nNo Altitude service on this machine.\n");
        for scope in [Scope::User, Scope::Machine] {
            field("looked", Installation::path(scope).display());
        }
        return Ok(());
    }

    println!("\nAltitude remote · remove the service\n");
    for installation in &installations {
        field(
            "found",
            format!(
                "{} · {}",
                installation.path.display(),
                scope_name(installation.scope)
            ),
        );
    }
    let steps = installations
        .iter()
        .flat_map(|installation| installation.steps.iter().cloned())
        .collect::<Vec<_>>();
    let privileged = steps.iter().any(|step| step.privileged);
    println!();
    paragraph(
        "This stops the remote and deletes what starts it. Your tasks, logins and settings \
         stay where they are.",
    );
    if privileged {
        println!();
        paragraph("Removing the machine-wide service needs your permission once (sudo).");
    }
    if !ask("Remove it? [y/N]", false)? {
        println!("\nNothing was removed.");
        return Ok(());
    }
    run_steps(&steps)?;
    println!();
    for installation in &installations {
        field("removed", installation.path.display());
    }
    Ok(())
}

/// One service found on this machine, and what removing it takes.
struct Installation {
    path: PathBuf,
    scope: Scope,
    steps: Vec<Step>,
}

impl Installation {
    fn find() -> Vec<Self> {
        let uid = uid();
        [Scope::User, Scope::Machine]
            .into_iter()
            .map(|scope| (Self::path(scope), scope))
            .filter(|(path, _)| path.exists())
            .map(|(path, scope)| Self {
                steps: Self::steps(&path, scope, &uid),
                path,
                scope,
            })
            .collect()
    }

    fn path(scope: Scope) -> PathBuf {
        if matches!(env::consts::OS, "linux") {
            return match scope {
                Scope::User => config_home().join("systemd/user").join(UNIT),
                Scope::Machine => PathBuf::from("/etc/systemd/system").join(UNIT),
            };
        }
        let directory = match scope {
            Scope::User => home().join("Library/LaunchAgents"),
            Scope::Machine => PathBuf::from("/Library/LaunchDaemons"),
        };
        directory.join(format!("{LABEL}.plist"))
    }

    /// Stops it, then removes the file. Both are best-effort until the removal: a
    /// service that is already stopped still has a file to delete, and a stop that
    /// refuses does not make the file less of a leftover.
    fn steps(path: &Path, scope: Scope, uid: &str) -> Vec<Step> {
        let mut steps = vec![if matches!(env::consts::OS, "linux") {
            Step::systemctl(scope, &["disable", "--now", UNIT], false)
        } else {
            Step::launchctl(scope, &["bootout", &domain(scope, uid)], false)
        }];
        steps.push(Step::remove(path, scope));
        if matches!(env::consts::OS, "linux") {
            steps.push(Step::systemctl(scope, &["daemon-reload"], false));
        }
        steps
    }
}

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

/// The launchd domain a job belongs to: this user's session, or the machine's.
fn domain(scope: Scope, uid: &str) -> String {
    match scope {
        Scope::User => format!("gui/{uid}"),
        Scope::Machine => "system".to_owned(),
    }
}

/// Everything about this machine that the definition needs to carry.
struct Service {
    exe: PathBuf,
    /// `serve`, plus whatever this run was told, so the service starts the same way.
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    user: String,
    uid: String,
    home: PathBuf,
    settings_path: PathBuf,
}

impl Service {
    fn now(args: &ServeArgs, base: &Config) -> Result<Self, Box<dyn Error>> {
        let exe = env::current_exe()?.canonicalize()?;
        let home = home();
        if home.as_os_str().is_empty() {
            return Err(io::Error::other("this machine has no HOME to install for").into());
        }

        let settings_path = args
            .config
            .clone()
            .unwrap_or_else(|| base.serve_settings_path());
        let config_dir = settings_path
            .parent()
            .ok_or_else(|| io::Error::other("the settings file has no directory"))?
            .to_owned();

        let mut arguments = vec!["serve".to_owned()];
        // The same flags this run was given, so a service never starts differently
        // from the process the operator just watched work.
        if args.config.is_some() {
            arguments.push("--config".to_owned());
            arguments.push(settings_path.display().to_string());
        }
        if let Some(listen) = &args.listen {
            arguments.push("--listen".to_owned());
            arguments.push(listen.clone());
        }

        let mut environment = vec![(
            "ALTITUDE_DATA_DIR".to_owned(),
            base.data_dir().display().to_string(),
        )];
        if args.config.is_none() {
            environment.push((
                "ALTITUDE_CONFIG_DIR".to_owned(),
                config_dir.display().to_string(),
            ));
        }
        // The sandbox runtime is found on this process's PATH, wherever it was
        // installed; a service manager's own PATH would not find it.
        environment.push((
            "PATH".to_owned(),
            env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned()),
        ));

        Ok(Self {
            exe,
            arguments,
            environment,
            user: whoami::username().map_err(io::Error::other)?,
            uid: uid(),
            home,
            settings_path,
        })
    }

    /// A secret that only this shell's environment holds will not reach a service.
    ///
    /// Said before anything is written, because the service would otherwise start
    /// without a tracker and look like a failed install. Reading the settings here
    /// writes a default file when there is none, which is the file the service is
    /// about to start from anyway.
    fn warn_about_inherited_secrets(&self) -> Result<(), Box<dyn Error>> {
        let settings = ServeSettings::load_or_create(&self.settings_path)?;
        let Some(linear) = settings.linear else {
            return Ok(());
        };
        if linear.client_secret.is_some() && linear.webhook_secret.is_some() {
            return Ok(());
        }
        println!();
        paragraph(
            "Some of this machine's Linear secrets come from the environment it is running in \
             now. A service starts with its own environment and will not see them, so the remote \
             would come up without a task tracker.",
        );
        println!();
        paragraph(&format!(
            "Put them in the [linear] section of {} before it starts — that file is only readable \
             by this machine's user.",
            self.settings_path.display()
        ));
        Ok(())
    }

    fn choose_scope(&self) -> io::Result<Scope> {
        println!();
        match env::consts::OS {
            "macos" => {
                paragraph(
                    "A user service starts when you log in, and keeps the sandboxes and logins \
                     this machine already has. A machine service starts at boot with nobody \
                     logged in, runs as you, and needs the machine owner's permission once.",
                );
            }
            _ => {
                paragraph(
                    "A user service starts at boot and runs without a session, keeping the \
                     sandboxes and logins this machine already has. A machine service also \
                     starts at boot, but is managed by the machine owner.",
                );
            }
        }
        println!();
        println!("  1  for this user · no sudo");
        println!("  2  for the whole machine · needs sudo");
        match choose("Which of these? [1]", 2, 0)? {
            0 => Ok(Scope::User),
            _ => Ok(Scope::Machine),
        }
    }

    fn plan(&self, scope: Scope) -> Result<Plan, Box<dyn Error>> {
        let systemd = matches!(env::consts::OS, "linux");
        if !systemd && env::consts::OS != "macos" {
            return Err(io::Error::other(
                "Altitude installs a service on macOS and systemd Linux only; start it from your \
                 own init system on this platform",
            )
            .into());
        }
        if systemd && !has_systemd() {
            return Err(io::Error::other(
                "this machine has no systemd; start altitude from your own init system",
            )
            .into());
        }
        Ok(if systemd {
            self.systemd_plan(scope)
        } else {
            self.launchd_plan(scope)
        })
    }

    fn systemd_plan(&self, scope: Scope) -> Plan {
        let path = match scope {
            Scope::User => config_home().join("systemd/user").join(UNIT),
            Scope::Machine => PathBuf::from("/etc/systemd/system").join(UNIT),
        };
        Plan {
            path,
            contents: self.systemd_unit(scope),
            commands: vec![
                Step::systemctl(scope, &["daemon-reload"], true),
                Step::systemctl(scope, &["enable", "--now", UNIT], true),
            ],
            logs: match scope {
                Scope::User => format!("journalctl --user -u {UNIT} -f"),
                Scope::Machine => format!("journalctl -u {UNIT} -f"),
            },
            verify: Step::systemctl(scope, &["is-active", UNIT], false),
            scope,
        }
    }

    fn launchd_plan(&self, scope: Scope) -> Plan {
        let path = match scope {
            Scope::User => self.home.join("Library/LaunchAgents"),
            Scope::Machine => PathBuf::from("/Library/LaunchDaemons"),
        }
        .join(format!("{LABEL}.plist"));
        let domain = domain(scope, &self.uid);
        Plan {
            path: path.clone(),
            contents: self.launchd_job(scope),
            // A job left loaded beside a new file keeps running the old definition,
            // so it is unloaded first; finding nothing loaded is the normal first
            // install and does not stop anything.
            commands: vec![
                Step::launchctl(scope, &["bootout", &format!("{domain}/{LABEL}")], false),
                Step::launchctl(
                    scope,
                    &["bootstrap", &domain, &path.display().to_string()],
                    true,
                ),
            ],
            logs: format!("tail -f {}", self.log_path().display()),
            verify: Step::launchctl(scope, &["print", &format!("{domain}/{LABEL}")], false),
            scope,
        }
    }

    fn systemd_unit(&self, scope: Scope) -> String {
        let mut unit = String::from(
            "[Unit]\nDescription=Altitude remote\nAfter=network-online.target\n\n[Service]\n",
        );
        let executable = self
            .command_parts()
            .iter()
            .map(|argument| systemd_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        unit.push_str(&format!("ExecStart={executable}\n"));
        if scope == Scope::Machine {
            unit.push_str(&format!("User={}\n", self.user));
        }
        for (key, value) in self.environment_for(scope) {
            unit.push_str(&format!("Environment={key}={value}\n"));
        }
        unit.push_str(
            "Restart=on-failure\nRestartSec=5\n\
             # Suspending every task flushes its capsule to disk; this is the budget a stop gets.\n\
             TimeoutStopSec=120\n\n",
        );
        unit.push_str(match scope {
            Scope::User => "[Install]\nWantedBy=default.target\n",
            Scope::Machine => "[Install]\nWantedBy=multi-user.target\n",
        });
        unit
    }

    fn launchd_job(&self, scope: Scope) -> String {
        let mut job = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        element(&mut job, "Label", LABEL, 1);
        job.push_str("  <key>ProgramArguments</key>\n  <array>\n");
        for argument in self.command_parts() {
            job.push_str(&format!("    <string>{}</string>\n", xml(&argument)));
        }
        job.push_str("  </array>\n");
        job.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (key, value) in self.environment_for(scope) {
            job.push_str(&format!(
                "    <key>{}</key><string>{}</string>\n",
                xml(&key),
                xml(&value)
            ));
        }
        job.push_str("  </dict>\n");
        // Launchd captures no output of its own, and the serve role logs everything
        // to stdout: without these, a failure leaves nothing to read.
        let log = self.log_path();
        element(&mut job, "StandardOutPath", &log.display().to_string(), 1);
        element(
            &mut job,
            "StandardErrorPath",
            &log.with_extension("err").display().to_string(),
            1,
        );
        job.push_str(
            "  <key>RunAtLoad</key><true/>\n\
             \x20 <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
             \x20 <key>ThrottleInterval</key><integer>10</integer>\n",
        );
        if scope == Scope::Machine {
            element(&mut job, "UserName", &self.user, 1);
        }
        job.push_str("</dict>\n</plist>\n");
        job
    }

    /// What the service is told about this machine, with `HOME` added for a machine
    /// service, which starts without a login to take it from.
    fn environment_for(&self, scope: Scope) -> Vec<(String, String)> {
        let mut environment = self.environment.clone();
        if scope == Scope::Machine {
            environment.push(("HOME".to_owned(), self.home.display().to_string()));
        }
        environment
    }

    fn command_parts(&self) -> Vec<String> {
        iter::once(self.exe.display().to_string())
            .chain(self.arguments.iter().cloned())
            .collect()
    }

    fn command_line(&self) -> String {
        self.command_parts().join(" ")
    }

    fn log_path(&self) -> PathBuf {
        self.home.join("Library/Logs").join("altitude-remote.log")
    }
}

/// A definition that has been rendered but not yet installed.
struct Plan {
    path: PathBuf,
    contents: String,
    commands: Vec<Step>,
    logs: String,
    verify: Step,
    scope: Scope,
}

#[derive(Clone)]
struct Step {
    command: Vec<String>,
    privileged: bool,
    /// Whether a failure stops the install. Unloading a job that is not loaded is
    /// the normal first install, so that step is not.
    required: bool,
}

impl Step {
    fn systemctl(scope: Scope, arguments: &[&str], required: bool) -> Self {
        Self {
            command: systemctl(arguments, scope),
            privileged: scope.privileged(),
            required,
        }
    }

    fn launchctl(scope: Scope, arguments: &[&str], required: bool) -> Self {
        Self {
            command: launchctl(arguments),
            privileged: scope.privileged(),
            required,
        }
    }

    fn remove(path: &Path, scope: Scope) -> Self {
        Self {
            command: vec!["rm".to_owned(), "--".to_owned(), path.display().to_string()],
            privileged: scope.privileged(),
            required: true,
        }
    }
}

/// `systemctl` for whichever manager this scope belongs to.
fn systemctl(arguments: &[&str], scope: Scope) -> Vec<String> {
    let mut command = vec!["systemctl".to_owned()];
    if scope == Scope::User {
        command.push("--user".to_owned());
    }
    command.extend(arguments.iter().map(|argument| argument.to_string()));
    command
}

fn launchctl(arguments: &[&str]) -> Vec<String> {
    iter::once("launchctl".to_owned())
        .chain(arguments.iter().map(|argument| argument.to_string()))
        .collect()
}

impl Plan {
    fn show(&self, service: &Service) {
        println!("\nAltitude remote · as a service\n");
        field("file", self.path.display());
        field("scope", scope_name(self.scope));
        field("starts", starts(self.scope));
        field("runs", service.command_line());
        field("logs", &self.logs);

        let mut says = vec![
            "It restarts if it stops, and runs as you so it uses the sandboxes and logins this \
             machine already has",
        ];
        if self.scope.privileged() {
            says.push("Writing it and starting it needs your permission once (sudo)");
        }
        if self.scope == Scope::User && matches!(env::consts::OS, "linux") {
            says.push(
                "Lingering is turned on for your user as part of this, without which a user \
                 service stops with your last session",
            );
        }
        for said in says {
            println!();
            paragraph(said);
        }
        println!();
        let kind = if matches!(env::consts::OS, "linux") {
            "unit"
        } else {
            "launch job"
        };
        paragraph(&format!(
            "The file is an ordinary {kind} you can read or edit later; Altitude does not manage \
             it again after this."
        ));
    }

    fn write(&self) -> Result<(), Box<dyn Error>> {
        if let Some(directory) = self.path.parent()
            && !self.scope.privileged()
        {
            fs::create_dir_all(directory)?;
        }
        if self.scope.privileged() {
            let mut child = Command::new("sudo")
                .arg("tee")
                .arg(&self.path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()?;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("could not hand the definition to sudo"))?;
            stdin.write_all(self.contents.as_bytes())?;
            drop(stdin);
            if !child.wait()?.success() {
                return Err(
                    io::Error::other(format!("could not write {}", self.path.display())).into(),
                );
            }
        } else {
            fs::write(&self.path, &self.contents)?;
        }
        Ok(())
    }

    fn run(&self, service: &Service) -> Result<(), Box<dyn Error>> {
        if self.scope == Scope::User && matches!(env::consts::OS, "linux") {
            self.linger(service)?;
        }
        run_steps(&self.commands)
    }

    /// A user service on Linux stops with the last session unless it is lingering,
    /// which is exactly what a headless remote must not do.
    fn linger(&self, service: &Service) -> Result<(), Box<dyn Error>> {
        let command = vec![
            "loginctl".to_owned(),
            "enable-linger".to_owned(),
            service.user.clone(),
        ];
        if run_command(&command, false, true)? {
            return Ok(());
        }
        println!();
        paragraph(
            "Lingering could not be enabled here. The service will still start, and it will stop \
             when your last session ends unless you enable it yourself.",
        );
        Ok(())
    }

    /// What a manager calls a service it has started, in a word.
    fn running_word(&self) -> &'static str {
        if matches!(env::consts::OS, "linux") {
            "active"
        } else {
            "loaded"
        }
    }

    fn verify(&self) {
        println!("\nInstalled.");
        field("file", self.path.display());
        let running =
            run_command(&self.verify.command, self.verify.privileged, false).unwrap_or(false);
        field(
            "state",
            if running {
                self.running_word()
            } else {
                "not running yet"
            },
        );
        field("logs", &self.logs);
        println!();
        paragraph(
            "With a shared tracker app installed, the first issue delegated to it shows up in \
             these logs.",
        );
    }
}

fn run_steps(steps: &[Step]) -> Result<(), Box<dyn Error>> {
    for step in steps {
        if run_command(&step.command, step.privileged, step.required)? || !step.required {
            continue;
        }
        return Err(io::Error::other(format!("{} did not succeed", step.command.join(" "))).into());
    }
    Ok(())
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "this user",
        Scope::Machine => "the whole machine",
    }
}

fn starts(scope: Scope) -> &'static str {
    match (env::consts::OS, scope) {
        ("macos", Scope::User) => "when you log in",
        _ => "at boot",
    }
}

/// Run one command, and answer whether it succeeded.
///
/// A step speaks only when it has something to complain about, and only when that
/// complaint matters: unloading a job that was never loaded prints "no such
/// process", which reads like a failure in the middle of a first install that is
/// going perfectly well.
fn run_command(command: &[String], privileged: bool, show_errors: bool) -> io::Result<bool> {
    let mut process = process_for(command, privileged)?;
    process.stdout(Stdio::null());
    if !show_errors {
        process.stderr(Stdio::null());
    }
    Ok(process.status()?.success())
}

fn process_for(command: &[String], privileged: bool) -> io::Result<Command> {
    let mut process = if privileged {
        let mut process = Command::new("sudo");
        process.args(command);
        process
    } else {
        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| io::Error::other("no command to run"))?;
        let mut process = Command::new(program);
        process.args(arguments);
        process
    };
    process.stdin(Stdio::inherit());
    Ok(process)
}

fn has_systemd() -> bool {
    Command::new("systemctl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// systemd splits `ExecStart` on whitespace unless an argument is quoted.
fn systemd_quote(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/._-:=@+".contains(character));
    if plain {
        argument.to_owned()
    } else {
        format!(
            "\"{}\"",
            argument.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

fn element(job: &mut String, key: &str, value: &str, depth: usize) {
    let indent = "  ".repeat(depth);
    job.push_str(&format!(
        "{indent}<key>{}</key><string>{}</string>\n",
        xml(key),
        xml(value)
    ));
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
