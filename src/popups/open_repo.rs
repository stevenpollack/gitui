use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, InputType, TextInputComponent,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings,
};
use anyhow::Result;
use asyncgit::sync::{repo_open_error, RepoPath};
use crossterm::event::Event;
use ratatui::{layout::Rect, Frame};
use std::path::PathBuf;

/// Prompt for a filesystem path and re-open the app against that repo.
///
/// Reuses the same in-process rebuild that entering a submodule or
/// switching a worktree uses: on confirm it pushes
/// [`InternalEvent::OpenRepo`], which the app turns into a
/// `QuitState::OpenSubmodule` that rebuilds a fresh `App` at the new
/// path (see `main.rs`).
pub struct OpenRepoPopup {
	input: TextInputComponent,
	queue: Queue,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for OpenRepoPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
			self.input.draw(f, rect)?;
		}

		Ok(())
	}
}

impl Component for OpenRepoPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			self.input.commands(out, force_all);

			out.push(CommandInfo::new(
				strings::commands::open_repo_confirm_msg(
					&self.key_config,
				),
				true,
				true,
			));
		}

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.is_visible() {
			if self.input.event(ev)?.is_consumed() {
				return Ok(EventState::Consumed);
			}

			if let Event::Key(e) = ev {
				if key_match(e, self.key_config.keys.enter) {
					self.confirm();
				}

				return Ok(EventState::Consumed);
			}
		}
		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.input.is_visible()
	}

	fn hide(&mut self) {
		self.input.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.input.show()?;

		Ok(())
	}
}

impl OpenRepoPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			queue: env.queue.clone(),
			input: TextInputComponent::new(
				env,
				&strings::open_repo_popup_title(&env.key_config),
				&strings::open_repo_popup_msg(&env.key_config),
				false,
			)
			.with_input_type(InputType::Singleline),
			key_config: env.key_config.clone(),
		}
	}

	///
	pub fn open(&mut self) -> Result<()> {
		self.show()?;

		Ok(())
	}

	fn confirm(&mut self) {
		let raw = self.input.get_text().trim().to_string();

		if raw.is_empty() {
			self.hide();
			return;
		}

		let path = expand_path(&raw);

		if let Some(err) =
			repo_open_error(&RepoPath::Path(path.clone()))
		{
			self.input.clear();
			self.hide();
			self.queue.push(InternalEvent::ShowErrorMsg(format!(
				"not a valid repository:\n{err}"
			)));
			return;
		}

		self.input.clear();
		self.hide();
		self.queue.push(InternalEvent::OpenRepo { path });
	}
}

/// Expand a leading `~`/`~/` to the user's home directory and make the
/// path absolute (resolved against the process cwd for relative input).
/// Absolute input passes through unchanged. Does not require the path to
/// exist; validity is checked separately via [`repo_open_error`].
fn expand_path(raw: &str) -> PathBuf {
	let expanded = if raw == "~" {
		dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
	} else if let Some(rest) = raw.strip_prefix("~/") {
		dirs::home_dir().map_or_else(
			|| PathBuf::from(raw),
			|home| home.join(rest),
		)
	} else {
		PathBuf::from(raw)
	};

	std::path::absolute(&expanded).unwrap_or(expanded)
}
