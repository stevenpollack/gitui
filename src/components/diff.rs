use super::{
	diff_highlight::{AsyncDiffHighlightJob, LineHighlight},
	utils::scroll_horizontal::HorizontalScroll,
	utils::scroll_vertical::VerticalScroll,
	CommandBlocking, Direction, DrawableComponent,
	HorizontalScrollType, ScrollType,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{Action, InternalEvent, NeedsUpdate, Queue, ResetItem},
	string_utils::tabs_to_spaces,
	string_utils::trim_offset,
	strings, try_or_popup,
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	hash,
	sync::{self, diff::DiffLinePosition, RepoPathRef},
	DiffLine, DiffLineType, FileDiff,
};
use bytesize::ByteSize;
use crossterm::event::Event;
use ratatui::{
	layout::{Alignment, Constraint, Layout, Rect},
	style::Style,
	symbols,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{
	borrow::Cow,
	cell::{Cell, RefCell},
	cmp,
	ops::Range,
	path::Path,
};
use unicode_width::UnicodeWidthStr;

/// Bundles the two hunk-position flags `get_line_to_add` needs to
/// pick the left gutter glyph/style — kept together to stay under
/// clippy's `too_many_arguments` limit now that the method also
/// takes `&self`.
#[derive(Clone, Copy)]
struct HunkLineFlags {
	selected_hunk: bool,
	end_of_hunk: bool,
}

/// One visual row of the split (side-by-side) diff. `left`/`right` are
/// FLAT diff-line indices (matching `build_highlight`'s indexing), or
/// `None` for a blank pad cell on that side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SplitRow {
	left: Option<usize>,
	right: Option<usize>,
}

/// Emits paired change rows from the accumulated deletion/addition flat
/// indices, then clears both buffers. Deletions map to the left
/// column, additions to the right; an unpaired line gets a blank cell
/// on the opposite side.
fn flush_split_change(
	rows: &mut Vec<SplitRow>,
	dels: &mut Vec<usize>,
	adds: &mut Vec<usize>,
) {
	let n = dels.len().max(adds.len());
	for i in 0..n {
		rows.push(SplitRow {
			left: dels.get(i).copied(),
			right: adds.get(i).copied(),
		});
	}
	dels.clear();
	adds.clear();
}

/// Builds the side-by-side row model for `diff`. Deletions align on the
/// left column, additions on the right, context/header lines span both
/// columns. The running `flat` counter advances for every line (headers
/// included) so the indices line up with the flat indexing used by
/// [`DiffComponent::get_text`] and `build_highlight`.
fn build_split_rows(diff: &FileDiff) -> Vec<SplitRow> {
	let mut rows = Vec::new();
	let (mut pending_del, mut pending_add): (Vec<usize>, Vec<usize>) =
		(Vec::new(), Vec::new());
	let mut flat = 0_usize;

	for hunk in &diff.hunks {
		for line in &hunk.lines {
			match line.line_type {
				DiffLineType::Header | DiffLineType::None => {
					flush_split_change(
						&mut rows,
						&mut pending_del,
						&mut pending_add,
					);
					rows.push(SplitRow {
						left: Some(flat),
						right: Some(flat),
					});
				}
				DiffLineType::Delete => {
					if !pending_add.is_empty() {
						flush_split_change(
							&mut rows,
							&mut pending_del,
							&mut pending_add,
						);
					}
					pending_del.push(flat);
				}
				DiffLineType::Add => pending_add.push(flat),
			}
			flat += 1;
		}
	}

	flush_split_change(&mut rows, &mut pending_del, &mut pending_add);

	rows
}

/// Flattens all hunk lines of `diff` into a single in-order slice of
/// `DiffLine` references, so a FLAT line index can be resolved in O(1).
fn flat_diff_lines(diff: &FileDiff) -> Vec<&DiffLine> {
	diff.hunks.iter().flat_map(|h| h.lines.iter()).collect()
}

/// Emits the syntect foreground spans for `content`, clipped to the
/// horizontally-scrolled visible window (`scrolled` bytes trimmed from
/// the left). Token gaps are filled with `bg`; highlighted tokens use
/// `seg_style.patch(bg)`. Shared by the unified and split renderers.
fn push_clipped_spans(
	spans: &mut Vec<Span<'static>>,
	segments: &[(Range<usize>, Style)],
	content: &str,
	scrolled: usize,
	bg: Style,
) {
	let visible = trim_offset(content, scrolled);
	let start = content.len() - visible.len();
	let mut last = start;
	for (range, seg_style) in segments {
		let seg_start = range.start.max(start);
		let seg_end = range.end.min(content.len());
		if seg_end <= seg_start {
			continue;
		}
		if seg_start > last {
			if let Some(t) = content.get(last..seg_start) {
				spans.push(Span::styled(t.to_string(), bg));
			}
		}
		if let Some(t) = content.get(seg_start..seg_end) {
			spans.push(Span::styled(
				t.to_string(),
				seg_style.patch(bg),
			));
		}
		last = seg_end;
	}
	if last < content.len() {
		if let Some(t) = content.get(last..) {
			spans.push(Span::styled(t.to_string(), bg));
		}
	}
}

#[derive(Default)]
struct Current {
	path: String,
	is_stage: bool,
	hash: u64,
}

///
#[derive(Clone, Copy)]
enum Selection {
	Single(usize),
	Multiple(usize, usize),
}

impl Selection {
	const fn get_start(&self) -> usize {
		match self {
			Self::Single(start) | Self::Multiple(start, _) => *start,
		}
	}

	const fn get_end(&self) -> usize {
		match self {
			Self::Single(end) | Self::Multiple(_, end) => *end,
		}
	}

	fn get_top(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::min(*start, *end),
		}
	}

	fn get_bottom(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::max(*start, *end),
		}
	}

	fn modify(&mut self, direction: Direction, max: usize) {
		let start = self.get_start();
		let old_end = self.get_end();

		*self = match direction {
			Direction::Up => {
				Self::Multiple(start, old_end.saturating_sub(1))
			}

			Direction::Down => {
				Self::Multiple(start, cmp::min(old_end + 1, max))
			}
		};
	}

	fn contains(&self, index: usize) -> bool {
		match self {
			Self::Single(start) => index == *start,
			Self::Multiple(start, end) => {
				if start <= end {
					*start <= index && index <= *end
				} else {
					*end <= index && index <= *start
				}
			}
		}
	}
}

///
pub struct DiffComponent {
	repo: RepoPathRef,
	diff: Option<FileDiff>,
	longest_line: usize,
	longest_split_line: usize,
	split_rows: Vec<SplitRow>,
	pending: bool,
	selection: Selection,
	selected_hunk: Option<usize>,
	current_size: Cell<(u16, u16)>,
	focused: bool,
	current: Current,
	vertical_scroll: VerticalScroll,
	horizontal_scroll: HorizontalScroll,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	is_immutable: bool,
	options: SharedOptions,
	syntax_highlight: RefCell<Vec<Option<LineHighlight>>>,
	highlight_job: AsyncSingleJob<AsyncDiffHighlightJob>,
}

impl DiffComponent {
	///
	pub fn new(env: &Environment, is_immutable: bool) -> Self {
		Self {
			focused: false,
			queue: env.queue.clone(),
			current: Current::default(),
			pending: false,
			selected_hunk: None,
			diff: None,
			longest_line: 0,
			longest_split_line: 0,
			split_rows: Vec::new(),
			current_size: Cell::new((0, 0)),
			selection: Selection::Single(0),
			vertical_scroll: VerticalScroll::new(),
			horizontal_scroll: HorizontalScroll::new(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			is_immutable,
			repo: env.repo.clone(),
			options: env.options.clone(),
			syntax_highlight: RefCell::new(Vec::new()),
			highlight_job: AsyncSingleJob::new(
				env.sender_app.clone(),
			),
		}
	}
	///
	fn can_scroll(&self) -> bool {
		self.diff.as_ref().is_some_and(|diff| diff.lines > 1)
	}
	///
	pub fn current(&self) -> (String, bool) {
		(self.current.path.clone(), self.current.is_stage)
	}
	///
	const fn can_edit_file(&self) -> bool {
		!self.is_immutable && !self.current.path.is_empty()
	}
	///
	pub fn clear(&mut self, pending: bool) {
		self.current = Current::default();
		self.diff = None;
		self.longest_line = 0;
		self.longest_split_line = 0;
		self.split_rows.clear();
		self.vertical_scroll.reset();
		self.horizontal_scroll.reset();
		self.selection = Selection::Single(0);
		self.selected_hunk = None;
		self.pending = pending;
		self.syntax_highlight.borrow_mut().clear();
		self.highlight_job.cancel();
	}
	///
	pub fn update(
		&mut self,
		path: String,
		is_stage: bool,
		diff: FileDiff,
	) {
		self.pending = false;

		let hash = hash(&diff);

		if self.current.hash != hash {
			let reset_selection = self.current.path != path;

			self.current = Current {
				path,
				is_stage,
				hash,
			};

			self.diff = Some(diff);

			self.longest_line = self
				.diff
				.iter()
				.flat_map(|diff| diff.hunks.iter())
				.flat_map(|hunk| hunk.lines.iter())
				.map(|line| {
					let converted_content = tabs_to_spaces(
						line.content.as_ref().to_string(),
					);

					converted_content.len()
				})
				.max()
				.map_or(0, |len| {
					// Each hunk uses a 1-character wide vertical bar to its left to indicate
					// selection.
					len + 1
				});

			self.split_rows = self
				.diff
				.as_ref()
				.map(build_split_rows)
				.unwrap_or_default();
			// split cells have no per-line gutter, so they are one
			// column narrower than the unified `longest_line`.
			self.longest_split_line =
				self.longest_line.saturating_sub(1);

			if reset_selection {
				self.vertical_scroll.reset();
				self.selection = Selection::Single(0);
				self.update_selection(0);
			} else {
				let old_selection = match self.selection {
					Selection::Single(line) => line,
					Selection::Multiple(start, _) => start,
				};
				self.update_selection(old_selection);
			}

			self.clamp_selection_to_mode();

			self.spawn_highlight();
		}
	}

	const MAX_HIGHLIGHT_LINES: usize = 10_000;

	/// Spawns the syntect highlight for the current diff on the
	/// shared threadpool (see [`AsyncDiffHighlightJob`]). The result
	/// is picked up later by [`Self::poll_highlight`] during `draw`.
	fn spawn_highlight(&self) {
		self.syntax_highlight.borrow_mut().clear();

		if !self.options.borrow().diff_highlight_style().is_on() {
			self.highlight_job.cancel();
			return;
		}

		let Some(diff) = self.diff.as_ref() else {
			return;
		};

		if diff.hunks.is_empty()
			|| diff.lines > Self::MAX_HIGHLIGHT_LINES
		{
			return;
		}

		let lines: Vec<(String, DiffLineType)> = diff
			.hunks
			.iter()
			.flat_map(|h| h.lines.iter())
			.map(|l| (l.content.as_ref().to_string(), l.line_type))
			.collect();

		let path = self.current.path.clone();
		let theme = self.theme.get_syntax();
		let hash = self.current.hash;

		self.highlight_job.spawn(AsyncDiffHighlightJob::new(
			hash, lines, path, theme,
		));
	}

	/// Applies a finished highlight job's result if it matches the
	/// currently displayed diff (stale results are discarded).
	fn poll_highlight(&self) {
		if let Some(job) = self.highlight_job.take_last() {
			if let Some((hash, result)) = job.result() {
				if hash == self.current.hash {
					*self.syntax_highlight.borrow_mut() = result;
				}
			}
		}
	}

	/// Whether the split (side-by-side) diff view is active.
	fn is_split(&self) -> bool {
		self.options.borrow().diff_view().is_split()
	}

	/// Whether the unified diff view is active (the default). Staging,
	/// hunk-jumping and editing only operate in this mode.
	fn is_unified(&self) -> bool {
		!self.is_split()
	}

	/// Number of selectable rows in the active view: visual split rows
	/// in split mode, flat diff lines in unified mode.
	fn row_count(&self) -> usize {
		if self.is_split() {
			self.split_rows.len()
		} else {
			self.lines_count()
		}
	}

	fn move_selection(&mut self, move_type: ScrollType) {
		if self.diff.is_some() {
			let max = self.row_count().saturating_sub(1);

			let new_start = match move_type {
				ScrollType::Down => {
					self.selection.get_bottom().saturating_add(1)
				}
				ScrollType::Up => {
					self.selection.get_top().saturating_sub(1)
				}
				ScrollType::Home => 0,
				ScrollType::End => max,
				ScrollType::PageDown => {
					self.selection.get_bottom().saturating_add(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
				ScrollType::PageUp => {
					self.selection.get_top().saturating_sub(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
			};

			self.update_selection(new_start);
		}
	}

	fn update_selection(&mut self, new_start: usize) {
		if self.diff.is_none() {
			return;
		}
		let max = self.row_count().saturating_sub(1);
		let new_start = cmp::min(max, new_start);
		self.selection = Selection::Single(new_start);
		// hunk-jumping/staging are unified-only, so the split view
		// keeps no selected hunk.
		self.selected_hunk = if self.is_split() {
			None
		} else {
			self.diff.as_ref().and_then(|diff| {
				Self::find_selected_hunk(diff, new_start)
			})
		};
	}

	/// Clamps the selection to the active view's row count (rows differ
	/// between unified and split) and refreshes the selected hunk.
	/// Called on view toggle and after rebuilding the row model. Both
	/// ends are clamped so a shift-selection survives the toggle.
	fn clamp_selection_to_mode(&mut self) {
		if self.diff.is_none() {
			return;
		}
		let max = self.row_count().saturating_sub(1);
		let start = cmp::min(self.selection.get_start(), max);
		let end = cmp::min(self.selection.get_end(), max);
		self.selection = if start == end {
			Selection::Single(start)
		} else {
			Selection::Multiple(start, end)
		};
		// hunk-jumping/staging are unified-only, so the split view
		// keeps no selected hunk.
		self.selected_hunk = if self.is_split() {
			None
		} else {
			self.diff
				.as_ref()
				.and_then(|diff| Self::find_selected_hunk(diff, end))
		};
	}

	fn lines_count(&self) -> usize {
		self.diff.as_ref().map_or(0, |diff| diff.lines)
	}

	fn max_scroll_right(&self) -> usize {
		self.longest_line
			.saturating_sub(self.current_size.get().0.into())
	}

	/// Maximum horizontal scroll offset for a split column of the given
	/// width (split cells carry no gutter, unlike [`Self::max_scroll_right`]).
	fn max_scroll_right_split(&self, col_w: u16) -> usize {
		self.longest_split_line.saturating_sub(usize::from(col_w))
	}

	fn modify_selection(&mut self, direction: Direction) {
		if self.diff.is_some() {
			self.selection.modify(direction, self.row_count());
		}
	}

	fn copy_selection(&self) {
		let Some(diff) = &self.diff else {
			return;
		};

		let lines_to_copy: Vec<String> = if self.is_split() {
			// selection is a row index; prefer the new (right) side,
			// falling back to the old (left) side for pure deletions.
			let flat = flat_diff_lines(diff);
			self.split_rows
				.iter()
				.enumerate()
				.filter(|(row_idx, _)| {
					self.selection.contains(*row_idx)
				})
				.filter_map(|(_, row)| {
					let idx = row.right.or(row.left)?;
					flat.get(idx).map(|line| {
						line.content
							.trim_matches(|c| c == '\n' || c == '\r')
							.to_string()
					})
				})
				.collect()
		} else {
			diff.hunks
				.iter()
				.flat_map(|hunk| hunk.lines.iter())
				.enumerate()
				.filter_map(|(i, line)| {
					if self.selection.contains(i) {
						Some(
							line.content
								.trim_matches(|c| {
									c == '\n' || c == '\r'
								})
								.to_string(),
						)
					} else {
						None
					}
				})
				.collect()
		};

		try_or_popup!(
			self,
			"copy to clipboard error:",
			crate::clipboard::copy_string(&lines_to_copy.join("\n"))
		);
	}

	fn find_selected_hunk(
		diff: &FileDiff,
		line_selected: usize,
	) -> Option<usize> {
		let mut line_cursor = 0_usize;
		for (i, hunk) in diff.hunks.iter().enumerate() {
			let hunk_len = hunk.lines.len();
			let hunk_min = line_cursor;
			let hunk_max = line_cursor + hunk_len;

			let hunk_selected =
				hunk_min <= line_selected && hunk_max > line_selected;

			if hunk_selected {
				return Some(i);
			}

			line_cursor += hunk_len;
		}

		None
	}

	fn get_text(&self, width: u16, height: u16) -> Vec<Line<'_>> {
		if let Some(diff) = &self.diff {
			return if diff.hunks.is_empty() {
				self.get_text_binary(diff)
			} else {
				let mut res: Vec<Line> = Vec::new();

				let highlights = self.syntax_highlight.borrow();

				let min = self.vertical_scroll.get_top();
				let max = min + height as usize;

				let mut line_cursor = 0_usize;
				let mut lines_added = 0_usize;

				for (i, hunk) in diff.hunks.iter().enumerate() {
					let hunk_selected = self.focused()
						&& self.selected_hunk.is_some_and(|s| s == i);

					if lines_added >= height as usize {
						break;
					}

					let hunk_len = hunk.lines.len();
					let hunk_min = line_cursor;
					let hunk_max = line_cursor + hunk_len;

					if Self::hunk_visible(
						hunk_min, hunk_max, min, max,
					) {
						for (i, line) in hunk.lines.iter().enumerate()
						{
							if line_cursor >= min
								&& line_cursor <= max
							{
								let highlight = highlights
									.get(line_cursor)
									.and_then(Option::as_ref)
									.map(Vec::as_slice);

								res.push(
									self.get_line_to_add(
										width,
										line,
										self.focused()
											&& self
												.selection
												.contains(
													line_cursor,
												),
										HunkLineFlags {
											selected_hunk:
												hunk_selected,
											end_of_hunk: i
												== hunk_len - 1,
										},
										self.horizontal_scroll
											.get_right(),
										highlight,
									),
								);
								lines_added += 1;
							}

							line_cursor += 1;
						}
					} else {
						line_cursor += hunk_len;
					}
				}

				res
			};
		}

		vec![]
	}

	fn get_text_binary(&self, diff: &FileDiff) -> Vec<Line<'_>> {
		let is_positive = diff.size_delta >= 0;
		let delta_byte_size =
			ByteSize::b(diff.size_delta.unsigned_abs());
		let sign = if is_positive { "+" } else { "-" };
		vec![Line::from(vec![
			Span::raw(Cow::from("size: ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.0))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" -> ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.1))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" (")),
			Span::styled(
				Cow::from(format!("{sign}{delta_byte_size:}")),
				self.theme.diff_line(
					if is_positive {
						DiffLineType::Add
					} else {
						DiffLineType::Delete
					},
					false,
				),
			),
			Span::raw(Cow::from(")")),
		])]
	}

	fn get_line_to_add(
		&self,
		width: u16,
		line: &DiffLine,
		selected: bool,
		hunk_flags: HunkLineFlags,
		scrolled_right: usize,
		highlight: Option<&[(Range<usize>, Style)]>,
	) -> Line<'_> {
		let theme = &self.theme;
		let hl_style = self.options.borrow().diff_highlight_style();

		let style = if hunk_flags.selected_hunk {
			theme.diff_hunk_marker(true)
		} else if hl_style.color_gutter() {
			theme.diff_line(line.line_type, false)
		} else {
			theme.diff_hunk_marker(false)
		};

		let is_content_line =
			matches!(line.line_type, DiffLineType::None);

		let left_side_of_line = if hunk_flags.end_of_hunk {
			Span::styled(Cow::from(symbols::line::BOTTOM_LEFT), style)
		} else {
			match line.line_type {
				DiffLineType::Header => Span::styled(
					Cow::from(symbols::line::TOP_LEFT),
					style,
				),
				_ => Span::styled(
					Cow::from(symbols::line::VERTICAL),
					style,
				),
			}
		};

		let content =
			if !is_content_line && line.content.as_ref().is_empty() {
				theme.line_break()
			} else {
				tabs_to_spaces(line.content.as_ref().to_string())
			};

		if let Some(segments) = highlight {
			if !segments.is_empty()
				&& !matches!(line.line_type, DiffLineType::Header)
			{
				let mut spans = vec![left_side_of_line.clone()];
				spans.extend(self.highlighted_content_spans(
					width,
					line.line_type,
					selected,
					scrolled_right,
					segments,
					&content,
				));
				return Line::from(spans);
			}
		}

		let content = trim_offset(&content, scrolled_right);

		let filled = if selected {
			// selected line
			format!("{content:w$}\n", w = width as usize)
		} else {
			// weird eof missing eol line
			format!("{content}\n")
		};

		Line::from(vec![
			left_side_of_line,
			Span::styled(
				Cow::from(filled),
				theme.diff_line(line.line_type, selected),
			),
		])
	}

	/// Builds the syntect-highlighted content spans (sign glyph,
	/// per-token foreground spans, optional row tint/pad) for one
	/// diff line, honouring the active [`DiffHighlightStyle`]. The
	/// caller prepends the left gutter span.
	fn highlighted_content_spans(
		&self,
		width: u16,
		line_type: DiffLineType,
		selected: bool,
		scrolled_right: usize,
		segments: &[(Range<usize>, Style)],
		content: &str,
	) -> Vec<Span<'static>> {
		let theme = &self.theme;
		let hl_style = self.options.borrow().diff_highlight_style();
		let mut spans: Vec<Span<'static>> = Vec::new();

		if hl_style.shows_sign() {
			let (sign, sign_style) = match line_type {
				DiffLineType::Add => {
					("+", theme.diff_line(DiffLineType::Add, false))
				}
				DiffLineType::Delete => (
					"-",
					theme.diff_line(DiffLineType::Delete, false),
				),
				_ => (" ", theme.diff_hunk_marker(false)),
			};
			spans.push(Span::styled(sign, sign_style));
		}

		let visible = trim_offset(content, scrolled_right);
		let bg = if hl_style.shows_tint() {
			theme.diff_line_tint(line_type, selected)
		} else {
			theme.diff_line_highlight_bg(selected)
		};
		push_clipped_spans(
			&mut spans,
			segments,
			content,
			scrolled_right,
			bg,
		);

		let tint_row = hl_style.shows_tint()
			&& matches!(
				line_type,
				DiffLineType::Add | DiffLineType::Delete
			);
		if selected || tint_row {
			let used = UnicodeWidthStr::width(visible);
			let pad = (width as usize).saturating_sub(used);
			spans.push(Span::styled(format!("{:pad$}\n", ""), bg));
		} else {
			spans.push(Span::raw("\n"));
		}

		spans
	}

	/// Minimum inner (content) width required to render the split view.
	/// Below this the columns would be uselessly narrow, so a hint is
	/// shown instead.
	const MIN_SPLIT_INNER_WIDTH: u16 = 48;

	/// Builds the visible-window `Line`s for both split columns. `lw` /
	/// `rw` are the column widths; only the rows in `[top, top+height)`
	/// are materialised.
	fn get_split_text(
		&self,
		lw: u16,
		rw: u16,
		height: usize,
	) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
		let Some(diff) = &self.diff else {
			return (Vec::new(), Vec::new());
		};

		let flat = flat_diff_lines(diff);
		let highlights = self.syntax_highlight.borrow();
		let highlights: &[Option<LineHighlight>] = &highlights;
		let top = self.vertical_scroll.get_top();
		let scrolled = self.horizontal_scroll.get_right();

		let mut left = Vec::new();
		let mut right = Vec::new();

		for (row_idx, row) in
			self.split_rows.iter().enumerate().skip(top).take(height)
		{
			let selected =
				self.focused() && self.selection.contains(row_idx);
			left.push(self.get_split_cell(
				lw, row.left, &flat, highlights, selected, scrolled,
			));
			right.push(self.get_split_cell(
				rw, row.right, &flat, highlights, selected, scrolled,
			));
		}

		(left, right)
	}

	/// Renders a single split cell (one column of one row) to a `Line`.
	/// `cell` is a FLAT diff-line index, or `None` for a blank pad cell.
	/// Split cells carry no gutter bar and no `+`/`-` sign glyph — the
	/// column position already conveys old-vs-new.
	fn get_split_cell(
		&self,
		width: u16,
		cell: Option<usize>,
		flat: &[&DiffLine],
		highlights: &[Option<LineHighlight>],
		selected: bool,
		scrolled: usize,
	) -> Line<'static> {
		let theme = &self.theme;

		let Some(line) = cell.and_then(|idx| flat.get(idx).copied())
		else {
			// blank pad cell (no counterpart on this side)
			return Line::from(Span::styled(
				format!("{:w$}", "", w = width as usize),
				theme.diff_line_highlight_bg(selected),
			));
		};

		let is_content_line =
			matches!(line.line_type, DiffLineType::None);
		let content =
			if !is_content_line && line.content.as_ref().is_empty() {
				theme.line_break()
			} else {
				tabs_to_spaces(line.content.as_ref().to_string())
			};

		let hl_style = self.options.borrow().diff_highlight_style();
		let is_header =
			matches!(line.line_type, DiffLineType::Header);
		let bg = if selected {
			theme.diff_line_highlight_bg(true)
		} else if hl_style.shows_tint()
			&& matches!(
				line.line_type,
				DiffLineType::Add | DiffLineType::Delete
			) {
			theme.diff_line_tint(line.line_type, false)
		} else {
			Style::default()
		};

		let highlight = cell
			.and_then(|idx| highlights.get(idx))
			.and_then(Option::as_ref)
			.map(Vec::as_slice)
			.filter(|s| !s.is_empty() && !is_header);

		let visible = trim_offset(&content, scrolled);
		let mut spans: Vec<Span<'static>> = Vec::new();

		if let Some(segments) = highlight {
			push_clipped_spans(
				&mut spans, segments, &content, scrolled, bg,
			);
		} else {
			// no syntect highlight (off / pending / header): keep the
			// red/green foreground via `diff_line`.
			spans.push(Span::styled(
				visible.to_string(),
				theme.diff_line(line.line_type, selected),
			));
		}

		// pad to the full column so tint/selection fills to the divider
		let used = UnicodeWidthStr::width(visible);
		let pad = (width as usize).saturating_sub(used);
		if pad > 0 {
			spans.push(Span::styled(format!("{:pad$}", ""), bg));
		}

		Line::from(spans)
	}

	const fn hunk_visible(
		hunk_min: usize,
		hunk_max: usize,
		min: usize,
		max: usize,
	) -> bool {
		// full overlap
		if hunk_min <= min && hunk_max >= max {
			return true;
		}

		// partly overlap
		if (hunk_min >= min && hunk_min <= max)
			|| (hunk_max >= min && hunk_max <= max)
		{
			return true;
		}

		false
	}

	fn unstage_hunk(&self) -> Result<()> {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;
				sync::unstage_hunk(
					&self.repo.borrow(),
					&self.current.path,
					hash,
					Some(self.options.borrow().diff_options()),
				)?;
				self.queue_update();
			}
		}

		Ok(())
	}

	fn stage_hunk(&self) -> Result<()> {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				if diff.untracked {
					sync::stage_add_file(
						&self.repo.borrow(),
						Path::new(&self.current.path),
					)?;
				} else {
					let hash = diff.hunks[hunk].header_hash;
					sync::stage_hunk(
						&self.repo.borrow(),
						&self.current.path,
						hash,
						Some(self.options.borrow().diff_options()),
					)?;
				}

				self.queue_update();
			}
		}

		Ok(())
	}

	fn queue_update(&self) {
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
	}

	fn reset_hunk(&self) {
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;

				self.queue.push(InternalEvent::ConfirmAction(
					Action::ResetHunk(
						self.current.path.clone(),
						hash,
					),
				));
			}
		}
	}

	fn reset_lines(&self) {
		self.queue.push(InternalEvent::ConfirmAction(
			Action::ResetLines(
				self.current.path.clone(),
				self.selected_lines(),
			),
		));
	}

	fn stage_lines(&self) {
		if let Some(diff) = &self.diff {
			//TODO: support untracked files as well
			if !diff.untracked {
				let selected_lines = self.selected_lines();

				try_or_popup!(
					self,
					"(un)stage lines:",
					sync::stage_lines(
						&self.repo.borrow(),
						&self.current.path,
						self.is_stage(),
						&selected_lines,
					)
				);

				self.queue_update();
			}
		}
	}

	fn selected_lines(&self) -> Vec<DiffLinePosition> {
		self.diff
			.as_ref()
			.map(|diff| {
				diff.hunks
					.iter()
					.flat_map(|hunk| hunk.lines.iter())
					.enumerate()
					.filter_map(|(i, line)| {
						let is_add_or_delete = line.line_type
							== DiffLineType::Add
							|| line.line_type == DiffLineType::Delete;
						if self.selection.contains(i)
							&& is_add_or_delete
						{
							Some(line.position)
						} else {
							None
						}
					})
					.collect()
			})
			.unwrap_or_default()
	}

	fn reset_untracked(&self) {
		self.queue.push(InternalEvent::ConfirmAction(Action::Reset(
			ResetItem {
				path: self.current.path.clone(),
			},
		)));
	}

	fn stage_unstage_hunk(&self) -> Result<()> {
		if self.current.is_stage {
			self.unstage_hunk()?;
		} else {
			self.stage_hunk()?;
		}

		Ok(())
	}

	fn calc_hunk_move_target(
		&self,
		direction: isize,
	) -> Option<usize> {
		let diff = self.diff.as_ref()?;
		if diff.hunks.is_empty() {
			return None;
		}
		let max = diff.hunks.len() - 1;
		let target_index = self.selected_hunk.map_or(0, |i| {
			let target = if direction >= 0 {
				i.saturating_add(direction.unsigned_abs())
			} else {
				i.saturating_sub(direction.unsigned_abs())
			};
			std::cmp::min(max, target)
		});
		Some(target_index)
	}

	fn diff_hunk_move_up_down(&mut self, direction: isize) {
		let Some(diff) = &self.diff else { return };
		let hunk_index = self.calc_hunk_move_target(direction);
		// return if selected_hunk not change
		if self.selected_hunk == hunk_index {
			return;
		}
		if let Some(hunk_index) = hunk_index {
			let line_index = diff
				.hunks
				.iter()
				.take(hunk_index)
				.fold(0, |sum, hunk| sum + hunk.lines.len());
			let hunk = &diff.hunks[hunk_index];
			self.selection = Selection::Single(line_index);
			self.selected_hunk = Some(hunk_index);
			self.vertical_scroll.move_area_to_visible(
				self.current_size.get().1 as usize,
				line_index,
				line_index.saturating_add(hunk.lines.len()),
			);
		}
	}

	const fn is_stage(&self) -> bool {
		self.current.is_stage
	}

	/// Renders the unified (single-column) diff — the default view and
	/// the only one that supports staging.
	fn draw_unified(
		&self,
		f: &mut Frame,
		r: Rect,
		block: Block<'_>,
		current_width: u16,
		current_height: u16,
	) {
		self.horizontal_scroll.update_no_selection(
			self.longest_line,
			current_width.into(),
		);

		let txt = if self.pending {
			vec![Line::from(vec![Span::styled(
				Cow::from(strings::loading_text(&self.key_config)),
				self.theme.text(false, false),
			)])]
		} else {
			self.get_text(r.width, current_height)
		};

		f.render_widget(Paragraph::new(txt).block(block), r);

		if self.focused() {
			self.vertical_scroll.draw(f, r, &self.theme);

			if self.max_scroll_right() > 0 {
				self.horizontal_scroll.draw(f, r, &self.theme);
			}
		}
	}

	/// Renders the side-by-side (split) diff: two view-only columns
	/// separated by a vertical divider. Falls back to a hint when the
	/// terminal is too narrow.
	fn draw_split(&self, f: &mut Frame, r: Rect, block: Block<'_>) {
		let inner = block.inner(r);

		if inner.width < Self::MIN_SPLIT_INNER_WIDTH {
			let hint = vec![Line::from(Span::styled(
				Cow::from("terminal too narrow for split view"),
				self.theme.text(false, false),
			))];
			f.render_widget(
				Paragraph::new(hint)
					.block(block)
					.alignment(Alignment::Center),
				r,
			);
			if self.focused() {
				self.vertical_scroll.draw(f, r, &self.theme);
			}
			return;
		}

		f.render_widget(block, r);

		let [left, divider, right] = Layout::horizontal([
			Constraint::Fill(1),
			Constraint::Length(1),
			Constraint::Fill(1),
		])
		.areas(inner);

		self.horizontal_scroll.update_no_selection(
			self.longest_split_line,
			usize::from(left.width),
		);

		let (l_lines, r_lines) = self.get_split_text(
			left.width,
			right.width,
			inner.height as usize,
		);

		f.render_widget(Paragraph::new(l_lines), left);
		f.render_widget(Paragraph::new(r_lines), right);

		let divider_lines = vec![
			Line::from(Span::styled(
				symbols::line::VERTICAL,
				self.theme.block(self.focused()),
			));
			inner.height as usize
		];
		f.render_widget(Paragraph::new(divider_lines), divider);

		if self.focused() {
			self.vertical_scroll.draw(f, r, &self.theme);

			if self.max_scroll_right_split(left.width) > 0 {
				self.horizontal_scroll.draw(f, r, &self.theme);
			}
		}
	}
}

impl DrawableComponent for DiffComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.poll_highlight();

		self.current_size.set((
			r.width.saturating_sub(2),
			r.height.saturating_sub(2),
		));

		let current_width = self.current_size.get().0;
		let current_height = self.current_size.get().1;

		self.vertical_scroll.update(
			self.selection.get_end(),
			self.row_count(),
			usize::from(current_height),
		);

		let title = format!(
			"{}{}",
			strings::title_diff(&self.key_config),
			self.current.path
		);

		let block = Block::default()
			.title(Span::styled(
				title,
				self.theme.title(self.focused()),
			))
			.borders(Borders::ALL)
			.border_style(self.theme.block(self.focused()));

		// split is opt-in and view-only; pending and binary diffs
		// always fall back to the unified renderer.
		let use_split = self.is_split()
			&& !self.pending
			&& self
				.diff
				.as_ref()
				.is_some_and(|d| !d.hunks.is_empty());

		if use_split {
			self.draw_split(f, r, block);
		} else {
			self.draw_unified(
				f,
				r,
				block,
				current_width,
				current_height,
			);
		}

		Ok(())
	}
}

impl Component for DiffComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			self.can_scroll(),
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_next(&self.key_config),
			self.calc_hunk_move_target(1) != self.selected_hunk,
			self.focused() && self.is_unified(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_prev(&self.key_config),
			self.calc_hunk_move_target(-1) != self.selected_hunk,
			self.focused() && self.is_unified(),
		));
		out.push(
			CommandInfo::new(
				strings::commands::diff_home_end(&self.key_config),
				self.can_scroll(),
				self.focused(),
			)
			.hidden(),
		);

		// editing is orthogonal to the view mode, so it stays available
		// in the split view; only staging is unified-only.
		if !self.is_immutable {
			out.push(CommandInfo::new(
				strings::commands::edit_item(&self.key_config),
				self.can_edit_file(),
				self.focused() && self.can_edit_file(),
			));
		}

		if !self.is_immutable && self.is_unified() {
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_remove(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_add(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_revert(&self.key_config),
				self.selected_hunk.is_some(),
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_revert(
					&self.key_config,
				),
				//TODO: only if any modifications are selected
				true,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_stage(&self.key_config),
				//TODO: only if any modifications are selected
				true,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_unstage(
					&self.key_config,
				),
				//TODO: only if any modifications are selected
				true,
				self.focused() && self.is_stage(),
			));
		}

		out.push(CommandInfo::new(
			strings::commands::copy(&self.key_config),
			true,
			self.focused(),
		));

		out.push(CommandInfo::new(
			strings::commands::diff_toggle_syntax(&self.key_config),
			true,
			self.focused(),
		));

		out.push(CommandInfo::new(
			strings::commands::diff_toggle_split(
				&self.key_config,
				self.is_split(),
			),
			true,
			self.focused(),
		));

		CommandBlocking::PassingOn
	}

	#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.focused() {
			if let Event::Key(e) = ev {
				return if key_match(e, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.shift_down,
				) {
					self.modify_selection(Direction::Down);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.shift_up)
				{
					self.modify_selection(Direction::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.end) {
					self.move_selection(ScrollType::End);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Right);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_left)
				{
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Left);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_next,
				) && self.is_unified()
				{
					self.diff_hunk_move_up_down(1);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_prev,
				) && self.is_unified()
				{
					self.diff_hunk_move_up_down(-1);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.edit_file)
					&& self.can_edit_file()
				{
					self.queue.push(
						InternalEvent::OpenExternalEditor(Some(
							self.current.path.clone(),
						)),
					);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.stage_unstage_item,
				) && !self.is_immutable
					&& self.is_unified()
				{
					try_or_popup!(
						self,
						"hunk error:",
						self.stage_unstage_hunk()
					);

					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_reset_item,
				) && !self.is_immutable
					&& !self.is_stage()
					&& self.is_unified()
				{
					if let Some(diff) = &self.diff {
						if diff.untracked {
							self.reset_untracked();
						} else {
							self.reset_hunk();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_stage_lines,
				) && !self.is_immutable
					&& self.is_unified()
				{
					self.stage_lines();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_reset_lines,
				) && !self.is_immutable
					&& !self.is_stage()
					&& self.is_unified()
				{
					if let Some(diff) = &self.diff {
						//TODO: reset untracked lines
						if !diff.untracked {
							self.reset_lines();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.copy) {
					self.copy_selection();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_toggle_syntax,
				) {
					self.options.borrow_mut().diff_cycle_highlight();
					let style =
						self.options.borrow().diff_highlight_style();
					if !style.is_on() {
						self.syntax_highlight.borrow_mut().clear();
						self.highlight_job.cancel();
					} else if self
						.syntax_highlight
						.borrow()
						.is_empty()
					{
						self.spawn_highlight();
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_toggle_split,
				) {
					self.options.borrow_mut().diff_toggle_view();
					self.clamp_selection_to_mode();
					Ok(EventState::Consumed)
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.focused
	}
	fn focus(&mut self, focus: bool) {
		self.focused = focus;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment, queue::InternalEvent, ui::style::Theme,
	};
	use asyncgit::sync::diff::Hunk;
	use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
	use std::io::Write;
	use std::rc::Rc;
	use tempfile::NamedTempFile;

	#[test]
	fn test_line_break() {
		let diff_line = DiffLine {
			content: "".into(),
			line_type: DiffLineType::Add,
			position: Default::default(),
		};

		{
			let default_theme = Rc::new(Theme::default());
			let mut env = Environment::test_env();
			env.theme = default_theme.clone();
			let diff = DiffComponent::new(&env, false);

			assert_eq!(
				diff.get_line_to_add(
					4,
					&diff_line,
					false,
					HunkLineFlags {
						selected_hunk: false,
						end_of_hunk: false,
					},
					0,
					None,
				)
				.spans
				.last()
				.unwrap(),
				&Span::styled(
					Cow::from("¶\n"),
					default_theme
						.diff_line(diff_line.line_type, false)
				)
			);
		}

		{
			let mut file = NamedTempFile::new().unwrap();

			writeln!(
				file,
				r#"
(
	line_break: Some("+")
)
"#
			)
			.unwrap();

			let theme =
				Rc::new(Theme::init(&file.path().to_path_buf()));
			let mut env = Environment::test_env();
			env.theme = theme.clone();
			let diff = DiffComponent::new(&env, false);

			assert_eq!(
				diff.get_line_to_add(
					4,
					&diff_line,
					false,
					HunkLineFlags {
						selected_hunk: false,
						end_of_hunk: false,
					},
					0,
					None,
				)
				.spans
				.last()
				.unwrap(),
				&Span::styled(
					Cow::from("+\n"),
					theme.diff_line(diff_line.line_type, false)
				)
			);
		}
	}

	#[test]
	fn diff_component_opens_editor_for_current_file() {
		let env = Environment::test_env();
		let mut diff = DiffComponent::new(&env, false);

		diff.focus(true);
		diff.current.path = String::from("src/main.rs");

		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('e'),
			KeyModifiers::empty(),
		));

		assert!(matches!(
			diff.event(&event).unwrap(),
			EventState::Consumed
		));

		let event = env.queue.pop();
		assert!(matches!(
			event,
			Some(InternalEvent::OpenExternalEditor(Some(path)))
				if path == "src/main.rs"
		));
	}

	fn diff_line(content: &str, t: DiffLineType) -> DiffLine {
		DiffLine {
			content: content.into(),
			line_type: t,
			position: DiffLinePosition::default(),
		}
	}

	/// Builds a single-hunk `FileDiff` from a list of line types; each
	/// line's content is `line{index}` so a copied flat index is easy
	/// to identify.
	fn file_diff(types: &[DiffLineType]) -> FileDiff {
		let lines: Vec<DiffLine> = types
			.iter()
			.enumerate()
			.map(|(i, &t)| diff_line(&format!("line{i}"), t))
			.collect();
		let count = lines.len();
		FileDiff {
			hunks: vec![Hunk {
				header_hash: 0,
				lines,
			}],
			lines: count,
			untracked: false,
			sizes: (0, 0),
			size_delta: 0,
		}
	}

	fn rows_of(types: &[DiffLineType]) -> Vec<SplitRow> {
		build_split_rows(&file_diff(types))
	}

	#[test]
	fn test_split_all_context() {
		use DiffLineType::None;
		let rows = rows_of(&[None, None, None]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: Some(0),
					right: Some(0)
				},
				SplitRow {
					left: Some(1),
					right: Some(1)
				},
				SplitRow {
					left: Some(2),
					right: Some(2)
				},
			]
		);
	}

	#[test]
	fn test_split_equal_del_then_add() {
		use DiffLineType::{Add, Delete};
		let rows = rows_of(&[Delete, Delete, Add, Add]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: Some(0),
					right: Some(2)
				},
				SplitRow {
					left: Some(1),
					right: Some(3)
				},
			]
		);
	}

	#[test]
	fn test_split_more_dels_than_adds() {
		use DiffLineType::{Add, Delete};
		let rows = rows_of(&[Delete, Delete, Delete, Add]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: Some(0),
					right: Some(3)
				},
				SplitRow {
					left: Some(1),
					right: None
				},
				SplitRow {
					left: Some(2),
					right: None
				},
			]
		);
	}

	#[test]
	fn test_split_more_adds_than_dels() {
		use DiffLineType::{Add, Delete};
		let rows = rows_of(&[Delete, Add, Add, Add]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: Some(0),
					right: Some(1)
				},
				SplitRow {
					left: None,
					right: Some(2)
				},
				SplitRow {
					left: None,
					right: Some(3)
				},
			]
		);
	}

	#[test]
	fn test_split_add_only() {
		use DiffLineType::Add;
		let rows = rows_of(&[Add, Add]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: None,
					right: Some(0)
				},
				SplitRow {
					left: None,
					right: Some(1)
				},
			]
		);
	}

	#[test]
	fn test_split_delete_only() {
		use DiffLineType::Delete;
		let rows = rows_of(&[Delete, Delete]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: Some(0),
					right: None
				},
				SplitRow {
					left: Some(1),
					right: None
				},
			]
		);
	}

	#[test]
	fn test_split_two_change_blocks_context_separated() {
		use DiffLineType::{Add, Delete, None};
		let rows = rows_of(&[Delete, Add, None, Delete, Add]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: Some(0),
					right: Some(1)
				},
				SplitRow {
					left: Some(2),
					right: Some(2)
				},
				SplitRow {
					left: Some(3),
					right: Some(4)
				},
			]
		);
	}

	#[test]
	fn test_split_add_then_delete_flushes() {
		// git never interleaves +/- within a block, but an Add->Delete
		// boundary must still start a fresh change block.
		use DiffLineType::{Add, Delete};
		let rows = rows_of(&[Add, Delete]);
		assert_eq!(
			rows,
			vec![
				SplitRow {
					left: None,
					right: Some(0)
				},
				SplitRow {
					left: Some(1),
					right: None
				},
			]
		);
	}

	#[test]
	fn test_split_header_single_row() {
		use DiffLineType::Header;
		let rows = rows_of(&[Header]);
		assert_eq!(
			rows,
			vec![SplitRow {
				left: Some(0),
				right: Some(0)
			}]
		);
	}

	#[test]
	fn test_split_flat_index_coverage() {
		use DiffLineType::{Add, Delete, Header, None};
		let types =
			[Header, None, Delete, Delete, Add, None, Add, Delete];
		let diff = file_diff(&types);
		let rows = build_split_rows(&diff);

		let mut left: Vec<usize> =
			rows.iter().filter_map(|r| r.left).collect();
		let mut right: Vec<usize> =
			rows.iter().filter_map(|r| r.right).collect();

		for (idx, t) in types.iter().enumerate() {
			match t {
				Header | None => {
					// context/header span both columns
					assert!(
						left.contains(&idx),
						"left missing {idx}"
					);
					assert!(
						right.contains(&idx),
						"right missing {idx}"
					);
				}
				Delete => {
					assert!(
						left.contains(&idx),
						"left missing {idx}"
					);
					assert!(
						!right.contains(&idx),
						"delete {idx} leaked to right"
					);
				}
				Add => {
					assert!(
						right.contains(&idx),
						"right missing {idx}"
					);
					assert!(
						!left.contains(&idx),
						"add {idx} leaked to left"
					);
				}
			}
		}

		// every flat line index is covered somewhere
		for idx in 0..diff.lines {
			assert!(
				left.contains(&idx) || right.contains(&idx),
				"flat index {idx} not covered"
			);
		}

		// no flat index is emitted twice on the same side
		let l_before = left.len();
		left.sort_unstable();
		left.dedup();
		assert_eq!(l_before, left.len(), "duplicate left index");
		let r_before = right.len();
		right.sort_unstable();
		right.dedup();
		assert_eq!(r_before, right.len(), "duplicate right index");
	}

	#[test]
	fn test_split_draw_does_not_panic() {
		let backend = ratatui::backend::TestBackend::new(100, 40);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let mut frame = terminal.get_frame();

		let env = Environment::test_env();
		{
			let mut opts = env.options.borrow_mut();
			opts.diff_toggle_view(); // Unified -> Split
			opts.diff_cycle_highlight(); // Tint -> Off (no async job)
		}
		assert!(env.options.borrow().diff_view().is_split());

		let mut diff = DiffComponent::new(&env, true);
		diff.focus(true);
		diff.update(
			"file.rs".to_string(),
			false,
			file_diff(&[
				DiffLineType::Header,
				DiffLineType::None,
				DiffLineType::Delete,
				DiffLineType::Add,
				DiffLineType::None,
			]),
		);

		// wide enough: renders the two columns + divider
		diff.draw(&mut frame, Rect::new(0, 0, 100, 40)).unwrap();
		// too narrow: falls back to the hint — must also not panic
		diff.draw(&mut frame, Rect::new(0, 0, 20, 40)).unwrap();
	}

	#[test]
	fn test_split_copy_prefers_new_side() {
		use DiffLineType::{Add, Delete, None};
		// change row, context, pure-delete row, context, pure-add row
		let types = [Delete, Add, None, Delete, None, Add];
		let rows = rows_of(&types);

		// copy_selection picks row.right.or(row.left) per selected row
		let preferred: Vec<usize> =
			rows.iter().filter_map(|r| r.right.or(r.left)).collect();

		// new side (Add=1) for the change row, context (2), old side
		// (Delete=3) for the pure deletion, context (4), new side
		// (Add=5) for the pure addition.
		assert_eq!(preferred, vec![1, 2, 3, 4, 5]);
	}
}
