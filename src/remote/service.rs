//! Installing the remote as a service: what keeps it running after the terminal
//! closes and the machine comes back up.
//!
//! Guided rather than printed, because a service definition is a machine's own
//! facts — where this binary is, which directories it reads, which user owns the
//! sandboxes — and someone who can already write one is not who this is for. It
//! shows everything before writing, never installs in a scope that was not chosen,
//! and is the only place in the serve role that asks for the machine's owner.

use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    iter,
    path::PathBuf,
    process::{Command, Stdio},
};

use crate::{
    cli::ServeArgs,
    config::{Config, ServeSettings},
};

use super::console::{ask, choose, field, has_terminal, prose};

/// The systemd unit name, and the launchd job label.
const UNIT: &str = "altitude.service";
const LABEL: &str = "dev.saillard.altitude.remote";

/// Who starts it, and whose permissions it runs with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// This user, and nothing beyond what that user can already do.
    User,
    /// Without anyone logged in, which is the machine owner's business.
    Machine,
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
    plan.run()?;
    plan.verify();
    Ok(())
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
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("this machine has no HOME to install for"))?;
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
            uid: uid()?,
            home,
            settings_path,
        })
    }

    /// A secret that only this shell's environment holds will not reach a service.
    ///
    /// Said before anything is written, because the service would otherwise start
    /// without a tracker and look like a failed install.
    fn warn_about_inherited_secrets(&self) -> Result<(), Box<dyn Error>> {
        let settings = ServeSettings::load_or_create(&self.settings_path)?;
        let Some(linear) = settings.linear else {
            return Ok(());
        };
        if linear.client_secret.is_some() && linear.webhook_secret.is_some() {
            return Ok(());
        }
        println!();
        prose(
            "Some of this machine's Linear secrets come from the environment it is running in \
             now. A service starts with its own environment and will not see them, so the \
             remote would come up without a task tracker.",
            "",
            "",
        );
        println!();
        prose(
            &format!(
                "Put them in the [linear] section of {} before it starts — that file is only \
                 readable by this machine's user.",
                self.settings_path.display()
            ),
            "",
            "",
        );
        Ok(())
    }

    fn choose_scope(&self) -> io::Result<Scope> {
        println!();
        match env::consts::OS {
            "macos" => {
                prose(
                    "A user service starts when you log in, and keeps the sandboxes and logins \
                     this machine already has. A machine service starts at boot with nobody \
                     logged in, runs as you, and needs the machine owner's permission once.",
                    "",
                    "",
                );
            }
            _ => {
                prose(
                    "A user service starts at boot and runs without a session, keeping the \
                     sandboxes and logins this machine already has. A machine service also \
                     starts at boot, but is managed by the machine owner.",
                    "",
                    "",
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
            Scope::User => self.config_home().join("systemd/user").join(UNIT),
            Scope::Machine => PathBuf::from("/etc/systemd/system").join(UNIT),
        };
        let privileged = scope == Scope::Machine;
        let commands = vec![
            Step::systemctl(scope, &["daemon-reload"], true),
            Step::systemctl(scope, &["enable", "--now", UNIT], true),
        ];
        Plan {
            path,
            contents: self.systemd_unit(scope),
            commands,
            privileged,
            logs: match scope {
                Scope::User => format!("journalctl --user -u {UNIT} -f"),
                Scope::Machine => format!("journalctl -u {UNIT} -f"),
            },
            verify: vec![Check::systemctl(scope, &["is-active", UNIT])],
            scope,
            user: self.user.clone(),
        }
    }

    fn launchd_plan(&self, scope: Scope) -> Plan {
        let domain = match scope {
            Scope::User => format!("gui/{}", self.uid),
            Scope::Machine => "system".to_owned(),
        };
        let path = match scope {
            Scope::User => self.home.join("Library/LaunchAgents"),
            Scope::Machine => PathBuf::from("/Library/LaunchDaemons"),
        }
        .join(format!("{LABEL}.plist"));
        let privileged = scope == Scope::Machine;
        // Leaving a job loaded beside a new file leaves launchd running the old
        // definition, so it is unloaded first — and not being loaded is the normal
        // first install, not an error.
        let commands = vec![
            Step::launchctl(scope, &["bootout", &format!("{domain}/{LABEL}")], false),
            Step::launchctl(
                scope,
                &["bootstrap", &domain, &path.display().to_string()],
                true,
            ),
        ];
        Plan {
            path: path.clone(),
            contents: self.launchd_job(scope),
            commands,
            privileged,
            logs: format!(
                "tail -f {}",
                self.home
                    .join("Library/Logs")
                    .join("altitude-remote.log")
                    .display()
            ),
            verify: vec![Check::launchctl(
                scope,
                &["print", &format!("{domain}/{LABEL}")],
            )],
            scope,
            user: self.user.clone(),
        }
    }

    fn systemd_unit(&self, scope: Scope) -> String {
        let mut unit = String::from(
            "[Unit]\nDescription=Altitude remote\nAfter=network-online.target\n\n[Service]\n",
        );
        let executable = iter::once(self.exe.display().to_string())
            .chain(self.arguments.iter().cloned())
            .map(|argument| systemd_quote(&argument))
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
        for argument in
            iter::once(self.exe.display().to_string()).chain(self.arguments.iter().cloned())
        {
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
        let logs = self.home.join("Library/Logs");
        element(
            &mut job,
            "StandardOutPath",
            &logs.join("altitude-remote.log").display().to_string(),
            1,
        );
        element(
            &mut job,
            "StandardErrorPath",
            &logs.join("altitude-remote.err").display().to_string(),
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

    fn config_home(&self) -> PathBuf {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| self.home.join(".config"))
    }
}

/// A definition that has been rendered but not yet installed.
struct Plan {
    path: PathBuf,
    contents: String,
    commands: Vec<Step>,
    privileged: bool,
    logs: String,
    verify: Vec<Check>,
    scope: Scope,
    user: String,
}

struct Check {
    command: Vec<String>,
    privileged: bool,
    /// What a successful answer means, in a word. The command's own output is not
    /// printed: `launchctl print` answers with its whole internal record.
    said: &'static str,
}

impl Check {
    fn systemctl(scope: Scope, arguments: &[&str]) -> Self {
        Self {
            command: systemctl(arguments, scope),
            privileged: scope == Scope::Machine,
            said: "active",
        }
    }

    /// `launchctl print` answers with the job's whole internal record, so only its
    /// exit status is used: loaded, or not.
    fn launchctl(scope: Scope, arguments: &[&str]) -> Self {
        Self {
            command: launchctl(arguments),
            privileged: scope == Scope::Machine,
            said: "loaded",
        }
    }
}

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
            privileged: scope == Scope::Machine,
            required,
        }
    }

    fn launchctl(scope: Scope, arguments: &[&str], required: bool) -> Self {
        Self {
            command: launchctl(arguments),
            privileged: scope == Scope::Machine,
            required,
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
        println!(
            "\nAltitude remote · as a service\n\n  writes   {}\n  scope    {}\n  logs     {}",
            self.path.display(),
            match (env::consts::OS, self.scope) {
                ("macos", Scope::User) => "this user · starts when you log in",
                ("macos", Scope::Machine) => "the whole machine · starts at boot",
                (_, Scope::User) => "this user · starts at boot, no session needed",
                (_, Scope::Machine) => "the whole machine · starts at boot",
            },
            self.logs,
        );
        println!("\n  {}", self.contents.replace('\n', "\n  ").trim_end());
        println!();
        for step in &self.commands {
            println!("  $ {}{}", prefix(step.privileged), step.command.join(" "));
        }
        if self.scope == Scope::User && matches!(env::consts::OS, "linux") {
            println!("\n  $ loginctl enable-linger {}", service.user);
            prose(
                "Without lingering, a user service stops when your last session ends — the \
                 remote would go away the moment you close the terminal you started it from.",
                "",
                "",
            );
        }
    }

    fn write(&self) -> Result<(), Box<dyn Error>> {
        println!();
        if let (Some(directory), false) = (self.path.parent(), self.privileged) {
            fs::create_dir_all(directory)?;
        }
        if self.privileged {
            println!("  $ sudo tee {}", self.path.display());
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
            println!("  wrote {}", self.path.display());
        }
        Ok(())
    }

    fn run(&self) -> Result<(), Box<dyn Error>> {
        if self.scope == Scope::User && matches!(env::consts::OS, "linux") {
            self.linger()?;
        }
        for step in &self.commands {
            println!("  $ {}{}", prefix(step.privileged), step.command.join(" "));
            if run_command(&step.command, step.privileged, step.required)? || !step.required {
                continue;
            }
            return Err(
                io::Error::other(format!("{} did not succeed", step.command.join(" "))).into(),
            );
        }
        Ok(())
    }

    /// A user service on Linux stops with the last session unless it is lingering,
    /// which is exactly what a headless remote must not do.
    fn linger(&self) -> Result<(), Box<dyn Error>> {
        let command = vec![
            "loginctl".to_owned(),
            "enable-linger".to_owned(),
            self.user.clone(),
        ];
        println!("  $ {}", command.join(" "));
        if run_command(&command, false, true)? {
            return Ok(());
        }
        println!();
        prose(
            "Lingering could not be enabled here. The service will still start, and it will stop \
             when your last session ends unless you enable it yourself.",
            "",
            "",
        );
        Ok(())
    }

    fn verify(&self) {
        println!();
        for check in &self.verify {
            let running = run_command(&check.command, check.privileged, false).unwrap_or(false);
            field(
                "state",
                if running {
                    check.said
                } else {
                    "not running yet"
                },
            );
        }
        field("logs", &self.logs);
        println!();
        prose(
            "With a shared tracker app installed, the first issue delegated to it is proven by a \
             delivery line in these logs.",
            "",
            "",
        );
    }
}

fn prefix(privileged: bool) -> &'static str {
    if privileged { "sudo " } else { "" }
}

/// Run one command, and answer whether it succeeded.
///
/// A step whose failure is not fatal keeps its complaints to itself: unloading a
/// job that was never loaded prints "no such process", which reads like a failure
/// in the middle of a first install that is going perfectly well.
fn run_command(command: &[String], privileged: bool, show_errors: bool) -> io::Result<bool> {
    let mut process = process_for(command, privileged)?;
    if !show_errors {
        process.stdout(Stdio::null());
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

fn uid() -> Result<String, Box<dyn Error>> {
    let output = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if uid.is_empty() {
        return Err(io::Error::other("this machine has no user id to install for").into());
    }
    Ok(uid)
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
