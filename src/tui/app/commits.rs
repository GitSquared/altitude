use std::{fs, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::Line;
use tokio::process::Command as HostCommand;

use crate::{project::ProjectCommits, tui::popups, tui::screens, tui::widgets::palette};

use popups::setup::{Setup, SetupKind};
use screens::get_started::{CommitsField, GetStarted};

use super::*;

impl App {
    pub(super) async fn commits_key(&mut self, panel: &mut GetStarted, key: KeyEvent) -> bool {
        match key.code {
            // Leaving saves, as it does for the project name. What cannot be
            // saved goes back to what is stored rather than trapping the keys in
            // an edit with no way out.
            KeyCode::Esc => {
                self.save_commits(panel).await;
                panel.commits.reload(&self.git_identity);
                false
            }
            KeyCode::Tab | KeyCode::Down => {
                panel.commits.move_focus(true);
                false
            }
            KeyCode::BackTab | KeyCode::Up => {
                panel.commits.move_focus(false);
                false
            }
            KeyCode::Enter => match panel.commits.field {
                CommitsField::Name | CommitsField::Email => {
                    if self.save_commits(panel).await {
                        panel.commits.reload(&self.git_identity);
                    }
                    false
                }
                CommitsField::Sign => {
                    if panel.commits.signs {
                        panel.commits.signs = false;
                        self.save_commits(panel).await;
                        return false;
                    }
                    let email = panel.commits.email.value().trim().to_owned();
                    if email.is_empty() || panel.commits.name.value().trim().is_empty() {
                        self.say("A commit needs a name and an email");
                        return false;
                    }
                    match self.make_signing_key(&email, "id_ed25519_signing").await {
                        Ok((pubkey, _)) => {
                            panel.commits.signs = true;
                            if self.save_commits(panel).await {
                                self.open_signing_key(pubkey);
                                return true;
                            }
                        }
                        Err(error) => self.problem(error),
                    }
                    false
                }
                CommitsField::Key => {
                    // Seeing the key is never refused for want of a name: the
                    // reason to look at it is often that the identity is not
                    // written yet. What is typed is saved on the way out.
                    if let Some(pubkey) = self.signing_key() {
                        if !panel.commits.name.value().trim().is_empty()
                            && !panel.commits.email.value().trim().is_empty()
                        {
                            self.save_commits(panel).await;
                        }
                        self.open_signing_key(pubkey);
                        return true;
                    }
                    let email = panel.commits.email.value().trim().to_owned();
                    if email.is_empty() {
                        self.say("Add an email first");
                        return false;
                    }
                    if !self.save_commits(panel).await {
                        return false;
                    }
                    match self.make_signing_key(&email, "id_ed25519_signing").await {
                        Ok((pubkey, _)) => {
                            if self.save_commits(panel).await {
                                self.open_signing_key(pubkey);
                                return true;
                            }
                            false
                        }
                        Err(error) => {
                            self.problem(error);
                            false
                        }
                    }
                }
            },
            _ => {
                match panel.commits.field {
                    CommitsField::Name => panel.commits.name.handle_key(key),
                    CommitsField::Email => panel.commits.email.handle_key(key),
                    _ => {}
                }
                false
            }
        }
    }

    pub(super) async fn save_commits(&mut self, panel: &mut GetStarted) -> bool {
        let name = panel.commits.name.value().trim().to_owned();
        let email = panel.commits.email.value().trim().to_owned();
        let signs = panel.commits.signs;
        self.save_commit_identity(&name, &name, &email, signs, true)
            .await
    }

    pub(super) async fn save_commit_identity(
        &mut self,
        label: &str,
        name: &str,
        email: &str,
        signs: bool,
        write_default: bool,
    ) -> bool {
        let label = label.trim();
        let name = name.trim();
        let email = email.trim();
        if label.is_empty() || name.is_empty() || email.is_empty() {
            self.say("An identity needs a name, a commit name, and an email");
            return false;
        }
        let existing_key = match &self.commit_edit {
            Some(CommitIdentityEdit::Saved(commits)) => commits.signing_key.clone(),
            Some(CommitIdentityEdit::Installation) | Some(CommitIdentityEdit::New) | None => None,
        };
        let signing_key = if signs {
            if write_default {
                if let Err(error) = self.make_signing_key(email, "id_ed25519_signing").await {
                    self.problem(error);
                    return false;
                }
                Some("/root/.ssh/id_ed25519_signing.pub".to_owned())
            } else if let Some(key) = existing_key {
                Some(key)
            } else {
                let name = Self::signing_key_name(label, email);
                match self.make_signing_key(email, &name).await {
                    Ok((_, path)) => Some(path),
                    Err(error) => {
                        self.problem(error);
                        return false;
                    }
                }
            }
        } else {
            None
        };
        if write_default {
            if let Err(error) = self.write_commits(name, email, signs).await {
                self.problem(error);
                return false;
            }
            self.refresh_git_identity().await;
        }
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return false;
        };
        let commits = ProjectCommits {
            label: label.to_owned(),
            name: name.to_owned(),
            email: email.to_owned(),
            signing_key,
        };
        let mut proposed = file.settings().clone();
        if let Some(CommitIdentityEdit::Saved(old)) = self.commit_edit.clone() {
            proposed.replace_identity(&old, commits);
        } else {
            proposed.remember_identity(&commits);
        }
        if let Err(error) = file.save(proposed) {
            self.problem(error.to_string());
            return false;
        }
        true
    }

    pub(super) async fn write_commits(
        &self,
        name: &str,
        email: &str,
        signs: bool,
    ) -> Result<(), String> {
        let directory = self
            .config
            .profile_dir(self.active_vcs().profile())
            .join("git");
        fs::create_dir_all(&directory)
            .map_err(|error| format!("could not make {}: {error}", directory.display()))?;
        let path = directory.join("config");
        let mut values = vec![
            ("user.name", name.to_owned()),
            ("user.email", email.to_owned()),
        ];
        if signs {
            values.extend([
                ("gpg.format", "ssh".to_owned()),
                (
                    "user.signingkey",
                    "/root/.ssh/id_ed25519_signing.pub".to_owned(),
                ),
                (
                    "gpg.ssh.allowedSignersFile",
                    "/root/.ssh/allowed_signers".to_owned(),
                ),
                ("commit.gpgsign", "true".to_owned()),
            ]);
        } else {
            values.push(("commit.gpgsign", "false".to_owned()));
        }
        for (key, value) in values {
            let output = HostCommand::new("git")
                .args(["config", "--file"])
                .arg(&path)
                .arg(key)
                .arg(value)
                .output()
                .await
                .map_err(|error| format!("could not run git: {error}"))?;
            if !output.status.success() {
                let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                return Err(format!("git config {key} failed: {reason}"));
            }
        }
        Ok(())
    }

    pub(super) fn signing_key_name(label: &str, email: &str) -> String {
        let source = if label.trim().is_empty() {
            email
        } else {
            label
        };
        let slug: String = source
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug = if slug.is_empty() { "identity" } else { &slug };
        format!("id_ed25519_signing_{}", &slug[..slug.len().min(40)])
    }

    pub(super) async fn make_signing_key(
        &self,
        email: &str,
        name: &str,
    ) -> Result<(String, String), String> {
        let directory = self
            .config
            .profile_dir(self.active_vcs().profile())
            .join("ssh");
        fs::create_dir_all(&directory)
            .map_err(|error| format!("could not make {}: {error}", directory.display()))?;
        let key = directory.join(name);
        let public = directory.join(format!("{name}.pub"));
        if !public.is_file() {
            // No passphrase: this key signs from inside a sandbox where nothing
            // can answer a prompt, so a key that asked for one would fail at the
            // first commit.
            let output = HostCommand::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", email, "-f"])
                .arg(&key)
                .output()
                .await
                .map_err(|error| format!("could not run ssh-keygen: {error}"))?;
            if !output.status.success() {
                let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                return Err(format!("ssh-keygen failed: {reason}"));
            }
        }
        let text = fs::read_to_string(&public)
            .map_err(|error| format!("could not read {}: {error}", public.display()))?;
        let text = text.trim().to_owned();
        // The signer file is what makes a signature verifiable here; keep every
        // identity's line and replace only this email's.
        let signers = directory.join("allowed_signers");
        let mut lines: Vec<String> = fs::read_to_string(&signers)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with(&format!("{email} ")))
            .map(str::to_owned)
            .collect();
        lines.push(format!("{email} {text}"));
        fs::write(&signers, format!("{}\n", lines.join("\n")))
            .map_err(|error| format!("could not write the allowed signers: {error}"))?;
        Ok((text, format!("/root/.ssh/{name}.pub")))
    }

    pub(super) fn signing_key_path(&self) -> Option<PathBuf> {
        let path = self
            .config
            .profile_dir(self.active_vcs().profile())
            .join("ssh/id_ed25519_signing.pub");
        path.is_file().then_some(path)
    }

    pub(super) fn signing_key(&self) -> Option<String> {
        let path = self.signing_key_path()?;
        fs::read_to_string(path)
            .ok()
            .map(|text| text.trim().to_owned())
    }

    pub(super) fn open_signing_key(&mut self, public: String) {
        let (intro, steps) = self.active_vcs().signing_key_instructions();
        let lines = vec![
            Line::styled(intro, palette::muted()),
            Line::from(""),
            Line::styled(public.clone(), palette::primary()),
            Line::from(""),
            Line::styled(steps, palette::muted()),
        ];
        self.foreground = Foreground::Setup(
            Setup::new(SetupKind::SigningKey, lines)
                .with_copy(public)
                .returning_to_card(),
        );
    }

    pub(super) fn commit_name(&mut self, panel: &GetStarted) -> bool {
        let name = panel.name.value().trim();
        if name.is_empty() {
            return false;
        }
        if name == self.active_project_name() {
            return true;
        }
        self.rename_project(&self.active_project_id(), name)
    }

    pub(super) fn finish_name(&mut self, panel: &mut GetStarted) {
        if !self.commit_name(panel) {
            let saved = self.active_project_name();
            panel.reset_name(&saved);
        }
        panel.name_editing = false;
    }

    pub(in crate::tui) fn delete_commit_identity(&mut self, commits: &ProjectCommits) {
        let Some(file) = self.settings_file.as_mut() else {
            self.problem("Settings are unavailable; fix the file and reopen");
            return;
        };
        let mut proposed = file.settings().clone();
        proposed.remove_identity(commits);
        match file.save(proposed) {
            Ok(()) => self.say("Commit identity deleted"),
            Err(error) => self.problem(error.to_string()),
        }
    }
}
